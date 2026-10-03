//! Direct Perry HIR → WAFFLE SSA lowering.
//!
//! Lowers assignments, branches, loops, function calls, and returns directly to
//! typed basic blocks, SSA values, and block parameters without linear memory
//! overhead for primitive values.

use std::collections::BTreeMap;

use anyhow::{Result, bail, ensure};
use perry_hir::ir::{BinaryOp, CatchClause, CompareOp, Expr, Function, Module as HirModule, Stmt};
use perry_hir::types::{FuncId, LocalId, Type as HirType};
use waffle::{
    Block, BlockTarget, Export, ExportKind, Func, FuncDecl, FunctionBody, Import, ImportKind,
    Module, Operator, SignatureData, Terminator, Type, Value, ValueDef,
};

use crate::waffle_backend::exceptions::{
    ExitReason, ReturnTarget, TryScope, UnwindContext, UnwindTarget,
};
use crate::waffle_backend::resolve::{ResolvedContract, ResolvedInputKind, TypedIntrinsic};

/// Compiles a resolved HIR module into a validated WAFFLE module.
pub(crate) fn lower_module(
    hir: &HirModule,
    contract: &ResolvedContract,
) -> Result<Module<'static>> {
    let mut module = Module::empty();

    // 1. Register imports for declared intrinsics
    let mut intrinsic_funcs = BTreeMap::new();
    for (name, intrinsic) in &contract.intrinsics {
        let (params, returns) = match intrinsic {
            TypedIntrinsic::WaitFor => (vec![Type::F64], vec![]),
            TypedIntrinsic::HostDouble => (vec![Type::F64], vec![Type::F64]),
            TypedIntrinsic::ReadChunk => (vec![Type::I32], vec![Type::F64]),
            TypedIntrinsic::ByteAt => (vec![Type::F64], vec![Type::F64]),
            TypedIntrinsic::StreamDrop => (vec![Type::I32], vec![]),
            TypedIntrinsic::StreamReset => (vec![], vec![]),
            TypedIntrinsic::Custom {
                params, returns, ..
            } => (params.clone(), returns.clone()),
        };
        let signature = module.signatures.push(SignatureData { params, returns });
        let func = module.funcs.push(FuncDecl::Import(signature, name.clone()));
        module.imports.push(Import {
            module: "host".into(),
            name: name.clone(),
            kind: ImportKind::Func(func),
        });
        intrinsic_funcs.insert(name.clone(), func);
    }

    // Add stream reset/drop imports if byte stream contract
    let stream_helpers = if contract.input_kind == ResolvedInputKind::ByteStream {
        let drop = if let Some(&f) = intrinsic_funcs.get("drop") {
            f
        } else {
            let sig = module.signatures.push(SignatureData {
                params: vec![Type::I32],
                returns: vec![],
            });
            let f = module.funcs.push(FuncDecl::Import(sig, "drop".into()));
            module.imports.push(Import {
                module: "host".into(),
                name: "drop".into(),
                kind: ImportKind::Func(f),
            });
            f
        };
        let reset = if let Some(&f) = intrinsic_funcs.get("reset") {
            f
        } else {
            let sig = module.signatures.push(SignatureData {
                params: vec![],
                returns: vec![],
            });
            let f = module.funcs.push(FuncDecl::Import(sig, "reset".into()));
            module.imports.push(Import {
                module: "host".into(),
                name: "reset".into(),
                kind: ImportKind::Func(f),
            });
            f
        };
        Some((drop, reset))
    } else {
        None
    };

    // 1.5. Declare and export linear memory for Canonical ABI options and component framing
    let memory = module.memories.push(waffle::MemoryData {
        initial_pages: 1,
        maximum_pages: None,
        segments: vec![],
    });
    module.exports.push(Export {
        name: "memory".to_string(),
        kind: ExportKind::Memory(memory),
    });

    // 2. Pre-declare all module functions to allow mutual / intra-module calls
    let mut func_decls = BTreeMap::new();
    let mut func_is_exported = BTreeMap::new();
    let mut func_return_types = BTreeMap::new();

    for func in &hir.functions {
        let is_exported = func.is_exported || (func.id == contract.entry_func_id);
        func_is_exported.insert(func.id, is_exported);
        func_return_types.insert(func.id, func.return_type.clone());
        let params = func
            .params
            .iter()
            .map(|p| map_type_to_waffle(&p.ty))
            .collect::<Result<Vec<_>>>()?;
        let returns = if is_exported {
            map_return_type_to_waffle(&func.return_type)?
        } else {
            vec![Type::I32, Type::F64]
        };
        let sig = module.signatures.push(SignatureData { params, returns });
        let mut body = FunctionBody::new(&module, sig);
        body.set_terminator(body.entry, Terminator::Unreachable);
        let declaration = module
            .funcs
            .push(FuncDecl::Body(sig, func.name.clone(), body));
        func_decls.insert(func.id, declaration);
    }

    // 3. Lower each function body
    for func in &hir.functions {
        let func_decl = func_decls[&func.id];
        let sig = module.funcs[func_decl].sig();
        let body = lower_function_body(
            func,
            sig,
            &module,
            contract,
            &intrinsic_funcs,
            &func_decls,
            &func_is_exported,
            &func_return_types,
            stream_helpers,
            memory,
        )?;
        module.funcs[func_decl] = FuncDecl::Body(sig, func.name.clone(), body);

        if func.is_exported || func.id == contract.entry_func_id {
            let export_name = if func.name == "main"
                || func.name == "experiment"
                || func.id == contract.entry_func_id
            {
                "run"
            } else {
                &func.name
            };
            module.exports.push(Export {
                name: export_name.into(),
                kind: ExportKind::Func(func_decl),
            });
        }
    }

    Ok(module)
}

fn map_type_to_waffle(ty: &HirType) -> Result<Type> {
    match ty {
        HirType::Number | HirType::Any => Ok(Type::F64),
        HirType::Boolean => Ok(Type::I32),
        HirType::Named(name) if name == "ByteStream" => Ok(Type::I32),
        _ => bail!("Unsupported parameter type in WAFFLE lowering: {ty:?}"),
    }
}

fn map_return_type_to_waffle(ty: &HirType) -> Result<Vec<Type>> {
    match ty {
        HirType::Void => Ok(vec![]),
        HirType::Number | HirType::Any => Ok(vec![Type::F64]),
        HirType::Boolean => Ok(vec![Type::I32]),
        HirType::Generic { base, type_args } if base == "Result" && type_args.len() == 2 => {
            Ok(vec![Type::I32])
        }
        HirType::Promise(inner) => map_return_type_to_waffle(inner),
        _ => bail!("Unsupported return type in WAFFLE lowering: {ty:?}"),
    }
}

/// Function body lowerer.
struct FunctionLowerer<'a> {
    module: &'a Module<'static>,
    _contract: &'a ResolvedContract,
    body: FunctionBody,
    block: Block,
    locals: BTreeMap<LocalId, Value>,
    intrinsic_funcs: &'a BTreeMap<String, Func>,
    func_decls: &'a BTreeMap<FuncId, Func>,
    func_is_exported: &'a BTreeMap<FuncId, bool>,
    func_return_types: &'a BTreeMap<FuncId, HirType>,
    stream_helpers: Option<(Func, Func)>,
    stream_parameter: Option<LocalId>,
    awaited_calls: usize,
    unwind_ctx: UnwindContext,
    is_exported: bool,
    returns_wit_result: bool,
    memory: waffle::Memory,
}

#[allow(clippy::too_many_arguments)]
fn lower_function_body(
    func: &Function,
    sig: waffle::Signature,
    module: &Module<'static>,
    contract: &ResolvedContract,
    intrinsic_funcs: &BTreeMap<String, Func>,
    func_decls: &BTreeMap<FuncId, Func>,
    func_is_exported: &BTreeMap<FuncId, bool>,
    func_return_types: &BTreeMap<FuncId, HirType>,
    stream_helpers: Option<(Func, Func)>,
    memory: waffle::Memory,
) -> Result<FunctionBody> {
    let body = FunctionBody::new(module, sig);
    let entry = body.entry;
    let mut locals = BTreeMap::new();
    let mut stream_parameter = None;

    // Map entry block parameters to function parameters
    for (i, param) in func.params.iter().enumerate() {
        let val = body.blocks[entry].params[i].1;
        locals.insert(param.id, val);
        if matches!(&param.ty, HirType::Named(n) if n == "ByteStream") {
            stream_parameter = Some(param.id);
        }
    }

    let is_exported = func_is_exported[&func.id];
    let mut ret_ty = &func.return_type;
    while let HirType::Promise(inner) = ret_ty {
        ret_ty = inner;
    }
    let returns_wit_result = if func.id == contract.entry_func_id {
        contract.entry_returns_wit_result()
    } else {
        matches!(ret_ty, HirType::Generic { base, .. } if base == "Result")
    };

    let mut lowerer = FunctionLowerer {
        module,
        _contract: contract,
        body,
        block: entry,
        locals,
        intrinsic_funcs,
        func_decls,
        func_is_exported,
        func_return_types,
        stream_helpers,
        stream_parameter,
        awaited_calls: 0,
        unwind_ctx: UnwindContext::new(),
        is_exported,
        returns_wit_result,
        memory,
    };

    if let Some((_, reset)) = lowerer.stream_helpers {
        lowerer.op(
            Operator::Call {
                function_index: reset,
            },
            &[],
            &[],
        );
    }

    lowerer.statements(&func.body)?;

    // If the block is not terminated, emit default return or ensure proper termination
    if lowerer.body.blocks[lowerer.block].terminator == Terminator::None {
        lowerer.cleanup_resources();
        let ret_types = &module.signatures[sig].returns;
        if ret_types.is_empty() {
            lowerer
                .body
                .set_terminator(lowerer.block, Terminator::Return { values: vec![] });
        } else if ret_types.len() == 1 && ret_types[0] == Type::F64 {
            let zero = lowerer.op(
                Operator::F64Const {
                    value: 0f64.to_bits(),
                },
                &[],
                &[Type::F64],
            );
            lowerer
                .body
                .set_terminator(lowerer.block, Terminator::Return { values: vec![zero] });
        } else if ret_types.len() == 1 && ret_types[0] == Type::I32 {
            let zero = lowerer.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
            lowerer
                .body
                .set_terminator(lowerer.block, Terminator::Return { values: vec![zero] });
        } else if ret_types.len() == 2 && ret_types[0] == Type::I32 && ret_types[1] == Type::F64 {
            let zero_i32 = lowerer.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
            let zero_f64 = lowerer.op(
                Operator::F64Const {
                    value: 0f64.to_bits(),
                },
                &[],
                &[Type::F64],
            );
            lowerer.body.set_terminator(
                lowerer.block,
                Terminator::Return {
                    values: vec![zero_i32, zero_f64],
                },
            );
        } else {
            bail!(
                "Unterminated block in function '{}' with non-void return signature {:?}",
                func.name,
                ret_types
            );
        }
    }

    lowerer.body.validate()?;
    lowerer.body.verify_reducible()?;

    Ok(lowerer.body)
}

impl<'a> FunctionLowerer<'a> {
    fn statements(&mut self, stmts: &[Stmt]) -> Result<()> {
        for stmt in stmts {
            if self.body.blocks[self.block].terminator != Terminator::None {
                break;
            }
            match stmt {
                Stmt::Let {
                    id,
                    init: Some(expr),
                    ..
                } => {
                    let val = self.expression(expr)?;
                    ensure!(
                        self.locals.insert(*id, val).is_none(),
                        "Duplicate local binding id: {:?}",
                        id
                    );
                }
                Stmt::Expr(Expr::LocalSet(id, expr)) => {
                    let val = self.expression(expr)?;
                    self.locals.insert(*id, val);
                }
                Stmt::Expr(Expr::Await(expr)) => {
                    self.await_expression(expr, true)?;
                }
                Stmt::Expr(expr) => {
                    self.expression(expr)?;
                }
                Stmt::Return(expr) => {
                    let ret_val = expr
                        .as_ref()
                        .map(|expr| self.expression(expr))
                        .transpose()?;

                    match self.unwind_ctx.target_for_return() {
                        ReturnTarget::Finally {
                            block: finally_block,
                            scope_locals,
                        } => {
                            let payload = if let Some(val) = ret_val {
                                let ty = self.body.values[val]
                                    .ty(&self.body.type_pool)
                                    .unwrap_or(Type::F64);
                                if ty == Type::I32 {
                                    self.op(Operator::F64ConvertI32U, &[val], &[Type::F64])
                                } else {
                                    val
                                }
                            } else {
                                self.op(
                                    Operator::F64Const {
                                        value: 0f64.to_bits(),
                                    },
                                    &[],
                                    &[Type::F64],
                                )
                            };

                            let reason_val = self.op(
                                Operator::I32Const {
                                    value: ExitReason::Return.tag(),
                                },
                                &[],
                                &[Type::I32],
                            );

                            let mut args = vec![reason_val, payload];
                            for id in &scope_locals {
                                args.push(self.locals[id]);
                            }
                            self.branch(finally_block, args);
                        }
                        ReturnTarget::FunctionExit => {
                            self.cleanup_resources();
                            if self.is_exported {
                                if self.returns_wit_result {
                                    let addr = self.op(
                                        Operator::I32Const { value: 8 },
                                        &[],
                                        &[Type::I32],
                                    );
                                    let status_0 = self.op(
                                        Operator::I32Const { value: 0 },
                                        &[],
                                        &[Type::I32],
                                    );
                                    let val = ret_val.unwrap_or_else(|| {
                                        self.op(
                                            Operator::F64Const {
                                                value: 0f64.to_bits(),
                                            },
                                            &[],
                                            &[Type::F64],
                                        )
                                    });
                                    let val_f64 = {
                                        let ty = self.body.values[val]
                                            .ty(&self.body.type_pool)
                                            .unwrap_or(Type::F64);
                                        if ty == Type::I32 {
                                            self.op(Operator::F64ConvertI32U, &[val], &[Type::F64])
                                        } else {
                                            val
                                        }
                                    };
                                    self.op(
                                        Operator::I32Store {
                                            memory: waffle::MemoryArg {
                                                align: 2,
                                                offset: 0,
                                                memory: self.memory,
                                            },
                                        },
                                        &[addr, status_0],
                                        &[],
                                    );
                                    self.op(
                                        Operator::F64Store {
                                            memory: waffle::MemoryArg {
                                                align: 3,
                                                offset: 8,
                                                memory: self.memory,
                                            },
                                        },
                                        &[addr, val_f64],
                                        &[],
                                    );
                                    self.body.set_terminator(
                                        self.block,
                                        Terminator::Return {
                                            values: vec![addr],
                                        },
                                    );
                                } else {
                                    let expected_ret_types = &self.body.rets;
                                    let values = if let Some(val) = ret_val {
                                        if expected_ret_types.len() == 1 {
                                            let ty = self.body.values[val]
                                                .ty(&self.body.type_pool)
                                                .unwrap_or(Type::F64);
                                            if expected_ret_types[0] == Type::I32 && ty == Type::F64
                                            {
                                                vec![self.op(
                                                    Operator::I32TruncF64U,
                                                    &[val],
                                                    &[Type::I32],
                                                )]
                                            } else if expected_ret_types[0] == Type::F64
                                                && ty == Type::I32
                                            {
                                                vec![self.op(
                                                    Operator::F64ConvertI32U,
                                                    &[val],
                                                    &[Type::F64],
                                                )]
                                            } else {
                                                vec![val]
                                            }
                                        } else {
                                            vec![val]
                                        }
                                    } else {
                                        vec![]
                                    };
                                    self.body
                                        .set_terminator(self.block, Terminator::Return { values });
                                }
                            } else {
                                let ok_status = self.op(
                                    Operator::I32Const { value: 0 },
                                    &[],
                                    &[Type::I32],
                                );
                                let val = ret_val.unwrap_or_else(|| {
                                    self.op(
                                        Operator::F64Const {
                                            value: 0f64.to_bits(),
                                        },
                                        &[],
                                        &[Type::F64],
                                    )
                                });
                                let val_f64 = {
                                    let ty = self.body.values[val]
                                        .ty(&self.body.type_pool)
                                        .unwrap_or(Type::F64);
                                    if ty == Type::I32 {
                                        self.op(Operator::F64ConvertI32U, &[val], &[Type::F64])
                                    } else {
                                        val
                                    }
                                };
                                self.body.set_terminator(
                                    self.block,
                                    Terminator::Return {
                                        values: vec![ok_status, val_f64],
                                    },
                                );
                            }
                        }
                    }
                }
                Stmt::Throw(expr) => {
                    let err_val = self.expression(expr)?;
                    let err_val_f64 = {
                        let ty = self.body.values[err_val]
                            .ty(&self.body.type_pool)
                            .unwrap_or(Type::F64);
                        if ty == Type::I32 {
                            self.op(Operator::F64ConvertI32U, &[err_val], &[Type::F64])
                        } else {
                            err_val
                        }
                    };

                    self.emit_throw(err_val_f64);
                }
                Stmt::Try {
                    body,
                    catch,
                    finally,
                } => {
                    self.try_statement(body, catch.as_ref(), finally.as_deref())?;
                }
                Stmt::If {
                    condition,
                    then_branch,
                    else_branch,
                } => {
                    self.if_statement(
                        condition,
                        then_branch,
                        else_branch.as_deref().unwrap_or(&[]),
                    )?;
                }
                Stmt::While { condition, body } => {
                    self.while_loop(condition, body)?;
                }
                _ => bail!("Unsupported statement in WAFFLE lowering: {stmt:?}"),
            }
        }
        Ok(())
    }

    fn if_statement(
        &mut self,
        condition: &Expr,
        then_branch: &[Stmt],
        else_branch: &[Stmt],
    ) -> Result<()> {
        let cond_val = self.condition(condition)?;
        let incoming_locals = self.locals.clone();

        let then_block = self.body.add_block();
        let else_block = self.body.add_block();
        let join_block = self.body.add_block();
        self.body.blocks[join_block].desc = "branch join".into();

        let joined_locals = self.block_parameters(join_block, &incoming_locals);

        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: cond_val,
                if_true: BlockTarget {
                    block: then_block,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: else_block,
                    args: vec![],
                },
            },
        );

        // Lower then branch
        self.block = then_block;
        self.locals = incoming_locals.clone();
        self.statements(then_branch)?;
        if self.body.blocks[self.block].terminator == Terminator::None {
            let args = incoming_locals.keys().map(|id| self.locals[id]).collect();
            self.branch(join_block, args);
        }

        // Lower else branch
        self.block = else_block;
        self.locals = incoming_locals;
        self.statements(else_branch)?;
        if self.body.blocks[self.block].terminator == Terminator::None {
            let args = joined_locals.keys().map(|id| self.locals[id]).collect();
            self.branch(join_block, args);
        }

        if self.body.blocks[join_block].preds.is_empty() {
            self.body
                .set_terminator(join_block, Terminator::Unreachable);
        }
        self.block = join_block;
        self.locals = joined_locals;
        Ok(())
    }

    fn while_loop(&mut self, condition: &Expr, body: &[Stmt]) -> Result<()> {
        let incoming_locals = self.locals.clone();
        let header = self.body.add_block();
        self.body.blocks[header].desc = "loop header".into();

        let header_locals = self.block_parameters(header, &incoming_locals);
        let header_args = incoming_locals.values().copied().collect();
        self.branch(header, header_args);

        self.block = header;
        self.locals = header_locals;

        let cond_val = self.condition(condition)?;
        let condition_locals = self.locals.clone();

        let body_block = self.body.add_block();
        let exit_block = self.body.add_block();

        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: cond_val,
                if_true: BlockTarget {
                    block: body_block,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: exit_block,
                    args: vec![],
                },
            },
        );

        self.block = body_block;
        self.statements(body)?;
        if self.body.blocks[self.block].terminator == Terminator::None {
            let loop_args = incoming_locals.keys().map(|id| self.locals[id]).collect();
            self.branch(header, loop_args);
        }

        self.block = exit_block;
        self.locals = condition_locals;
        Ok(())
    }

    fn is_func_return_bool(&self, fid: &FuncId) -> bool {
        self.func_return_types.get(fid).map_or(false, |ty| {
            let mut t = ty;
            while let HirType::Promise(inner) = t {
                t = inner.as_ref();
            }
            matches!(t, HirType::Boolean)
        })
    }

    fn await_expression(&mut self, expr: &Expr, is_statement: bool) -> Result<Option<Value>> {
        let Expr::Call { callee, args, .. } = expr else {
            // Awaiting an immediate value / non-call expression:
            // Settle immediately and resume continuation with the evaluated value.
            let val = self.expression(expr)?;
            let result_val = if is_statement { None } else { Some(val) };
            return Ok(self.continuation(result_val));
        };

        match callee.as_ref() {
            Expr::ExternFuncRef { name, .. } => {
                let intrinsic_func = self
                    .intrinsic_funcs
                    .get(name)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("Unknown async intrinsic: {name}"))?;

                // Evaluate arguments left-to-right
                let mut arg_values = Vec::new();
                for arg in args {
                    arg_values.push(self.expression(arg)?);
                }

                let ret_types =
                    &self.module.signatures[self.module.funcs[intrinsic_func].sig()].returns;
                let returns = ret_types.clone();

                let call_res = self.op(
                    Operator::Call {
                        function_index: intrinsic_func,
                    },
                    &arg_values,
                    &returns,
                );

                let result_val = if returns.is_empty() || is_statement {
                    None
                } else {
                    Some(call_res)
                };

                Ok(self.continuation(result_val))
            }
            Expr::FuncRef(fid) => {
                let &func_idx = self
                    .func_decls
                    .get(fid)
                    .ok_or_else(|| anyhow::anyhow!("Unknown internal function id: {fid:?}"))?;
                let is_exported = *self.func_is_exported.get(fid).unwrap_or(&false);

                let mut arg_vals = Vec::new();
                for a in args {
                    arg_vals.push(self.expression(a)?);
                }

                if !is_exported {
                    let call_val = self.body.add_op(
                        self.block,
                        Operator::Call {
                            function_index: func_idx,
                        },
                        &arg_vals,
                        &[Type::I32, Type::F64],
                    );
                    let status = self.body.add_value(ValueDef::PickOutput(
                        call_val,
                        0,
                        Type::I32,
                    ));
                    self.body.append_to_block(self.block, status);
                    let payload = self.body.add_value(ValueDef::PickOutput(
                        call_val,
                        1,
                        Type::F64,
                    ));
                    self.body.append_to_block(self.block, payload);

                    let is_ok = self.op(Operator::I32Eqz, &[status], &[Type::I32]);
                    let ok_block = self.body.add_block();
                    self.body.blocks[ok_block].desc = "await call ok".into();
                    let err_block = self.body.add_block();
                    self.body.blocks[err_block].desc = "await call err".into();

                    self.body.set_terminator(
                        self.block,
                        Terminator::CondBr {
                            cond: is_ok,
                            if_true: BlockTarget {
                                block: ok_block,
                                args: vec![],
                            },
                            if_false: BlockTarget {
                                block: err_block,
                                args: vec![],
                            },
                        },
                    );

                    // Rejection enters guest exception path at the await
                    self.block = err_block;
                    self.emit_throw(payload);

                    self.block = ok_block;
                    let result_val = if is_statement {
                        None
                    } else if self.is_func_return_bool(fid) {
                        let payload_i32 = self.op(
                            Operator::I32TruncF64U,
                            &[payload],
                            &[Type::I32],
                        );
                        Some(payload_i32)
                    } else {
                        Some(payload)
                    };
                    Ok(self.continuation(result_val))
                } else {
                    let ret_types =
                        &self.module.signatures[self.module.funcs[func_idx].sig()].returns;
                    let returns = ret_types.clone();
                    let call_res = self.op(
                        Operator::Call {
                            function_index: func_idx,
                        },
                        &arg_vals,
                        &returns,
                    );
                    let result_val = if returns.is_empty() || is_statement {
                        None
                    } else {
                        Some(call_res)
                    };
                    Ok(self.continuation(result_val))
                }
            }
            _ => bail!("Await callee must be a declared intrinsic or function, got: {callee:?}"),
        }
    }

    fn continuation(&mut self, result: Option<Value>) -> Option<Value> {
        self.awaited_calls += 1;
        let resumed = self.body.add_block();
        self.body.blocks[resumed].desc = "await continuation".into();
        let resumed_locals = self.block_parameters(resumed, &self.locals.clone());
        let mut args: Vec<_> = self.locals.values().copied().collect();

        let resumed_result = result.map(|value| {
            args.push(value);
            let ty = self.body.values[value]
                .ty(&self.body.type_pool)
                .expect("Await results have one primitive type");
            self.body.add_blockparam(resumed, ty)
        });

        self.branch(resumed, args);
        self.block = resumed;
        self.locals = resumed_locals;
        resumed_result
    }

    fn condition(&mut self, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::Bool(b) => {
                let v = if *b { 1 } else { 0 };
                Ok(self.op(Operator::I32Const { value: v }, &[], &[Type::I32]))
            }
            Expr::Compare { op, left, right } => {
                let left_val = self.expression(left)?;
                let right_val = self.expression(right)?;
                let left_type = self.body.values[left_val].ty(&self.body.type_pool);
                let right_type = self.body.values[right_val].ty(&self.body.type_pool);
                ensure!(
                    left_type == right_type,
                    "Unsupported comparison operand types: {left_type:?} and {right_type:?}"
                );
                let operator = match (left_type, op) {
                    (Some(Type::F64), CompareOp::Eq | CompareOp::LooseEq) => Operator::F64Eq,
                    (Some(Type::F64), CompareOp::Ne | CompareOp::LooseNe) => Operator::F64Ne,
                    (Some(Type::F64), CompareOp::Lt) => Operator::F64Lt,
                    (Some(Type::F64), CompareOp::Le) => Operator::F64Le,
                    (Some(Type::F64), CompareOp::Gt) => Operator::F64Gt,
                    (Some(Type::F64), CompareOp::Ge) => Operator::F64Ge,
                    (Some(Type::I32), CompareOp::Eq | CompareOp::LooseEq) => Operator::I32Eq,
                    (Some(Type::I32), CompareOp::Ne | CompareOp::LooseNe) => Operator::I32Ne,
                    (Some(Type::I32), CompareOp::Lt) => Operator::I32LtU,
                    (Some(Type::I32), CompareOp::Le) => Operator::I32LeU,
                    (Some(Type::I32), CompareOp::Gt) => Operator::I32GtU,
                    (Some(Type::I32), CompareOp::Ge) => Operator::I32GeU,
                    _ => bail!(
                        "Unsupported comparison operand types: {left_type:?} and {right_type:?}"
                    ),
                };
                Ok(self.op(operator, &[left_val, right_val], &[Type::I32]))
            }
            _ => bail!("Unsupported condition expression: {expr:?}"),
        }
    }

    fn expression(&mut self, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::Number(n) => {
                Ok(self.op(Operator::F64Const { value: n.to_bits() }, &[], &[Type::F64]))
            }
            Expr::Integer(i) => Ok(self.op(
                Operator::F64Const {
                    value: (*i as f64).to_bits(),
                },
                &[],
                &[Type::F64],
            )),
            Expr::Bool(b) => {
                let v = if *b { 1 } else { 0 };
                Ok(self.op(Operator::I32Const { value: v }, &[], &[Type::I32]))
            }
            Expr::LocalGet(id) => self
                .locals
                .get(id)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("Uninitialized local {:?}", id)),
            Expr::Binary { op, left, right } => {
                let left_val = self.expression(left)?;
                let right_val = self.expression(right)?;
                let operator = match op {
                    BinaryOp::Add => Operator::F64Add,
                    BinaryOp::Sub => Operator::F64Sub,
                    BinaryOp::Mul => Operator::F64Mul,
                    BinaryOp::Div => Operator::F64Div,
                    _ => bail!("Unsupported binary operator: {op:?}"),
                };
                Ok(self.op(operator, &[left_val, right_val], &[Type::F64]))
            }
            Expr::Await(inner) => {
                let res = self.await_expression(inner, false)?;
                res.ok_or_else(|| anyhow::anyhow!("Await had no return value"))
            }
            Expr::Call { callee, args, .. } => {
                // Check if calling an intrinsic or module function
                if let Expr::ExternFuncRef { name, .. } = callee.as_ref() {
                    let &func_idx = self
                        .intrinsic_funcs
                        .get(name)
                        .ok_or_else(|| anyhow::anyhow!("Unknown extern function: {name}"))?;
                    let mut arg_vals = Vec::new();
                    for a in args {
                        arg_vals.push(self.expression(a)?);
                    }
                    let ret_types =
                        &self.module.signatures[self.module.funcs[func_idx].sig()].returns;
                    Ok(self.op(
                        Operator::Call {
                            function_index: func_idx,
                        },
                        &arg_vals,
                        ret_types,
                    ))
                } else if let Expr::FuncRef(fid) = callee.as_ref() {
                    let &func_idx = self
                        .func_decls
                        .get(fid)
                        .ok_or_else(|| anyhow::anyhow!("Unknown internal function id: {fid:?}"))?;
                    let is_exported = *self.func_is_exported.get(fid).unwrap_or(&false);

                    let mut arg_vals = Vec::new();
                    for a in args {
                        arg_vals.push(self.expression(a)?);
                    }

                    if !is_exported {
                        let call_val = self.body.add_op(
                            self.block,
                            Operator::Call {
                                function_index: func_idx,
                            },
                            &arg_vals,
                            &[Type::I32, Type::F64],
                        );
                        let status = self.body.add_value(ValueDef::PickOutput(
                            call_val,
                            0,
                            Type::I32,
                        ));
                        self.body.append_to_block(self.block, status);
                        let payload = self.body.add_value(ValueDef::PickOutput(
                            call_val,
                            1,
                            Type::F64,
                        ));
                        self.body.append_to_block(self.block, payload);

                        let is_ok = self.op(Operator::I32Eqz, &[status], &[Type::I32]);
                        let ok_block = self.body.add_block();
                        self.body.blocks[ok_block].desc = "call ok".into();
                        let err_block = self.body.add_block();
                        self.body.blocks[err_block].desc = "call err".into();

                        self.body.set_terminator(
                            self.block,
                            Terminator::CondBr {
                                cond: is_ok,
                                if_true: BlockTarget {
                                    block: ok_block,
                                    args: vec![],
                                },
                                if_false: BlockTarget {
                                    block: err_block,
                                    args: vec![],
                                },
                            },
                        );

                        self.block = err_block;
                        self.emit_throw(payload);

                        self.block = ok_block;
                        if self.is_func_return_bool(fid) {
                            let payload_i32 = self.op(
                                Operator::I32TruncF64U,
                                &[payload],
                                &[Type::I32],
                            );
                            Ok(payload_i32)
                        } else {
                            Ok(payload)
                        }
                    } else {
                        let ret_types =
                            &self.module.signatures[self.module.funcs[func_idx].sig()].returns;
                        Ok(self.op(
                            Operator::Call {
                                function_index: func_idx,
                            },
                            &arg_vals,
                            ret_types,
                        ))
                    }
                } else {
                    bail!("Unsupported call callee in WAFFLE lowering: {callee:?}");
                }
            }
            _ => bail!("Unsupported expression in WAFFLE lowering: {expr:?}"),
        }
    }

    fn cleanup_resources(&mut self) {
        if let (Some(stream_id), Some((drop, _))) =
            (self.stream_parameter, self.stream_helpers)
            && let Some(&stream_val) = self.locals.get(&stream_id)
        {
            self.op(
                Operator::Call {
                    function_index: drop,
                },
                &[stream_val],
                &[],
            );
        }
    }

    fn emit_throw(&mut self, err_val_f64: Value) {
        match self.unwind_ctx.target_for_throw() {
            UnwindTarget::Catch {
                block: catch_block,
                param: _param,
                scope_locals,
            } => {
                let mut args = vec![err_val_f64];
                for id in &scope_locals {
                    args.push(self.locals[id]);
                }
                self.branch(catch_block, args);
            }
            UnwindTarget::Finally {
                block: finally_block,
                scope_locals,
            } => {
                let reason_val = self.op(
                    Operator::I32Const {
                        value: ExitReason::Throw.tag(),
                    },
                    &[],
                    &[Type::I32],
                );
                let mut args = vec![reason_val, err_val_f64];
                for id in &scope_locals {
                    args.push(self.locals[id]);
                }
                self.branch(finally_block, args);
            }
            UnwindTarget::FunctionExit => {
                self.cleanup_resources();
                if self.is_exported {
                    if self.returns_wit_result {
                        let addr = self.op(
                            Operator::I32Const { value: 8 },
                            &[],
                            &[Type::I32],
                        );
                        let err_status = self.op(
                            Operator::I32Const { value: 1 },
                            &[],
                            &[Type::I32],
                        );
                        self.op(
                            Operator::I32Store {
                                memory: waffle::MemoryArg {
                                    align: 2,
                                    offset: 0,
                                    memory: self.memory,
                                },
                            },
                            &[addr, err_status],
                            &[],
                        );
                        self.op(
                            Operator::F64Store {
                                memory: waffle::MemoryArg {
                                    align: 3,
                                    offset: 8,
                                    memory: self.memory,
                                },
                            },
                            &[addr, err_val_f64],
                            &[],
                        );
                        self.body.set_terminator(
                            self.block,
                            Terminator::Return {
                                values: vec![addr],
                            },
                        );
                    } else {
                        self.body.set_terminator(self.block, Terminator::Unreachable);
                    }
                } else {
                    let err_status = self.op(
                        Operator::I32Const { value: 1 },
                        &[],
                        &[Type::I32],
                    );
                    self.body.set_terminator(
                        self.block,
                        Terminator::Return {
                            values: vec![err_status, err_val_f64],
                        },
                    );
                }
            }
        }
    }

    fn try_statement(
        &mut self,
        body: &[Stmt],
        catch: Option<&CatchClause>,
        finally: Option<&[Stmt]>,
    ) -> Result<()> {
        let incoming_scope_locals: Vec<LocalId> = self.locals.keys().copied().collect();
        let join_block = self.body.add_block();
        self.body.blocks[join_block].desc = "try-finally join".into();

        for &id in &incoming_scope_locals {
            let ty = self.body.values[self.locals[&id]]
                .ty(&self.body.type_pool)
                .unwrap();
            self.body.add_blockparam(join_block, ty);
        }

        let finally_block = if finally.is_some() {
            let fb = self.body.add_block();
            self.body.blocks[fb].desc = "finally entry".into();
            self.body.add_blockparam(fb, Type::I32); // exit_reason
            self.body.add_blockparam(fb, Type::F64); // payload
            for &id in &incoming_scope_locals {
                let ty = self.body.values[self.locals[&id]]
                    .ty(&self.body.type_pool)
                    .unwrap();
                self.body.add_blockparam(fb, ty);
            }
            Some(fb)
        } else {
            None
        };

        let catch_block = if catch.is_some() {
            let cb = self.body.add_block();
            self.body.blocks[cb].desc = "catch entry".into();
            self.body.add_blockparam(cb, Type::F64); // exception payload
            for &id in &incoming_scope_locals {
                let ty = self.body.values[self.locals[&id]]
                    .ty(&self.body.type_pool)
                    .unwrap();
                self.body.add_blockparam(cb, ty);
            }
            Some(cb)
        } else {
            None
        };

        let catch_param = catch.and_then(|c| c.param.as_ref().map(|(id, _)| *id));
        self.unwind_ctx.push_scope(TryScope {
            catch_target: catch_block,
            catch_param,
            finally_target: finally_block,
            scope_locals: incoming_scope_locals.clone(),
        });

        let mut join_reached = false;

        // 1. Lower try body
        self.statements(body)?;

        if self.body.blocks[self.block].terminator == Terminator::None {
            if let Some(fb) = finally_block {
                let zero_reason = self.op(
                    Operator::I32Const {
                        value: ExitReason::Normal.tag(),
                    },
                    &[],
                    &[Type::I32],
                );
                let zero_payload = self.op(
                    Operator::F64Const {
                        value: 0f64.to_bits(),
                    },
                    &[],
                    &[Type::F64],
                );
                let mut args = vec![zero_reason, zero_payload];
                for id in &incoming_scope_locals {
                    args.push(self.locals[id]);
                }
                self.branch(fb, args);
            } else {
                let args = incoming_scope_locals
                    .iter()
                    .map(|id| self.locals[id])
                    .collect();
                self.branch(join_block, args);
                join_reached = true;
            }
        }

        // 2. Lower catch clause (if present)
        if let (Some(cb), Some(c_clause)) = (catch_block, catch) {
            self.block = cb;
            self.unwind_ctx.clear_catch_in_innermost();

            let exc_val = self.body.blocks[cb].params[0].1;
            if let Some(param_id) = catch_param {
                self.locals.insert(param_id, exc_val);
            }
            for (idx, &id) in incoming_scope_locals.iter().enumerate() {
                let param_val = self.body.blocks[cb].params[idx + 1].1;
                self.locals.insert(id, param_val);
            }

            self.statements(&c_clause.body)?;

            if self.body.blocks[self.block].terminator == Terminator::None {
                if let Some(fb) = finally_block {
                    let zero_reason = self.op(
                        Operator::I32Const {
                            value: ExitReason::Normal.tag(),
                        },
                        &[],
                        &[Type::I32],
                    );
                    let zero_payload = self.op(
                        Operator::F64Const {
                            value: 0f64.to_bits(),
                        },
                        &[],
                        &[Type::F64],
                    );
                    let mut args = vec![zero_reason, zero_payload];
                    for id in &incoming_scope_locals {
                        args.push(self.locals[id]);
                    }
                    self.branch(fb, args);
                } else {
                    let args = incoming_scope_locals
                        .iter()
                        .map(|id| self.locals[id])
                        .collect();
                    self.branch(join_block, args);
                    join_reached = true;
                }
            }
        }

        // Pop try scope before lowering finally body
        self.unwind_ctx.pop_scope();

        // 3. Lower finally clause (if present)
        if let (Some(fb), Some(f_stmts)) = (finally_block, finally) {
            self.block = fb;
            let exit_reason = self.body.blocks[fb].params[0].1;
            let payload = self.body.blocks[fb].params[1].1;
            for (idx, &id) in incoming_scope_locals.iter().enumerate() {
                let param_val = self.body.blocks[fb].params[idx + 2].1;
                self.locals.insert(id, param_val);
            }

            self.statements(f_stmts)?;

            if self.body.blocks[self.block].terminator == Terminator::None {
                // Emit finally dispatcher
                let on_normal = self.body.add_block();
                self.body.blocks[on_normal].desc = "finally dispatch normal".into();
                let on_not_normal = self.body.add_block();
                self.body.blocks[on_not_normal].desc = "finally dispatch non-normal".into();

                let is_normal = self.op(Operator::I32Eqz, &[exit_reason], &[Type::I32]);
                self.body.set_terminator(
                    self.block,
                    Terminator::CondBr {
                        cond: is_normal,
                        if_true: BlockTarget {
                            block: on_normal,
                            args: vec![],
                        },
                        if_false: BlockTarget {
                            block: on_not_normal,
                            args: vec![],
                        },
                    },
                );

                // on_normal: branch to join_block
                self.block = on_normal;
                let join_args = incoming_scope_locals
                    .iter()
                    .map(|id| self.locals[id])
                    .collect();
                self.branch(join_block, join_args);
                join_reached = true;

                // on_not_normal: check return vs throw
                self.block = on_not_normal;
                let on_return = self.body.add_block();
                self.body.blocks[on_return].desc = "finally dispatch return".into();
                let on_throw = self.body.add_block();
                self.body.blocks[on_throw].desc = "finally dispatch throw".into();

                let one = self.op(Operator::I32Const { value: 1 }, &[], &[Type::I32]);
                let is_return = self.op(Operator::I32Eq, &[exit_reason, one], &[Type::I32]);
                self.body.set_terminator(
                    self.block,
                    Terminator::CondBr {
                        cond: is_return,
                        if_true: BlockTarget {
                            block: on_return,
                            args: vec![],
                        },
                        if_false: BlockTarget {
                            block: on_throw,
                            args: vec![],
                        },
                    },
                );

                // on_return:
                self.block = on_return;
                match self.unwind_ctx.target_for_return() {
                    ReturnTarget::Finally {
                        block: outer_finally,
                        scope_locals,
                    } => {
                        let reason_val = self.op(
                            Operator::I32Const {
                                value: ExitReason::Return.tag(),
                            },
                            &[],
                            &[Type::I32],
                        );
                        let mut args = vec![reason_val, payload];
                        for id in &scope_locals {
                            args.push(self.locals[id]);
                        }
                        self.branch(outer_finally, args);
                    }
                    ReturnTarget::FunctionExit => {
                        self.cleanup_resources();
                        if self.is_exported {
                            if self.returns_wit_result {
                                let addr = self.op(
                                    Operator::I32Const { value: 8 },
                                    &[],
                                    &[Type::I32],
                                );
                                let status_0 = self.op(
                                    Operator::I32Const { value: 0 },
                                    &[],
                                    &[Type::I32],
                                );
                                self.op(
                                    Operator::I32Store {
                                        memory: waffle::MemoryArg {
                                            align: 2,
                                            offset: 0,
                                            memory: self.memory,
                                        },
                                    },
                                    &[addr, status_0],
                                    &[],
                                );
                                self.op(
                                    Operator::F64Store {
                                        memory: waffle::MemoryArg {
                                            align: 3,
                                            offset: 8,
                                            memory: self.memory,
                                        },
                                    },
                                    &[addr, payload],
                                    &[],
                                );
                                self.body.set_terminator(
                                    self.block,
                                    Terminator::Return {
                                        values: vec![addr],
                                    },
                                );
                            } else {
                                let expected_ret_types = &self.body.rets;
                                let return_val = if expected_ret_types.len() == 1
                                    && expected_ret_types[0] == Type::I32
                                {
                                    self.op(Operator::I32TruncF64U, &[payload], &[Type::I32])
                                } else {
                                    payload
                                };
                                self.body.set_terminator(
                                    self.block,
                                    Terminator::Return {
                                        values: vec![return_val],
                                    },
                                );
                            }
                        } else {
                            let ok_status = self.op(
                                Operator::I32Const { value: 0 },
                                &[],
                                &[Type::I32],
                            );
                            self.body.set_terminator(
                                self.block,
                                Terminator::Return {
                                    values: vec![ok_status, payload],
                                },
                            );
                        }
                    }
                }

                // on_throw:
                self.block = on_throw;
                self.emit_throw(payload);
            }
        }

        // Set current block to join_block
        if !join_reached {
            self.body
                .set_terminator(join_block, Terminator::Unreachable);
        } else {
            for (idx, &id) in incoming_scope_locals.iter().enumerate() {
                let param_val = self.body.blocks[join_block].params[idx].1;
                self.locals.insert(id, param_val);
            }
        }
        self.block = join_block;

        Ok(())
    }

    fn block_parameters(
        &mut self,
        block: Block,
        bindings: &BTreeMap<LocalId, Value>,
    ) -> BTreeMap<LocalId, Value> {
        bindings
            .iter()
            .map(|(&id, &value)| {
                let ty = self.body.values[value]
                    .ty(&self.body.type_pool)
                    .expect("Bindings have one primitive type");
                (id, self.body.add_blockparam(block, ty))
            })
            .collect()
    }

    fn branch(&mut self, block: Block, args: Vec<Value>) {
        self.body.set_terminator(
            self.block,
            Terminator::Br {
                target: BlockTarget { block, args },
            },
        );
    }

    fn op(&mut self, operator: Operator, args: &[Value], returns: &[Type]) -> Value {
        self.body.add_op(self.block, operator, args, returns)
    }
}
