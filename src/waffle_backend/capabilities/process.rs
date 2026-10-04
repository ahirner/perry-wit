//! Instance-local exit status, published through the standard CLI exit interface.

use std::collections::BTreeMap;

use anyhow::Result;
use perry_hir::types::Type as HirType;
use waffle::{
    Func, GlobalData, Memory, Module, Operator as Op,
    Type::{F64, I32},
    Value,
};

use super::{CapabilityImplementation, CapabilityOperation, CapabilityPlan, LowerCapability};
use crate::waffle_backend::{
    resolve::{ResolvedContract, TypedIntrinsic},
    runtime::builder::{self, Builder},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ProcessOperation {
    Exit,
    ExitCurrent,
    GetExitCode,
    SetExitCode,
    ClearExitCode,
}

impl ProcessOperation {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Exit => "process.exit",
            Self::ExitCurrent => "process.exit-current",
            Self::GetExitCode => "process.exitCode.get",
            Self::SetExitCode => "process.exitCode.set",
            Self::ClearExitCode => "process.exitCode.clear",
        }
    }
}

impl LowerCapability for ProcessOperation {
    fn lower(&self) -> CapabilityPlan {
        CapabilityPlan {
            params: if matches!(self, Self::Exit | Self::SetExitCode) {
                vec![HirType::Number]
            } else {
                vec![]
            },
            result: match self {
                Self::GetExitCode => HirType::Union(vec![HirType::Number, HirType::Void]),
                Self::SetExitCode => HirType::Number,
                _ => HirType::Void,
            },
            implementation: CapabilityImplementation::Process,
        }
    }
}

pub(crate) struct ProcessImports {
    operations: BTreeMap<String, ProcessOperation>,
    exit: Option<Func>,
    finish: bool,
}

pub(crate) fn declare(
    module: &mut Module<'static>,
    contract: &ResolvedContract,
) -> Option<ProcessImports> {
    let operations: BTreeMap<_, _> = contract
        .intrinsics
        .iter()
        .filter_map(|(name, intrinsic)| match intrinsic {
            TypedIntrinsic::Capability(CapabilityOperation::Process(operation)) => {
                Some((name.clone(), *operation))
            }
            _ => None,
        })
        .collect();
    if operations.is_empty() {
        return None;
    }
    let finish = operations.values().any(|op| {
        matches!(
            op,
            ProcessOperation::SetExitCode | ProcessOperation::ClearExitCode
        )
    }) && contract.wit.as_ref().is_some_and(|wit| {
        wit.functions
            .values()
            .any(|export| export.core_name == "wasi:cli/run@0.3.0#run")
    });
    let exit = (finish
        || operations
            .values()
            .any(|op| matches!(op, ProcessOperation::Exit | ProcessOperation::ExitCurrent)))
    .then(|| {
        let signature = module.signatures.push(waffle::SignatureData {
            params: vec![I32],
            returns: vec![],
        });
        let function = module.funcs.push(waffle::FuncDecl::Import(
            signature,
            "process.exit-native".into(),
        ));
        module.imports.push(waffle::Import {
            module: "wasi:cli/exit@0.3.0".into(),
            name: "exit-with-code".into(),
            kind: waffle::ImportKind::Func(function),
        });
        function
    });
    Some(ProcessImports {
        operations,
        exit,
        finish,
    })
}

fn number(b: &mut Builder, value: f64) -> Value {
    b.op(
        Op::F64Const {
            value: value.to_bits(),
        },
        &[],
        F64,
    )
}

fn validate_code(b: &mut Builder, value: Value) {
    let integer = b.op(Op::F64Trunc, &[value], F64);
    let valid = b.op(Op::F64Eq, &[value, integer], I32);
    b.require(valid);
    let maximum = number(b, 9_007_199_254_740_991.0);
    let magnitude = b.op(Op::F64Abs, &[value], F64);
    let in_range = b.op(Op::F64Le, &[magnitude, maximum], I32);
    b.require(in_range);
}

fn exit(b: &mut Builder, native: Func, value: Value) {
    let width = number(b, 256.0);
    let quotient = b.op(Op::F64Div, &[value, width], F64);
    let quotient = b.op(Op::F64Floor, &[quotient], F64);
    let multiple = b.op(Op::F64Mul, &[quotient, width], F64);
    let code = b.op(Op::F64Sub, &[value, multiple], F64);
    let code = b.op(Op::I32TruncF64U, &[code], I32);
    b.call(native, &[code], &[]);
    b.body
        .set_terminator(b.block, waffle::Terminator::Unreachable);
}

pub(crate) fn emit(
    module: &mut Module<'static>,
    memory: Memory,
    imports: ProcessImports,
) -> Result<(BTreeMap<String, Func>, Option<Func>)> {
    let state = module.globals.push(GlobalData {
        ty: F64,
        value: Some(f64::NAN.to_bits()),
        mutable: true,
    });
    let mut functions = BTreeMap::new();
    for (name, operation) in imports.operations {
        let params = if matches!(
            operation,
            ProcessOperation::Exit | ProcessOperation::SetExitCode
        ) {
            vec![F64]
        } else {
            vec![]
        };
        let returns = if matches!(
            operation,
            ProcessOperation::GetExitCode | ProcessOperation::SetExitCode
        ) {
            vec![F64]
        } else {
            vec![]
        };
        let function = builder::declare(module, &name, &params, &returns);
        let mut b = Builder::new(module, function, memory);
        match operation {
            ProcessOperation::GetExitCode => {
                let value = b.op(
                    Op::GlobalGet {
                        global_index: state,
                    },
                    &[],
                    F64,
                );
                b.ret(&[value]);
            }
            ProcessOperation::ClearExitCode => {
                let value = number(&mut b, f64::NAN);
                b.effect(
                    Op::GlobalSet {
                        global_index: state,
                    },
                    &[value],
                );
                b.ret(&[]);
            }
            ProcessOperation::SetExitCode | ProcessOperation::Exit => {
                let value = b.param(0);
                validate_code(&mut b, value);
                if operation == ProcessOperation::SetExitCode {
                    b.effect(
                        Op::GlobalSet {
                            global_index: state,
                        },
                        &[value],
                    );
                    b.ret(&[value]);
                } else {
                    exit(&mut b, imports.exit.unwrap(), value);
                }
            }
            ProcessOperation::ExitCurrent => {
                let value = b.op(
                    Op::GlobalGet {
                        global_index: state,
                    },
                    &[],
                    F64,
                );
                let present = b.op(Op::F64Eq, &[value, value], I32);
                let zero = number(&mut b, 0.0);
                let value = b.op(Op::Select, &[value, zero, present], F64);
                exit(&mut b, imports.exit.unwrap(), value);
            }
        }
        b.finish(module, function)?;
        functions.insert(name, function);
    }
    let finish = if imports.finish {
        let function = builder::declare(module, "process.finish-command", &[], &[]);
        let mut b = Builder::new(module, function, memory);
        let value = b.op(
            Op::GlobalGet {
                global_index: state,
            },
            &[],
            F64,
        );
        let zero = number(&mut b, 0.0);
        let present = b.op(Op::F64Eq, &[value, value], I32);
        let nonzero = b.op(Op::F64Ne, &[value, zero], I32);
        let nonzero = b.op(Op::I32And, &[present, nonzero], I32);
        let terminate = b.body.add_block();
        let success = b.body.add_block();
        b.branch(nonzero, terminate, success);
        b.block = terminate;
        exit(&mut b, imports.exit.unwrap(), value);
        b.block = success;
        b.ret(&[]);
        b.finish(module, function)?;
        Some(function)
    } else {
        None
    };
    Ok((functions, finish))
}
