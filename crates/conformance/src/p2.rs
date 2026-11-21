//! P2 source semantics are compared with Node across the usual numeric boundary corpus.
use crate::{
    execution::check_equivalence,
    program::WIT,
    registry::{Case, Contract, Witness},
};
use proptest::prelude::*;

fn case(source: String) -> Case {
    Case::P2 {
        source,
        wit: WIT.replace(
            "world generated {",
            "world generated { include wasi:cli/imports@0.2.12;",
        ),
        diagnostic: None,
    }
}

fn timers(delay: u32) -> Case {
    case(format!(
        r#"
import {{setTimeout}} from 'node:timers/promises';
async function later(value: number, delay: number): Promise<number> {{ await setTimeout(delay); return value; }}
export async function run(x: number): Promise<{{value:number,trace:number[]}}> {{
  const values = await Promise.all([later(11, {delay}), later(29, 0)]);
  return {{value:x, trace:[values[0], values[1]]}};
}}
"#
    ))
}

pub(crate) fn contracts() -> Vec<Contract> {
    let fs = case(
        r#"
import {readFile,writeFile,stat,readdir,mkdir,unlink,rmdir} from 'node:fs/promises';
export async function run(x:number):Promise<{value:number,trace:number[]}> {
  await mkdir('roundtrip');
  await writeFile('roundtrip/data','héllo');
  const text=await readFile('roundtrip/data','utf8');
  const size=(await stat('roundtrip/data')).size;
  const count=(await readdir('roundtrip')).length;
  await unlink('roundtrip/data'); await rmdir('roundtrip');
  let missing=0; try { await readFile('missing','utf8'); }
  catch(e) { if(e instanceof Error && e.message.startsWith('ENOENT:')) missing=1; }
  return {value:x,trace:[text.length,size,count,missing]};
}
"#
        .into(),
    );
    let mixed = Case::P2 {
        source: "import {setTimeout} from 'node:timers/promises'; export async function run(x:number):Promise<{value:number,trace:number[]}>{await setTimeout(1);return {value:x,trace:[]};}"
            .into(),
        wit: WIT
            .replace(
                "world generated {",
                "world generated { include wasi:cli/imports@0.2.12;",
            )
            .replace("export run: func", "export run: async func"),
        diagnostic: Some("setTimeout cannot wait on wasi:clocks/monotonic-clock@0.2.12".into()),
    };
    let missing_poll = Case::P2 {
        source: "export async function run(x:number):Promise<{value:number,trace:number[]}>{return {value:x,trace:[]};}"
            .into(),
        wit: WIT.into(),
        diagnostic: Some("async TypeScript in a P2 world requires wasi:io/poll".into()),
    };
    vec![
        crate::fetch::p2_contract(),
        Contract::new(
            "wasi.p2.promises",
            vec![
                Witness {
                    partition: "ready",
                    case: timers(0),
                },
                Witness {
                    partition: "overlapping",
                    case: timers(3),
                },
            ],
            (0u32..=5).prop_map(timers).boxed(),
            check_equivalence,
        ),
        Contract::new(
            "wasi.p2.filesystem",
            vec![Witness {
                partition: "roundtrip-and-error",
                case: fs.clone(),
            }],
            Just(fs).boxed(),
            check_equivalence,
        ),
        Contract::new(
            "wasi.p2.reject_mixed_wait",
            vec![Witness {
                partition: "p2-timer-p3-export",
                case: mixed.clone(),
            }],
            Just(mixed).boxed(),
            check_equivalence,
        ),
        Contract::new(
            "wasi.p2.require_poll",
            vec![Witness {
                partition: "async-source-without-poll",
                case: missing_poll.clone(),
            }],
            Just(missing_poll).boxed(),
            check_equivalence,
        ),
    ]
}
