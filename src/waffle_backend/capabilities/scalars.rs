//! Scalar capability adapters preserve source units around canonical WIT calls.

use super::super::{
    allocation::AllocationFuncs,
    capabilities::{CapabilityOperation, ClockOperation, RandomOperation},
    resolve::{ResolvedContract, TypedIntrinsic},
    runtime::builder::{self, Builder},
    runtime::imports,
};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{Func, Memory, Module};

#[derive(Clone, Copy)]
pub(crate) enum Scalar {
    Exit,
    Wait,
    Timeout,
    Monotonic,
    Date,
    Random,
}
impl Scalar {
    fn of(intrinsic: &TypedIntrinsic) -> Option<Self> {
        match intrinsic {
            TypedIntrinsic::Capability(CapabilityOperation::Exit) => Some(Self::Exit),
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
                Some(Self::Date)
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
            Self::Exit => ("wasi:cli/exit@0.3.0", "exit-with-code", vec!["i32"], vec![]),
            Self::Wait | Self::Timeout => (
                "wasi:clocks/monotonic-clock@0.3.0",
                "[async-lower]wait-for",
                vec!["i64"],
                vec!["i32"],
            ),
            Self::Monotonic => (
                "wasi:clocks/monotonic-clock@0.3.0",
                "now",
                vec![],
                vec!["i64"],
            ),
            Self::Date => ("wasi:clocks/system-clock@0.3.0", "now", vec!["i32"], vec![]),
            Self::Random => (
                "wasi:random/random@0.3.0",
                "get-random-u64",
                vec![],
                vec!["i64"],
            ),
        }
    }
}

pub(in crate::waffle_backend) fn declare_imports(
    module: &mut Module<'static>,
    contract: &ResolvedContract,
) -> BTreeMap<String, Func> {
    contract
        .intrinsics
        .iter()
        .filter_map(|(name, intrinsic)| {
            let (interface, function, params, results) = Scalar::of(intrinsic)?.binding();
            let imports = imports::declare_imports(
                module,
                interface,
                &[imports::Function {
                    name: function.into(),
                    params,
                    results,
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
    await_subtask: Option<Func>,
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
            Scalar::Exit => {
                let value = b.param(0);
                let integer = b.op(Op::F64Trunc, &[value], F64);
                let valid = b.op(Op::F64Eq, &[value, integer], I32);
                b.require(valid);
                let maximum = real(&mut b, 9_007_199_254_740_991.0);
                let magnitude = b.op(Op::F64Abs, &[value], F64);
                let in_range = b.op(Op::F64Le, &[magnitude, maximum], I32);
                b.require(in_range);
                let width = real(&mut b, 256.0);
                let quotient = b.op(Op::F64Div, &[value, width], F64);
                let quotient = b.op(Op::F64Floor, &[quotient], F64);
                let multiple = b.op(Op::F64Mul, &[quotient, width], F64);
                let code = b.op(Op::F64Sub, &[value, multiple], F64);
                let code = b.op(Op::I32TruncF64U, &[code], I32);
                b.call(function, &[code], &[]);
                let zero = b.integer(0);
                b.require(zero);
                vec![]
            }
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
                let subtask = b.call(function, &[ns], &[I32])[0];
                let status = b.call(await_subtask.unwrap(), &[subtask], &[I32])[0];
                let returned = b.integer(2);
                let success = b.op(Op::I32Eq, &[status, returned], I32);
                b.require(success);
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
            Scalar::Date => {
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
