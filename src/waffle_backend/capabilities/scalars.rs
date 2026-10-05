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
    Wait,
    Timeout,
    TimeoutValue,
    Monotonic,
    Date,
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
                ClockOperation::TimeoutValue,
            )) => Some(Self::TimeoutValue),
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
            Self::Wait | Self::Timeout | Self::TimeoutValue => (
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

pub(crate) struct NativeWait {
    pub(crate) wait: Option<Func>,
    pub(crate) operations: Option<super::super::runtime::operations::Operations>,
}

pub(in crate::waffle_backend) fn emit(
    module: &mut Module<'static>,
    contract: &ResolvedContract,
    memory: Memory,
    allocator: Option<AllocationFuncs>,
    imports: BTreeMap<String, Func>,
    native: NativeWait,
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
        let result = match scalar {
            Scalar::Wait | Scalar::Timeout | Scalar::TimeoutValue => {
                if let Some(operations) = native.operations {
                    let signal = if matches!(scalar, Scalar::TimeoutValue) {
                        b.param(3)
                    } else {
                        b.integer(0)
                    };
                    b.call(operations.bind_signal, &[signal], &[]);
                    let aborted = b.call(operations.aborted, &[], &[I32])[0];
                    let cancelled = b.body.add_block();
                    let begin = b.body.add_block();
                    b.branch(aborted, cancelled, begin);
                    b.block = cancelled;
                    let thrown = b.integer(1);
                    let reason = b.number(20.0);
                    let zero = b.integer(0);
                    b.call(operations.bind_signal, &[zero], &[]);
                    b.ret(&[thrown, reason]);
                    b.block = begin;
                }
                let value = b.param(0);
                let zero = b.number(0.0);
                let value = if matches!(scalar, Scalar::Timeout | Scalar::TimeoutValue) {
                    let minimum = b.number(1.0);
                    let maximum = b.number(2147483647.0);
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
                let million = b.number(1_000_000.0);
                let ns = b.op(Op::F64Mul, &[value, million], F64);
                let ns = b.op(Op::I64TruncF64U, &[ns], I64);
                let subtask = b.call(function, &[ns], &[I32])[0];
                let status = b.call(native.wait.unwrap(), &[subtask], &[I32])[0];
                if let Some(operations) = native.operations {
                    let zero = b.integer(0);
                    b.call(operations.bind_signal, &[zero], &[]);
                }
                let returned = b.integer(2);
                let success = b.op(Op::I32Eq, &[status, returned], I32);
                let complete = b.body.add_block();
                let cancelled = b.body.add_block();
                b.branch(success, complete, cancelled);
                b.block = cancelled;
                let thrown = b.integer(1);
                let reason = b.number(20.0);
                b.ret(&[thrown, reason]);
                b.block = complete;
                let tag = b.integer(0);
                let value = if matches!(scalar, Scalar::TimeoutValue) {
                    b.param(1)
                } else {
                    b.number(0.0)
                };
                vec![tag, value]
            }
            Scalar::Monotonic => {
                let nanos = b.call(function, &[], &[I64])[0];
                let nanos = b.op(Op::F64ConvertI64U, &[nanos], F64);
                let million = b.number(1_000_000.0);
                vec![b.op(Op::F64Div, &[nanos, million], F64)]
            }
            Scalar::Random => {
                let word = b.call(function, &[], &[I64])[0];
                let shift = b.op(Op::I64Const { value: 11 }, &[], I64);
                let word = b.op(Op::I64ShrU, &[word, shift], I64);
                let word = b.op(Op::F64ConvertI64U, &[word], F64);
                let scale = b.number(9007199254740992.0);
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
                let thousand = b.number(1000.0);
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
