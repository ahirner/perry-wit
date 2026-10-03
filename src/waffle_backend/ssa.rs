//! Direct Perry HIR → WAFFLE SSA lowering.
//!
//! Lowers assignments, branches, loops, function calls, and returns directly to
//! typed basic blocks, SSA values, and block parameters without linear memory
//! overhead for primitive values.

use std::collections::BTreeMap;

use anyhow::{Result, bail, ensure};
use perry_hir::ir::{BinaryOp, CatchClause, CompareOp, Expr, Function, Module as HirModule, Stmt};
use perry_hir::types::{LocalId, Type as HirType};
use waffle::{
    Block, BlockTarget, Export, ExportKind, FunctionBody, Module,
    Operator, Terminator, Type, Value,
};

use crate::waffle_backend::abi;
use crate::waffle_backend::exceptions::{
    ExitReason, ReturnTarget, TryScope, UnwindContext, UnwindTarget,
};
use crate::waffle_backend::registry::{CallingConvention, FunctionInfo, ModuleRegistry};
use crate::waffle_backend::resolve::ResolvedContract;

/// Compiles a resolved HIR module into a validated WAFFLE module.
pub(crate) fn lower_module(
    hir: &HirModule,
    contract: &ResolvedContract,
) -> Result<Module<'static>> {
    let mut module = Module::empty();

    // 1. Declare and export linear memory for Canonical ABI options and component framing
    let memory = module.memories.push(waffle::MemoryData {
        initial_pages: 1,
        maximum_pages: None,
        segments: vec![],
    });
    module.exports.push(Export {
        name: "memory".to_string(),
        kind: ExportKind::Memory(memory),
    });

    // 2. Build complete module declarations registry
    let registry = ModuleRegistry::build(&mut module, hir, contract, None, memory)?;

    // 3. Lower each function body using the established registry contracts
    for func in &hir.functions {
        let info = &registry.functions[&func.id];
        let body = lower_function_body(func, info, &registry, &module, contract)?;
        module.funcs[info.func_index] = waffle::FuncDecl::Body(info.sig, func.name.clone(), body);

        if let Some(export_name) = &info.export_name {
            module.exports.push(Export {
                name: export_name.clone(),
                kind: ExportKind::Func(info.func_index),
            });
        }
    }

    Ok(module)
}

/// Function body lowerer.
struct FunctionLowerer<'a> {
    module: &'a Module<'static>,
    registry: &'a ModuleRegistry,
    current_func: &'a FunctionInfo,
    _contract: &'a ResolvedContract,
    body: FunctionBody,
    block: Block,
    locals: BTreeMap<LocalId, Value>,
    stream_parameter: Option<LocalId>,
    awaited_calls: usize,
    unwind_ctx: UnwindContext,
}

fn lower_function_body(
    func: &Function,
    info: &FunctionInfo,
    registry: &ModuleRegistry,
    module: &Module<'static>,
    contract: &ResolvedContract,
) -> Result<FunctionBody> {
    let body = FunctionBody::new(module, info.sig);
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

    let mut lowerer = FunctionLowerer {
        module,
        registry,
        current_func: info,
        _contract: contract,
        body,
        block: entry,
        locals,
        stream_parameter,
        awaited_calls: 0,
        unwind_ctx: UnwindContext::new(),
    };

    if let Some((_, reset)) = registry.stream_helpers {
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
        let expected_rets = &module.signatures[info.sig].returns;
        abi::emit_function_return(
            &mut lowerer.body,
            lowerer.block,
            registry.memory,
            info.calling_convention,
            expected_rets,
            None,
        );
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
                            let payload = abi::encode_payload(&mut self.body, self.block, ret_val);
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
                            let expected_rets =
                                &self.module.signatures[self.current_func.sig].returns;
                            abi::emit_function_return(
                                &mut self.body,
                                self.block,
                                self.registry.memory,
                                self.current_func.calling_convention,
                                expected_rets,
                                ret_val,
                            );
                        }
                    }
                }
                Stmt::Throw(expr) => {
                    let err_val = self.expression(expr)?;
                    let err_val_f64 = abi::encode_payload(&mut self.body, self.block, Some(err_val));
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
                let &intrinsic_func = self
                    .registry
                    .intrinsics
                    .get(name)
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
                let callee_info = self
                    .registry
                    .functions
                    .get(fid)
                    .ok_or_else(|| anyhow::anyhow!("Unknown internal function id: {fid:?}"))?;

                let mut arg_vals = Vec::new();
                for a in args {
                    arg_vals.push(self.expression(a)?);
                }

                match callee_info.calling_convention {
                    CallingConvention::Internal => {
                        let outcome = abi::emit_internal_call(
                            &mut self.body,
                            self.block,
                            callee_info,
                            &arg_vals,
                        );

                        // Rejection enters guest exception path at the await
                        self.block = outcome.err_block;
                        self.emit_throw(outcome.payload);

                        self.block = outcome.ok_block;
                        let result_val = if is_statement {
                            None
                        } else {
                            Some(abi::decode_payload(
                                &mut self.body,
                                self.block,
                                outcome.payload,
                                callee_info.is_boolean_return(),
                            ))
                        };
                        Ok(self.continuation(result_val))
                    }
                    CallingConvention::ExportedDirect | CallingConvention::ExportedWitResult => {
                        let ret_types = &self.module.signatures[callee_info.sig].returns;
                        let returns = ret_types.clone();
                        let call_res = self.op(
                            Operator::Call {
                                function_index: callee_info.func_index,
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
                        .registry
                        .intrinsics
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
                    let callee_info = self
                        .registry
                        .functions
                        .get(fid)
                        .ok_or_else(|| anyhow::anyhow!("Unknown internal function id: {fid:?}"))?;

                    let mut arg_vals = Vec::new();
                    for a in args {
                        arg_vals.push(self.expression(a)?);
                    }

                    match callee_info.calling_convention {
                        CallingConvention::Internal => {
                            let outcome = abi::emit_internal_call(
                                &mut self.body,
                                self.block,
                                callee_info,
                                &arg_vals,
                            );

                            self.block = outcome.err_block;
                            self.emit_throw(outcome.payload);

                            self.block = outcome.ok_block;
                            let return_val = abi::decode_payload(
                                &mut self.body,
                                self.block,
                                outcome.payload,
                                callee_info.is_boolean_return(),
                            );
                            Ok(return_val)
                        }
                        CallingConvention::ExportedDirect | CallingConvention::ExportedWitResult => {
                            let ret_types =
                                &self.module.signatures[callee_info.sig].returns;
                            Ok(self.op(
                                Operator::Call {
                                    function_index: callee_info.func_index,
                                },
                                &arg_vals,
                                ret_types,
                            ))
                        }
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
            (self.stream_parameter, self.registry.stream_helpers)
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
                abi::emit_function_throw(
                    &mut self.body,
                    self.block,
                    self.registry.memory,
                    self.current_func.calling_convention,
                    err_val_f64,
                );
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
                        let expected_rets =
                            &self.module.signatures[self.current_func.sig].returns;
                        abi::emit_function_return(
                            &mut self.body,
                            self.block,
                            self.registry.memory,
                            self.current_func.calling_convention,
                            expected_rets,
                            Some(payload),
                        );
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
