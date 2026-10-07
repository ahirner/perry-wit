//! Pure stream inputs and observation assertions; transports own the effects.
use crate::{
    execution::{Evidence, Outcome, check_equivalence, number},
    registry::{Case, Contract, Witness},
};
use anyhow::{Result, ensure};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamCase {
    pub bytes: Vec<u8>,
    pub cancel: bool,
    #[serde(default)]
    pub byob: bool,
}
impl StreamCase {
    pub fn source(&self) -> String {
        if self.byob {
            return self.byob_source();
        }
        format!(
            r#"
import {{Writable}} from 'node:stream';
export async function run(input:ReadableStream<Uint8Array>):Promise<{{value:number,trace:number[]}}> {{
  const trace:number[]=[];
  const reader=input.getReader();
  if(!input.locked)throw 1;
  let locked=false;try{{input.getReader();}}catch{{locked=true;}}
  if(!locked)throw 2;trace.push(1);
  const writer=Writable.toWeb(process.stdout).getWriter();
  let count=0;
  try {{
    let result=await reader.read();
    while(!result.done){{
      const bytes=result.value;if(bytes===undefined)throw 3;
      const pending=writer.write(bytes);
      await pending;await pending;
      count+=bytes.length;
      if({cancel}){{await reader.cancel();trace.push(2);break;}}
      result=await reader.read();
    }}
    writer.releaseLock();
    reader.releaseLock();
    if(input.locked)throw 4;
    const finished=input.getReader();
    if(!(await finished.read()).done)throw 5;
    finished.releaseLock();trace.push(3);
    return {{value:count,trace}};
  }} finally {{if(input.locked){{await reader.cancel();reader.releaseLock();}}}}
}}
"#,
            cancel = self.cancel
        )
    }
    fn byob_source(&self) -> String {
        format!(
            r#"
import {{Writable}} from 'node:stream';
export async function run(input:ReadableStream<Uint8Array>):Promise<{{value:number,trace:number[]}}> {{
  const trace:number[]=[];const reader=input.getReader({{mode:"byob"}});
  if(!input.locked)throw 1;
  let locked=false;try{{input.getReader();}}catch{{locked=true;}}
  if(!locked)throw 2;trace.push(1);
  const writer=Writable.toWeb(process.stdout).getWriter();
  let buffer=new Uint8Array(8196);buffer[0]=19;buffer[8195]=23;let count=0;
  try {{
    while(true){{
      const original=buffer.buffer;const alias=buffer.subarray(1,2);
      const pending=reader.read(buffer.subarray(2,8194),{{min:8192}});
      if(buffer.length!==0 || alias.length!==0 || original.byteLength!==0)throw 3;
      const result=await pending;const bytes=result.value;if(bytes===undefined)throw 4;
      if(bytes.byteOffset!==2 || bytes.buffer.byteLength!==8196)throw 5;
      buffer=new Uint8Array(bytes.buffer);
      if(buffer[0]!==19 || buffer[8195]!==23)throw 6;
      if(bytes.length>0){{
        const write=writer.write(bytes);await write;await write;count+=bytes.length;
        if({cancel}){{await reader.cancel();trace.push(2);break;}}
      }}
      if(result.done)break;
    }}
    writer.releaseLock();reader.releaseLock();if(input.locked)throw 7;
    const finished=input.getReader();if(!(await finished.read()).done)throw 8;
    finished.releaseLock();trace.push(3);return {{value:count,trace}};
  }}finally{{if(input.locked){{await reader.cancel();reader.releaseLock();}}}}
}}
"#,
            cancel = self.cancel
        )
    }
    pub fn expected_bytes(&self) -> &[u8] {
        if self.cancel {
            &self.bytes[..self.bytes.len().min(8192)]
        } else {
            &self.bytes
        }
    }
}
fn check(case: &Case, evidence: &Evidence) -> Result<()> {
    check_equivalence(case, evidence)?;
    let Case::Stream(case) = case else {
        anyhow::bail!("stream checker requires a stream case")
    };
    let Outcome::Values {
        observations,
        stdout,
        stderr,
        ..
    } = &evidence.component
    else {
        anyhow::bail!("missing stream observations")
    };
    ensure!(stderr.is_empty(), "unexpected stream stderr");
    ensure!(
        *stdout == case.expected_bytes().repeat(3),
        "stream lost or repeated bytes"
    );
    let trace = if case.cancel && !case.bytes.is_empty() {
        vec![number(1.), number(2.), number(3.)]
    } else {
        vec![number(1.), number(3.)]
    };
    ensure!(
        observations
            .iter()
            .all(|v| v.value == number(case.expected_bytes().len() as f64) && v.trace == trace),
        "stream lifecycle differs from model"
    );
    Ok(())
}
pub fn contract() -> Contract {
    let case = |length, cancel| {
        Case::Stream(StreamCase {
            bytes: (0..length).map(|i| (i * 37 + i / 251) as u8).collect(),
            cancel,
            byob: false,
        })
    };
    Contract {
        id: "wasi.streams.incoming_bytes",
        description: "Owned WIT byte streams use Web readers and acknowledged Web writes",
        specification: "https://streams.spec.whatwg.org/",
        domain: "Three calls per instance; input chunks of 8192 bytes; 64 KiB guest memory; exact output bytes, locking, cancellation and completion",
        witnesses: vec![
            Witness {
                partition: "empty",
                case: case(0, false),
            },
            Witness {
                partition: "byte-boundary",
                case: case(257, false),
            },
            Witness {
                partition: "transfer-boundary",
                case: case(8193, false),
            },
            Witness {
                partition: "beyond-guest-memory",
                case: case(4 * 1024 * 1024, false),
            },
            Witness {
                partition: "cancel-and-reuse",
                case: case(16385, true),
            },
        ],
        strategy: (prop::collection::vec(any::<u8>(), 0..=16385), any::<bool>())
            .prop_map(|(bytes, cancel)| {
                Case::Stream(StreamCase {
                    bytes,
                    cancel,
                    byob: false,
                })
            })
            .boxed(),
        check,
    }
}

pub fn byob_contract() -> Contract {
    let mut contract = contract();
    contract.id = "web.streams.byob_reader";
    contract.description = "BYOB readers transfer shared buffers and fill visible views directly";
    contract.domain = "Uint8Array destinations with offsets, alias detachment, buffer reuse, min:8192, short EOF, locking and cancellation; three calls per instance; exact output bytes within 64 KiB guest memory";
    for witness in &mut contract.witnesses {
        if let Case::Stream(case) = &mut witness.case {
            case.byob = true;
        }
    }
    contract.strategy = (prop::collection::vec(any::<u8>(), 0..=16385), any::<bool>())
        .prop_map(|(bytes, cancel)| {
            Case::Stream(StreamCase {
                bytes,
                cancel,
                byob: true,
            })
        })
        .boxed();
    contract
}
