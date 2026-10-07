//! Pure bounded Fetch scenarios, shared by the Node and component transports.
use crate::{
    execution::{Evidence, Outcome, check_equivalence, number},
    registry::{Case, Contract, Witness},
};
use anyhow::{Result, ensure};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchCase {
    pub bytes: Vec<u8>,
    pub status: u16,
    pub limit: usize,
    pub truncated: bool,
    #[serde(default)]
    pub byob: bool,
}
impl FetchCase {
    pub fn source(&self) -> String {
        format!(
            r#"{reader}

export async function run(url:string):Promise<{{value:number,trace:number[]}}> {{
  const trace:number[]=[];
  const pending=fetch(url,{{headers:{{'x-contract':'bounded'}}}});
  const response=await pending;
  if(response!==await pending)throw 90;
  trace.push(response.status);
  try {{
    const bytes=await readBounded(response,{limit});
    for(let i=0;i<bytes.length;i++){{const byte=bytes[i];if(byte===undefined)throw 92;trace.push(byte);}}
    if(!response.bodyUsed)throw 91;
    return {{value:bytes.length,trace}};
  }}catch{{return {{value:-1,trace}};}}
}}
"#,
            reader = if self.byob {
                include_str!("../../../tests/fixtures/bounded_byob_response.ts")
            } else {
                include_str!("../../../tests/fixtures/bounded_response.ts")
            },
            limit = self.limit
        )
    }
}
fn check(case: &Case, evidence: &Evidence) -> Result<()> {
    check_equivalence(case, evidence)?;
    let Case::Fetch(case) = case else {
        anyhow::bail!("Fetch checker requires Fetch input")
    };
    let Outcome::Values { observations, .. } = &evidence.component else {
        anyhow::bail!("missing Fetch observations")
    };
    let failed = case.truncated || case.bytes.len() > case.limit;
    let mut trace = vec![number(f64::from(case.status))];
    if !failed {
        trace.extend(case.bytes.iter().map(|b| number(f64::from(*b))));
    }
    let value = number(if failed { -1. } else { case.bytes.len() as f64 });
    ensure!(
        observations
            .iter()
            .all(|o| o.value == value && o.trace == trace),
        "Fetch differs from the bounded-consumer model"
    );
    Ok(())
}
pub fn contract() -> Contract {
    let case = |bytes, status, limit, truncated| {
        Case::Fetch(FetchCase {
            bytes,
            status,
            limit,
            truncated,
            byob: false,
        })
    };
    Contract {
        id: "web.fetch.bounded_body",
        description: "Headers-first Fetch, bounded byte consumption, completion failure and reuse",
        specification: "https://fetch.spec.whatwg.org/",
        domain: "GET loopback HTTP; 200 and 404; binary bodies, explicit body cap and producer truncation; three calls per instance",
        witnesses: vec![
            Witness {
                partition: "empty",
                case: case(vec![], 200, 0, false),
            },
            Witness {
                partition: "binary-exact-limit",
                case: case(vec![0, 255, 128, 240, 159, 146], 200, 6, false),
            },
            Witness {
                partition: "non-success-status",
                case: case(b"missing".to_vec(), 404, 7, false),
            },
            Witness {
                partition: "overflow-cancellation",
                case: case(vec![7; 257], 200, 256, false),
            },
            Witness {
                partition: "completion-error",
                case: case(vec![17; 30], 200, 64, true),
            },
        ],
        strategy: (
            prop::collection::vec(any::<u8>(), 0..=256),
            prop::sample::select(vec![200u16, 404]),
            0usize..=256,
            any::<bool>(),
        )
            .prop_map(move |(bytes, status, limit, truncated)| {
                case(
                    bytes,
                    if truncated { 200 } else { status },
                    limit,
                    truncated,
                )
            })
            .boxed(),
        check,
    }
}

pub fn byob_contract() -> Contract {
    let mut contract = contract();
    contract.id = "web.fetch.bounded_byob_body";
    contract.description = "BYOB bounded Fetch consumes directly into its final buffer";
    for witness in &mut contract.witnesses {
        if let Case::Fetch(case) = &mut witness.case {
            case.byob = true;
        }
    }
    contract.strategy = contract
        .strategy
        .prop_map(|mut case| {
            if let Case::Fetch(fetch) = &mut case {
                fetch.byob = true;
            }
            case
        })
        .boxed();
    contract
}
