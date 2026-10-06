//! The native HTTP service remains a WASI contract with WIT-owned declarations.
use crate::{
    execution::{Evidence, Outcome, check_equivalence, number},
    registry::{Case, Contract, Witness},
};
use anyhow::{Result, ensure};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandlerCase {
    pub bytes: Vec<u8>,
    pub status: u16,
}
impl HandlerCase {
    pub fn source(&self) -> String {
        format!(
            r#"
import type {{Request,Response}} from 'perry:http-handler/types';
export function handle(request:Request):Response {{
  if(request.method.tag!=='put'||request.pathWithQuery!=='/contract?q=1')throw 1;
  const scheme=request.scheme;
  if(request.authority!=='example.test'||scheme===undefined||scheme===null)throw 2;
  if(scheme.tag!=='https')throw 3;
  return {{status:{status},headers:request.headers,body:request.body}};
}}
"#,
            status = self.status
        )
    }
}
fn check(case: &Case, evidence: &Evidence) -> Result<()> {
    check_equivalence(case, evidence)?;
    let Case::Handler(case) = case else {
        anyhow::bail!("handler checker requires native HTTP input")
    };
    let Outcome::Values { observations, .. } = &evidence.component else {
        anyhow::bail!("missing handler observations")
    };
    let trace = case
        .bytes
        .iter()
        .map(|b| number(f64::from(*b)))
        .collect::<Vec<_>>();
    ensure!(
        observations
            .iter()
            .all(|o| o.value == number(f64::from(case.status)) && o.trace == trace),
        "native HTTP response differs from model"
    );
    Ok(())
}
pub fn contract() -> Contract {
    let case = |bytes, status| Case::Handler(HandlerCase { bytes, status });
    Contract {
        id: "wasi.http.handler",
        description: "Native HTTP ABI with WIT-generated request and response declarations",
        specification: "https://github.com/WebAssembly/wasi-http",
        domain: "PUT requests with binary bodies up to 64 KiB, duplicate headers, statuses 200/201/404; three exchanges reuse one component instance",
        witnesses: vec![
            Witness {
                partition: "empty",
                case: case(vec![], 200),
            },
            Witness {
                partition: "binary-and-duplicates",
                case: case(vec![0, 255, 128], 201),
            },
            Witness {
                partition: "request-response-limit",
                case: case((0..65536).map(|i| i as u8).collect(), 404),
            },
        ],
        strategy: (
            prop::collection::vec(any::<u8>(), 0..=256),
            prop::sample::select(vec![200u16, 201, 404]),
        )
            .prop_map(move |(bytes, status)| case(bytes, status))
            .boxed(),
        check,
    }
}
