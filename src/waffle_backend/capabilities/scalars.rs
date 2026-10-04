//! Scalar capability adapters preserve source units around canonical WIT calls.

use super::super::{
    allocation::AllocationFuncs,
    capabilities::{CapabilityOperation, ClockOperation, RandomOperation},
    component::forward,
    resolve::{ResolvedContract, TypedIntrinsic},
    runtime::builder::{self, Builder},
};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{Func, Memory, Module};

#[derive(Clone, Copy)]
pub(crate) enum Scalar {
    Wait,
    Timeout,
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
            TypedIntrinsic::Capability(CapabilityOperation::Clock(ClockOperation::Timeout)) => {
                Some(Self::Timeout)
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
            Self::Wait | Self::Timeout => (
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
            Self::Timeout => format!(
                r#"(func (export {export:?}) (param $milliseconds f64)
                (if (i32.eqz (i32.and (f64.ge (local.get $milliseconds) (f64.const 1)) (f64.le (local.get $milliseconds) (f64.const 2147483647))))
                  (then (local.set $milliseconds (f64.const 1))))
                (call ${native} (i64.mul (i64.trunc_f64_u (local.get $milliseconds)) (i64.const 1000000))))"#
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
    if contract.wit.is_none() {
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
        use waffle::{
            Operator as Op,
            Type::{F64, I32, I64},
        };
        let signature = contract.intrinsics[&name].core_signature()?;
        let helper = builder::declare(module, &name, &signature.params, &signature.returns);
        let mut b = Builder::new(module, helper, memory);
        let real = |b: &mut Builder, value: f64| {
            b.op(
                Op::F64Const {
                    value: value.to_bits(),
                },
                &[],
                F64,
            )
        };
        let result = match scalar {
            Scalar::Wait | Scalar::Timeout => {
                let value = b.param(0);
                let zero = real(&mut b, 0.0);
                let value = if matches!(scalar, Scalar::Timeout) {
                    let minimum = real(&mut b, 1.0);
                    let maximum = real(&mut b, 2147483647.0);
                    let low = b.op(Op::F64Ge, &[value, minimum], I32);
                    let high = b.op(Op::F64Le, &[value, maximum], I32);
                    let valid = b.op(Op::I32And, &[low, high], I32);
                    let selected = b.op(Op::Select, &[value, minimum, valid], F64);
                    b.op(Op::F64Trunc, &[selected], F64)
                } else {
                    let valid = b.op(Op::F64Ge, &[value, zero], I32);
                    b.require(valid);
                    value
                };
                let million = real(&mut b, 1_000_000.0);
                let ns = b.op(Op::F64Mul, &[value, million], F64);
                let ns = b.op(Op::I64TruncF64U, &[ns], I64);
                b.call(function, &[ns], &[]);
                vec![]
            }
            Scalar::Monotonic => {
                let nanos = b.call(function, &[], &[I64])[0];
                let nanos = b.op(Op::F64ConvertI64U, &[nanos], F64);
                let million = real(&mut b, 1_000_000.0);
                vec![b.op(Op::F64Div, &[nanos, million], F64)]
            }
            Scalar::Random => {
                let word = b.call(function, &[], &[I64])[0];
                let shift = b.op(Op::I64Const { value: 11 }, &[], I64);
                let word = b.op(Op::I64ShrU, &[word, shift], I64);
                let word = b.op(Op::F64ConvertI64U, &[word], F64);
                let scale = real(&mut b, 9007199254740992.0);
                vec![b.op(Op::F64Div, &[word, scale], F64)]
            }
            Scalar::Date(_) => {
                let allocator = allocator.unwrap();
                let zero = b.integer(0);
                let eight = b.integer(8);
                let size = b.integer(16);
                let scratch = b.call(allocator.realloc, &[zero, zero, eight, size], &[I32])[0];
                b.call(function, &[scratch], &[]);
                let nanos = b.load(scratch, 8, I32);
                let billion = b.integer(1_000_000_000);
                let valid = b.op(Op::I32LtU, &[nanos, billion], I32);
                b.require(valid);
                let seconds = b.op(
                    Op::I64Load {
                        memory: b.memory(0),
                    },
                    &[scratch],
                    I64,
                );
                let seconds = b.op(Op::F64ConvertI64S, &[seconds], F64);
                let thousand = real(&mut b, 1000.0);
                let seconds = b.op(Op::F64Mul, &[seconds, thousand], F64);
                let million = b.integer(1_000_000);
                let millis = b.op(Op::I32DivU, &[nanos, million], I32);
                let millis = b.op(Op::F64ConvertI32U, &[millis], F64);
                let value = b.op(Op::F64Add, &[seconds, millis], F64);
                b.call(allocator.realloc, &[scratch, size, eight, zero], &[I32]);
                vec![value]
            }
        };
        b.ret(&result);
        b.finish(module, helper)?;
        helpers.insert(name, helper);
    }
    Ok(helpers)
}
