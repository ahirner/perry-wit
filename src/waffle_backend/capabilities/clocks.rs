//! Source clock units and selective WASI P3 clock imports.

use super::{
    CapabilityImplementation, CapabilityPlan, LowerCapability,
    scalars::{DateStorage, Scalar},
};
use anyhow::Result;
use perry_hir::types::Type as HirType;
use std::{collections::BTreeSet, fmt::Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ClockOperation {
    WaitFor,
    Timeout,
    MonotonicNow,
    DateNow,
}

impl ClockOperation {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::WaitFor => "waitFor",
            Self::Timeout => "setTimeout",
            Self::MonotonicNow => "performance.now",
            Self::DateNow => "Date.now",
        }
    }
}

impl LowerCapability for ClockOperation {
    fn lower(&self) -> CapabilityPlan {
        CapabilityPlan {
            params: if matches!(self, Self::WaitFor | Self::Timeout) {
                vec![HirType::Number]
            } else {
                vec![]
            },
            result: if matches!(self, Self::WaitFor | Self::Timeout) {
                HirType::Promise(Box::new(HirType::Void))
            } else {
                HirType::Number
            },
            implementation: CapabilityImplementation::Standalone {
                core_function: match self {
                    Self::WaitFor => "(func $clocks \"waitFor\")",
                    Self::Timeout => "(func $clocks \"setTimeout\")",
                    Self::MonotonicNow => "(func $clocks \"performance.now\")",
                    Self::DateNow => "(func $clocks \"Date.now\")",
                },
            },
        }
    }
}

pub(crate) fn declare_adapters(operations: &BTreeSet<ClockOperation>) -> Result<String> {
    if operations.is_empty() {
        return Ok(String::new());
    }
    let timeout = operations.contains(&ClockOperation::Timeout);
    let mut operations = operations.clone();
    if timeout {
        operations.insert(ClockOperation::WaitFor);
    }
    let mut wat = String::new();
    let mut imports = String::new();
    let mut exports = String::new();
    let mut bodies = String::new();
    if timeout {
        bodies.push_str(&Scalar::Timeout.body("setTimeout", "wait-for"));
    }
    if operations.contains(&ClockOperation::WaitFor)
        || operations.contains(&ClockOperation::Timeout)
        || operations.contains(&ClockOperation::MonotonicNow)
    {
        wat.push_str("(import \"wasi:clocks/monotonic-clock@0.3.0\" (instance $clock");
        for (operation, name, signature, core_signature, scalar) in [
            (
                ClockOperation::WaitFor,
                "wait-for",
                "(param \"how-long\" u64)",
                "(param i64)",
                Scalar::Wait,
            ),
            (
                ClockOperation::MonotonicNow,
                "now",
                "(result u64)",
                "(result i64)",
                Scalar::Monotonic,
            ),
        ] {
            if !operations.contains(&operation) {
                continue;
            }
            write!(
                wat,
                "(export {name:?} (func {} {signature}))",
                if matches!(operation, ClockOperation::WaitFor | ClockOperation::Timeout) {
                    "async"
                } else {
                    ""
                }
            )?;
            writeln!(
                imports,
                "(import \"native\" {name:?} (func ${name} {core_signature}))"
            )?;
            writeln!(exports, "(export {name:?} (func $clock-{name}))")?;
            bodies.push_str(&scalar.body(operation.name(), name));
        }
        wat.push_str("))\n");
        for (operation, name) in [
            (ClockOperation::WaitFor, "wait-for"),
            (ClockOperation::MonotonicNow, "now"),
        ] {
            if operations.contains(&operation) {
                writeln!(
                    wat,
                    "(core func $clock-{name} (canon lower (func $clock {name:?})))"
                )?;
            }
        }
    }
    if operations.contains(&ClockOperation::DateNow) {
        wat.push_str(r#"
          (import "wasi:clocks/system-clock@0.3.0" (instance $system-clock
            (type $instant-base (record (field "seconds" s64) (field "nanoseconds" u32)))
            (export "instant" (type $instant (eq $instant-base)))
            (export "now" (func (result $instant)))))
          (core module $clock-storage (memory (export "memory") 1))
          (core instance $clock-storage (instantiate $clock-storage))
          (core func $clock-system-now (canon lower (func $system-clock "now") (memory (core memory $clock-storage "memory"))))
        "#);
        imports.push_str(
            r#"(import "native" "system-now" (func $system-now (param i32)))
          (import "native" "memory" (memory 1))"#,
        );
        exports.push_str(r#"(export "system-now" (func $clock-system-now)) (export "memory" (memory $clock-storage "memory"))"#);
        bodies.push_str(&Scalar::Date(DateStorage::Standalone).body("Date.now", "system-now"));
    }
    write!(
        wat,
        "(core module $clocks {imports} {bodies}) (core instance $clocks (instantiate $clocks (with \"native\" (instance {exports}))))"
    )?;
    Ok(wat)
}
