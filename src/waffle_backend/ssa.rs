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
    Block, BlockTarget, Export, ExportKind, FunctionBody, Module, Operator, Terminator, Type, Value,
};

use crate::waffle_backend::abi;
use crate::waffle_backend::control_flow::{JoinPoint, create_block_parameters};
use crate::waffle_backend::exceptions::{self, TryClauseBlocks, TryScope, UnwindContext};
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

    // If stream parameter is present and an initialize helper exists, call it at entry
    if let (Some(stream_id), Some((_, init))) = (stream_parameter, registry.stream_helpers) {
        let stream_val = lowerer.locals[&stream_id];
        lowerer.op(
            Operator::Call {
                function_index: init,
            },
            &[stream_val],
            &[],
        );
    } else if stream_parameter.is_some() {
        // Fallback dummy op
        lowerer.op(Operator::I32Const { value: 0 }, &[], &[]);
    }

    lowerer.statements(&func.body)?;

    // If the block is not terminated, emit default return or ensure proper termination
    if lowerer.body.blocks[lowerer.block].terminator == Terminator::None {
        lowerer.emit_return(None);
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
                    self.emit_return(ret_val);
                }
                Stmt::Throw(expr) => {
                    let err_val = self.expression(expr)?;
                    ensure!(
                        self.body.values[err_val].ty(&self.body.type_pool) == Some(Type::F64),
                        "Only numeric thrown payloads are supported until the exception ABI preserves primitive type tags"
                    );
                    self.emit_throw(err_val);
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
        let join = JoinPoint::new(&mut self.body, "branch join", &incoming_locals);

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

        // Lower then branch (self.locals is already incoming_locals)
        self.block = then_block;
        self.statements(then_branch)?;
        if self.body.blocks[self.block].terminator == Terminator::None {
            join.emit_branch(&mut self.body, self.block, &self.locals);
        }

        // Lower else branch (move incoming_locals)
        self.block = else_block;
        self.locals = incoming_locals;
        self.statements(else_branch)?;
        if self.body.blocks[self.block].terminator == Terminator::None {
            join.emit_branch(&mut self.body, self.block, &self.locals);
        }

        if self.body.blocks[join.block].preds.is_empty() {
            self.body
                .set_terminator(join.block, Terminator::Unreachable);
        }
        self.block = join.block;
        self.locals = join.bindings;
        Ok(())
    }

    fn while_loop(&mut self, condition: &Expr, body: &[Stmt]) -> Result<()> {
        let header = JoinPoint::new(&mut self.body, "loop header", &self.locals);
        header.emit_branch(&mut self.body, self.block, &self.locals);

        self.block = header.block;
        self.locals = header.bindings.clone();

        let cond_val = self.condition(condition)?;
        let body_block = self.body.add_block();
        let exit = JoinPoint::new(&mut self.body, "loop exit", &self.locals);

        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: cond_val,
                if_true: BlockTarget {
                    block: body_block,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: exit.block,
                    args: exit.branch_args(&self.locals),
                },
            },
        );

        self.block = body_block;
        self.statements(body)?;
        if self.body.blocks[self.block].terminator == Terminator::None {
            header.emit_branch(&mut self.body, self.block, &self.locals);
        }

        self.block = exit.block;
        self.locals = exit.bindings;
        Ok(())
    }

    fn call_operation(&mut self, callee: &Expr, args: &[Expr]) -> Result<Option<Value>> {
        let mut arg_vals = Vec::with_capacity(args.len());
        for a in args {
            arg_vals.push(self.expression(a)?);
        }

        if let Expr::ExternFuncRef { name, .. } = callee {
            let &func_idx = self
                .registry
                .intrinsics
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("Unknown extern function: {name}"))?;
            let ret_types = &self.module.signatures[self.module.funcs[func_idx].sig()].returns;
            if ret_types.is_empty() {
                self.op(
                    Operator::Call {
                        function_index: func_idx,
                    },
                    &arg_vals,
                    &[],
                );
                Ok(None)
            } else {
                let call_res = self.op(
                    Operator::Call {
                        function_index: func_idx,
                    },
                    &arg_vals,
                    ret_types,
                );
                Ok(Some(call_res))
            }
        } else if let Expr::FuncRef(fid) = callee {
            let callee_info = self
                .registry
                .functions
                .get(fid)
                .ok_or_else(|| anyhow::anyhow!("Unknown internal function id: {fid:?}"))?;

            match callee_info.calling_convention {
                CallingConvention::Internal => {
                    let outcome =
                        abi::emit_internal_call(&mut self.body, self.block, callee_info, &arg_vals);

                    self.block = outcome.err_block;
                    self.emit_throw(outcome.payload);

                    self.block = outcome.ok_block;
                    if callee_info.return_type == HirType::Void {
                        Ok(None)
                    } else {
                        let return_val = abi::decode_payload(
                            &mut self.body,
                            self.block,
                            outcome.payload,
                            callee_info.is_boolean_return(),
                        );
                        Ok(Some(return_val))
                    }
                }
                CallingConvention::ExportedDirect | CallingConvention::ExportedWitResult { .. } => {
                    let ret_types = &self.module.signatures[callee_info.sig].returns;
                    if ret_types.is_empty() {
                        self.op(
                            Operator::Call {
                                function_index: callee_info.func_index,
                            },
                            &arg_vals,
                            &[],
                        );
                        Ok(None)
                    } else {
                        let call_res = self.op(
                            Operator::Call {
                                function_index: callee_info.func_index,
                            },
                            &arg_vals,
                            ret_types,
                        );
                        Ok(Some(call_res))
                    }
                }
            }
        } else {
            bail!("Unsupported call callee in WAFFLE lowering: {callee:?}");
        }
    }

    fn await_expression(&mut self, expr: &Expr, is_statement: bool) -> Result<Option<Value>> {
        let result_val = match expr {
            Expr::Call { callee, args, .. } => {
                let res = self.call_operation(callee, args)?;
                if is_statement { None } else { res }
            }
            _ => {
                let val = self.expression(expr)?;
                if is_statement { None } else { Some(val) }
            }
        };
        Ok(self.continuation(result_val))
    }

    fn continuation(&mut self, result: Option<Value>) -> Option<Value> {
        self.awaited_calls += 1;
        let resumed = self.body.add_block();
        self.body.blocks[resumed].desc = "await continuation".into();
        let resumed_locals = create_block_parameters(&mut self.body, resumed, &self.locals);
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
            _ => {
                let val = self.expression(expr)?;
                let ty = self.body.values[val].ty(&self.body.type_pool);
                if ty == Some(Type::I32) {
                    Ok(val)
                } else {
                    let zero = self.op(
                        Operator::F64Const {
                            value: 0f64.to_bits(),
                        },
                        &[],
                        &[Type::F64],
                    );
                    let nonzero = self.op(Operator::F64Ne, &[val, zero], &[Type::I32]);
                    let not_nan = self.op(Operator::F64Eq, &[val, val], &[Type::I32]);
                    Ok(self.op(Operator::I32And, &[nonzero, not_nan], &[Type::I32]))
                }
            }
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
                let res = self.call_operation(callee, args)?;
                Ok(res.unwrap_or_else(|| {
                    self.op(
                        Operator::F64Const {
                            value: 0f64.to_bits(),
                        },
                        &[],
                        &[Type::F64],
                    )
                }))
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

    fn emit_terminal_return(&mut self, ret_val: Option<Value>) {
        self.cleanup_resources();
        let expected_rets = &self.module.signatures[self.current_func.sig].returns;
        abi::emit_function_return(
            &mut self.body,
            self.block,
            self.registry.memory,
            self.current_func.calling_convention,
            expected_rets,
            ret_val,
        );
    }

    fn emit_terminal_throw(&mut self, err_val_f64: Value) {
        self.cleanup_resources();
        abi::emit_function_throw(
            &mut self.body,
            self.block,
            self.registry.memory,
            self.current_func.calling_convention,
            err_val_f64,
        );
    }

    fn emit_return(&mut self, ret_val: Option<Value>) {
        let payload = abi::encode_payload(&mut self.body, self.block, ret_val);
        if exceptions::route_return(
            &mut self.body,
            self.block,
            &self.unwind_ctx,
            &self.locals,
            payload,
        ) {
            self.emit_terminal_return(ret_val);
        }
    }

    fn emit_finally_return(&mut self, payload: Value) {
        if exceptions::route_return(
            &mut self.body,
            self.block,
            &self.unwind_ctx,
            &self.locals,
            payload,
        ) {
            self.emit_terminal_return(Some(payload));
        }
    }

    fn emit_throw(&mut self, err_val_f64: Value) {
        if exceptions::route_throw(
            &mut self.body,
            self.block,
            &self.unwind_ctx,
            &self.locals,
            err_val_f64,
        ) {
            self.emit_terminal_throw(err_val_f64);
        }
    }

    fn try_statement(
        &mut self,
        body: &[Stmt],
        catch: Option<&CatchClause>,
        finally: Option<&[Stmt]>,
    ) -> Result<()> {
        let blocks = TryClauseBlocks::build(
            &mut self.body,
            &self.locals,
            catch.is_some(),
            catch.and_then(|c| c.param.as_ref().map(|(id, _)| *id)),
            finally.is_some(),
        );

        self.unwind_ctx.push_scope(TryScope {
            catch_target: blocks.catch_block,
            catch_param: blocks.catch_param,
            finally_target: blocks.finally_block,
            scope_locals: blocks.scope_locals.clone(),
        });

        let mut join_reached = false;

        // 1. Lower try body
        self.statements(body)?;
        if self.body.blocks[self.block].terminator == Terminator::None {
            join_reached |= blocks.emit_normal_transition(&mut self.body, self.block, &self.locals);
        }

        // 2. Lower catch clause (if present)
        if let (Some(cb), Some(c_clause)) = (blocks.catch_block, catch) {
            self.block = cb;
            self.unwind_ctx.clear_catch_in_innermost();
            self.locals = blocks.catch_environment(&self.body);

            self.statements(&c_clause.body)?;
            if self.body.blocks[self.block].terminator == Terminator::None {
                join_reached |=
                    blocks.emit_normal_transition(&mut self.body, self.block, &self.locals);
            }
        }

        // Pop try scope before lowering finally body
        self.unwind_ctx.pop_scope();

        // 3. Lower finally clause (if present)
        if let (Some(fb), Some(f_stmts)) = (blocks.finally_block, finally) {
            self.block = fb;
            let environment = blocks.finally_environment(&self.body);
            self.locals = environment.locals;

            self.statements(f_stmts)?;

            if self.body.blocks[self.block].terminator == Terminator::None {
                let (on_return, on_throw) = exceptions::emit_finally_dispatcher(
                    &mut self.body,
                    self.block,
                    environment.exit_reason,
                    blocks.join_block,
                    &blocks.scope_locals,
                    &self.locals,
                );
                join_reached = true;

                // on_return:
                self.block = on_return;
                self.emit_finally_return(environment.payload);

                // on_throw:
                self.block = on_throw;
                self.emit_throw(environment.payload);
            }
        }

        // 4. Join phase
        if !join_reached {
            self.body
                .set_terminator(blocks.join_block, Terminator::Unreachable);
        } else {
            self.locals = blocks.join_environment(&self.body);
        }
        self.block = blocks.join_block;
        Ok(())
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
