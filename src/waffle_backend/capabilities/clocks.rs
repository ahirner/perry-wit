//! Millisecond source durations mapped to the P3 monotonic clock.

use super::{CapabilityPlan, LowerCapability};
use perry_hir::types::Type as HirType;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ClockOperation {
    WaitFor,
}

impl LowerCapability for ClockOperation {
    fn lower(&self) -> CapabilityPlan {
        match self {
            Self::WaitFor => CapabilityPlan {
                params: vec![HirType::Number],
                result: HirType::Promise(Box::new(HirType::Void)),
                adapter: P3_CLOCK_ADAPTER,
                core_function: "(func $delay \"wait-for\")",
            },
        }
    }
}

const P3_CLOCK_ADAPTER: &str = r#"
  (import "wasi:clocks/monotonic-clock@0.3.0" (instance $clock
    (export "wait-for" (func async (param "how-long" u64)))))
  (alias export $clock "wait-for" (func $wait-for))
  (core func $wait-for (canon lower (func $wait-for)))
  (core module $delay
    (import "wasi" "wait-for" (func $wait-for (param i64)))
    (func (export "wait-for") (param $milliseconds f64)
      (if (f64.lt (local.get $milliseconds) (f64.const 0)) (then unreachable))
      (call $wait-for (i64.trunc_f64_u
        (f64.mul (local.get $milliseconds) (f64.const 1000000))))))
  (core instance $delay (instantiate $delay
    (with "wasi" (instance (export "wait-for" (func $wait-for))))))
"#;
