//! Scalar capability adapters preserve source units around canonical WIT calls.

use super::super::{
    allocation::AllocationFuncs,
    capabilities::{CapabilityOperation, ClockOperation, RandomOperation},
    component::forward,
    resolve::{ResolvedContract, TypedIntrinsic},
    runtime,
};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{Func, Memory, Module};

#[derive(Clone, Copy)]
pub(crate) enum Scalar {
    Wait,
    Monotonic,
    Date(DateStorage),
    Random,
}
impl Scalar {
    fn of(intrinsic: &TypedIntrinsic) -> Option<Self> {
        match intrinsic {
            TypedIntrinsic::Capability(CapabilityOperation::Clock(ClockOperation::WaitFor)) => {
                Some(Self::Wait)
            }
            TypedIntrinsic::Capability(CapabilityOperation::Clock(
                ClockOperation::MonotonicNow,
            )) => Some(Self::Monotonic),
            TypedIntrinsic::Capability(CapabilityOperation::Clock(ClockOperation::DateNow)) => {
                Some(Self::Date(DateStorage::GuestHeap))
            }
            TypedIntrinsic::Capability(CapabilityOperation::Random(RandomOperation::Number)) => {
                Some(Self::Random)
            }
            _ => None,
        }
    }
    fn binding(
        self,
    ) -> (
        &'static str,
        &'static str,
        Vec<&'static str>,
        Vec<&'static str>,
    ) {
        match self {
            Self::Wait => (
                "wasi:clocks/monotonic-clock@0.3.0",
                "wait-for",
                vec!["i64"],
                vec![],
            ),
            Self::Monotonic => (
                "wasi:clocks/monotonic-clock@0.3.0",
                "now",
                vec![],
                vec!["i64"],
            ),
            Self::Date(_) => ("wasi:clocks/system-clock@0.3.0", "now", vec!["i32"], vec![]),
            Self::Random => (
                "wasi:random/random@0.3.0",
                "get-random-u64",
                vec![],
                vec!["i64"],
            ),
        }
    }
    pub(crate) fn body(self, export: &str, native: &str) -> String {
        match self {
            Self::Wait => format!(
                r#"(func (export {export:?}) (param $milliseconds f64)
                (if (f64.lt (local.get $milliseconds) (f64.const 0)) (then unreachable))
                (call ${native} (i64.trunc_f64_u (f64.mul (local.get $milliseconds) (f64.const 1000000)))))"#
            ),
            Self::Monotonic => format!(
                r#"(func (export {export:?}) (result f64)
                (f64.div (f64.convert_i64_u (call ${native})) (f64.const 1000000)))"#
            ),
            Self::Random => format!(
                r#"(func (export {export:?}) (result f64)
                (f64.div (f64.convert_i64_u (i64.shr_u (call ${native}) (i64.const 11))) (f64.const 9007199254740992)))"#
            ),
            Self::Date(storage) => {
                let (allocate, release) = match storage {
                    DateStorage::Standalone => ("", ""),
                    DateStorage::GuestHeap => (
                        "(local.set $scratch (call $realloc (i32.const 0) (i32.const 0) (i32.const 8) (i32.const 16)))",
                        "(drop (call $realloc (local.get $scratch) (i32.const 16) (i32.const 8) (i32.const 0)))",
                    ),
                };
                format!(
                    r#"(func (export {export:?}) (result f64) (local $scratch i32) (local $value f64)
                    {allocate}
                    (call ${native} (local.get $scratch))
                    (if (i32.ge_u (i32.load offset=8 (local.get $scratch)) (i32.const 1000000000)) (then unreachable))
                    (local.set $value (f64.add (f64.mul (f64.convert_i64_s (i64.load (local.get $scratch))) (f64.const 1000))
                        (f64.convert_i32_u (i32.div_u (i32.load offset=8 (local.get $scratch)) (i32.const 1000000)))))
                    {release}
                    (local.get $value))"#
                )
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum DateStorage {
    Standalone,
    GuestHeap,
}

pub(in crate::waffle_backend) fn declare_imports(
    module: &mut Module<'static>,
    contract: &ResolvedContract,
) -> BTreeMap<String, Func> {
    if contract.wit.is_none() || contract.http_handler.is_some() {
        return BTreeMap::new();
    }
    contract
        .intrinsics
        .iter()
        .filter_map(|(name, intrinsic)| {
            let (interface, function, params, results) = Scalar::of(intrinsic)?.binding();
            let imports = forward::declare_imports(
                module,
                interface,
                &[forward::Function {
                    name: function.into(),
                    params,
                    results,
                    target: String::new(),
                }],
            );
            Some((name.clone(), imports[function]))
        })
        .collect()
}

pub(in crate::waffle_backend) fn emit(
    module: &mut Module<'static>,
    contract: &ResolvedContract,
    memory: Memory,
    allocator: Option<AllocationFuncs>,
    imports: BTreeMap<String, Func>,
) -> Result<BTreeMap<String, Func>> {
    let mut helpers = BTreeMap::new();
    for (name, function) in imports {
        let scalar = Scalar::of(&contract.intrinsics[&name]).unwrap();
        let (_, _, params, results) = scalar.binding();
        let mut wat = format!(
            "(module (import \"host\" \"memory\" (memory 1)) (import \"host\" \"native\" (func $native (param {}) (result {})))",
            params.join(" "),
            results.join(" ")
        );
        let mut imports = BTreeMap::from([("native", function)]);
        if matches!(scalar, Scalar::Date(_)) {
            wat.push_str("(import \"host\" \"realloc\" (func $realloc (param i32 i32 i32 i32) (result i32)))");
            imports.insert("realloc", allocator.unwrap().realloc);
        }
        wat.push_str(&scalar.body("run", "native"));
        wat.push(')');
        helpers.insert(
            name,
            runtime::emit_functions(module, memory, &wat, &imports)?["run"],
        );
    }
    Ok(helpers)
}
