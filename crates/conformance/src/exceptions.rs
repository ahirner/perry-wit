//! Exception control-flow contracts independent of JSON, codecs and promise combinators.
use crate::{
    execution::check_equivalence,
    registry::{Case, Contract, Witness},
};
use proptest::prelude::*;

fn rejection_case(message: String, rounds: u32) -> Case {
    Case::Function {
        asynchronous: true,
        source: format!(
            r#"
import {{ setTimeout }} from 'node:timers/promises';
async function fail(error: Error): Promise<number> {{ await setTimeout(1); throw error; }}
export async function run(x: number): Promise<{{value: number, trace: number[]}}> {{
  const message = {message};
  const original = new TypeError(message);
  const pending = fail(original);
  const trace: number[] = [];
  for (let i = 0; i < {rounds}; i++) {{
    try {{ await pending; trace.push(99); }}
    catch (e) {{
      if (e !== original) throw 90;
      if (!(e instanceof TypeError)) throw 91;
      if (e.message !== message) throw 92;
      trace.push(i);
    }} finally {{ await setTimeout(1); trace.push(-i); }}
  }}
  try {{
    try {{ throw original; }} finally {{
      try {{ throw new Error("temporary"); }} catch {{ trace.push(50); }}
    }}
  }} catch (e) {{ if (e !== original) throw 93; trace.push(60); }}
  try {{ throw x; }} catch (e) {{ trace.push(e === x ? 1 : 0); }}
  return {{ value: x, trace }};
}}
"#,
            message = serde_json::to_string(&message).unwrap()
        ),
    }
}

const COMPLETIONS: &str = r#"
function choose(x: number): number {
  try {
    if (x === 0) return 1;
    if (x === 1) throw new Error("first");
    return 3;
  } finally {
    if (x === 0) return 4;
    if (x === 1) throw new RangeError("last");
  }
}
export function run(x: number): {value: number, trace: number[]} {
  const trace: number[] = [];
  let value = 0;
  try { value = choose(x); }
  catch (e) { if (!(e instanceof RangeError)) throw e; value = 9; }
  for (let i = 0; i < 4; i++) {
    try {
      if (i === 0) continue;
      if (i === 2) break;
      trace.push(i);
    } finally { trace.push(10 + i); }
  }
  for (let i = 0; i < 3; i++) {
    try { throw new Error("overridden"); }
    finally { if (i === 0) continue; break; }
  }
  return { value, trace };
}
"#;

pub(crate) fn contract() -> Contract {
    Contract {
        id: "ecma.error.control_flow",
        description: "Caught values and completion through nested finalizers and suspended awaits",
        specification: "https://tc39.es/ecma262/#sec-try-statement",
        domain: "Error identity and string messages, primitive throws, return/throw/break/continue finalizers; 1..3 repeated waits on one rejected promise, UTF-8 scalar messages of 0..16 characters",
        witnesses: vec![
            Witness {
                partition: "completion-overrides",
                case: Case::Function {
                    source: COMPLETIONS.into(),
                    asynchronous: false,
                },
            },
            Witness {
                partition: "empty-message-repeated-await",
                case: rejection_case(String::new(), 3),
            },
            Witness {
                partition: "unicode-and-nul-message",
                case: rejection_case("é雪🙂\0".into(), 2),
            },
        ],
        strategy: (prop::collection::vec(any::<char>(), 0..=16), 1u32..=3)
            .prop_map(|(chars, rounds)| rejection_case(chars.into_iter().collect(), rounds))
            .boxed(),
        check: check_equivalence,
    }
}
