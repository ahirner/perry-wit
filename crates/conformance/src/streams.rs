//! Pure stream inputs and observation assertions. Transports own the host effects.
use crate::{
    execution::{Evidence, Outcome, check_equivalence, number},
    registry::{Case, Contract},
};
use anyhow::{Result, ensure};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamCase {
    pub(crate) bytes: Vec<u8>,
    pub(crate) cancel: bool,
    #[serde(default)]
    pub(crate) byob: bool,
}
impl StreamCase {
    pub(crate) fn source(&self) -> String {
        let cancel = self.cancel;
        let (mode, setup, read_loop, unlock_err, eof_err) = if self.byob {
            (
                r#"{mode:"byob"}"#,
                "let buffer=new Uint8Array(8196);buffer[0]=19;buffer[8195]=23;",
                format!(
                    r#"while(true){{
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
    }}"#
                ),
                7,
                8,
            )
        } else {
            (
                "",
                "",
                format!(
                    r#"let result=await reader.read();
    while(!result.done){{
      const bytes=result.value;if(bytes===undefined)throw 3;
      const pending=writer.write(bytes);
      await pending;await pending;
      count+=bytes.length;
      if({cancel}){{await reader.cancel();trace.push(2);break;}}
      result=await reader.read();
    }}"#
                ),
                4,
                5,
            )
        };
        format!(
            r#"
import {{Writable}} from 'node:stream';
export async function run(input:ReadableStream<Uint8Array>):Promise<{{value:number,trace:number[]}}> {{
  const trace:number[]=[];
  const reader=input.getReader({mode});
  if(!input.locked)throw 1;
  let locked=false;try{{input.getReader();}}catch{{locked=true;}}
  if(!locked)throw 2;trace.push(1);
  const writer=Writable.toWeb(process.stdout).getWriter();
  {setup}let count=0;
  try {{
    {read_loop}
    writer.releaseLock();
    reader.releaseLock();
    if(input.locked)throw {unlock_err};
    const finished=input.getReader();
    if(!(await finished.read()).done)throw {eof_err};
    finished.releaseLock();trace.push(3);
    return {{value:count,trace}};
  }} finally {{if(input.locked){{await reader.cancel();reader.releaseLock();}}}}
}}
"#
        )
    }
    pub(crate) fn expected_bytes(&self) -> &[u8] {
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
fn stream_contract(id: &'static str, byob: bool) -> Contract {
    let case = |length, cancel| {
        Case::Stream(StreamCase {
            bytes: (0..length).map(|i| (i * 37 + i / 251) as u8).collect(),
            cancel,
            byob,
        })
    };
    Contract::checked(
        id,
        [
            ("empty", case(0, false)),
            ("byte-boundary", case(257, false)),
            ("transfer-boundary", case(8193, false)),
            ("beyond-guest-memory", case(4 * 1024 * 1024, false)),
            ("cancel-and-reuse", case(16385, true)),
        ],
        (prop::collection::vec(any::<u8>(), 0..=16385), any::<bool>())
            .prop_map(move |(bytes, cancel)| {
                Case::Stream(StreamCase {
                    bytes,
                    cancel,
                    byob,
                })
            })
            .boxed(),
        check,
    )
}

pub(crate) fn contract() -> Contract {
    stream_contract("wasi.streams.incoming_bytes", false)
}

pub(crate) fn byob_contract() -> Contract {
    stream_contract("web.streams.byob_reader", true)
}
