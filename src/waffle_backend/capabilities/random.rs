//! Uniform double samples using the high 53 bits of a WASI random word.

use super::{CapabilityImplementation, CapabilityPlan, LowerCapability};
use perry_hir::types::Type as HirType;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RandomOperation {
    Number,
}

impl LowerCapability for RandomOperation {
    fn lower(&self) -> CapabilityPlan {
        match self {
            Self::Number => CapabilityPlan {
                params: vec![],
                result: HirType::Number,
                implementation: CapabilityImplementation::Standalone {
                    adapter: P3_RANDOM_ADAPTER,
                    core_function: "(func $random-number \"sample\")",
                },
            },
        }
    }
}

const P3_RANDOM_ADAPTER: &str = r#"
  (import "wasi:random/random@0.3.0" (instance $random
    (export "get-random-u64" (func (result u64)))))
  (alias export $random "get-random-u64" (func $random-word))
  (core func $random-word (canon lower (func $random-word)))
  (core module $random-number
    (import "wasi" "random-word" (func $random-word (result i64)))
    (func (export "sample") (result f64)
      (f64.div
        (f64.convert_i64_u (i64.shr_u (call $random-word) (i64.const 11)))
        (f64.const 9007199254740992))))
  (core instance $random-number (instantiate $random-number
    (with "wasi" (instance (export "random-word" (func $random-word))))))
"#;
