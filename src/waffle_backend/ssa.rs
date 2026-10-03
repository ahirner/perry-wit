//! Direct Perry HIR → WAFFLE SSA lowering.
//!
//! Lowers assignments, branches, loops, function calls, and returns directly to
//! typed basic blocks, SSA values, and block parameters without linear memory
//! overhead for primitive values.

mod arrays;
mod string_ops;
mod types;

use std::collections::BTreeMap;

use anyhow::{Result, bail, ensure};
use perry_hir::ir::{BinaryOp, CatchClause, CompareOp, Expr, Function, Module as HirModule, Stmt};
use perry_hir::types::{LocalId, Type as HirType};
use waffle::{
    Block, BlockTarget, Export, ExportKind, FunctionBody, MemoryArg, Module, Operator, Terminator,
    Type, Value,
};

use crate::waffle_backend::abi::{self, CompletionStatus};
use crate::waffle_backend::control_flow::{JoinPoint, create_block_parameters};
use crate::waffle_backend::exceptions::{self, TryClauseBlocks, TryScope, UnwindContext};
use crate::waffle_backend::registry::{FunctionInfo, ModuleRegistry};
use crate::waffle_backend::resolve::ResolvedContract;
use crate::waffle_backend::strings::{RequiredStringHelpers, StringPool};

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

    // 2. Scan module for string requirements and build string pool if needed
    let reqs = scan_module_string_requirements(hir);
    let mut string_pool = StringPool::new();
    let string_heap_base = if reqs.needs_strings {
        collect_strings_in_module(hir, &mut string_pool);
        string_pool.populate_memory_segments(&mut module.memories[memory]);
        let next_free = string_pool.next_free_address();
        let needs_helper_library = reqs.find_substring
            || reqs.code_point_at
            || reqs.from_code_point
            || reqs.case_convert
            || reqs.split
            || reqs.join;
        if needs_helper_library {
            let raw_base = next_free + 65_536 + 4096;
            let aligned_heap_base = (raw_base + 65_535) & !65_535;
            let needed_pages = (aligned_heap_base / 65_536) as usize + 1;
            if module.memories[memory].initial_pages < needed_pages {
                module.memories[memory].initial_pages = needed_pages;
            }
            Some(aligned_heap_base)
        } else {
            Some(next_free)
        }
    } else {
        None
    };

    // 3. Build complete module declarations registry
    let registry = ModuleRegistry::build(
        &mut module,
        hir,
        contract,
        None,
        string_heap_base,
        memory,
        reqs,
    )?;

    // 5. Lower each function body using the established registry contracts
    for func in &hir.functions {
        let info = &registry.functions[&func.id];
        let body = lower_function_body(func, info, &registry, &module, &string_pool, contract)?;
        module.funcs[info.func_index] = waffle::FuncDecl::Body(info.sig, func.name.clone(), body);

        if let Some(export) = &info.export {
            let lift_fn = registry.string_helpers.as_ref().map(|h| h.lift_canonical);
            let wrapper =
                abi::build_export_wrapper(&module, info, export, registry.memory, lift_fn)?;
            module.funcs[export.func_index] =
                waffle::FuncDecl::Body(export.sig, format!("{}.export", export.name), wrapper);
            module.exports.push(Export {
                name: export.name.clone(),
                kind: ExportKind::Func(export.func_index),
            });
        }
    }

    Ok(module)
}

/// Function body lowerer.
struct FunctionLowerer<'a> {
    module: &'a Module<'static>,
    registry: &'a ModuleRegistry,
    _contract: &'a ResolvedContract,
    string_pool: &'a StringPool,
    return_type: &'a HirType,
    body: FunctionBody,
    block: Block,
    locals: BTreeMap<LocalId, Value>,
    local_types: BTreeMap<LocalId, HirType>,
    stream_parameter: Option<LocalId>,
    awaited_calls: usize,
    unwind_ctx: UnwindContext,
}

fn lower_function_body(
    func: &Function,
    info: &FunctionInfo,
    registry: &ModuleRegistry,
    module: &Module<'static>,
    string_pool: &StringPool,
    contract: &ResolvedContract,
) -> Result<FunctionBody> {
    let body = FunctionBody::new(module, info.sig);
    let entry = body.entry;
    let mut locals = BTreeMap::new();
    let mut local_types = BTreeMap::new();
    let mut stream_parameter = None;

    // Map entry block parameters to function parameters
    for (i, param) in func.params.iter().enumerate() {
        let val = body.blocks[entry].params[i].1;
        locals.insert(param.id, val);
        local_types.insert(param.id, param.ty.clone());
        if matches!(&param.ty, HirType::Named(n) if n == "ByteStream") {
            stream_parameter = Some(param.id);
        }
    }

    let mut lowerer = FunctionLowerer {
        module,
        registry,
        _contract: contract,
        string_pool,
        return_type: info.success_type(),
        body,
        block: entry,
        locals,
        local_types,
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
                    let inferred = self.infer_expr_type(expr);
                    self.local_types.insert(*id, inferred);
                    let val = self.expression(expr)?;
                    ensure!(
                        self.locals.insert(*id, val).is_none(),
                        "Duplicate local binding id: {:?}",
                        id
                    );
                }
                Stmt::Expr(Expr::LocalSet(id, expr)) => {
                    let inferred = self.infer_expr_type(expr);
                    self.local_types.insert(*id, inferred);
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
                        .map(|expr| {
                            if self.return_type == &HirType::String {
                                self.string_receiver(expr)
                            } else {
                                ensure!(
                                    !self.is_string_or_undefined(expr)
                                        || self.return_type == &HirType::Void,
                                    "Cannot return a string-or-undefined value as {:?}",
                                    self.return_type
                                );
                                self.expression(expr)
                            }
                        })
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
        if let Expr::PropertyGet {
            object, property, ..
        } = callee
        {
            if property == "join" {
                return self.array_join(object, args).map(Some);
            }
            return self.string_method(object, property, args).map(Some);
        }

        let mut arg_vals = Vec::with_capacity(args.len());
        for (index, arg) in args.iter().enumerate() {
            let expected = match callee {
                Expr::FuncRef(fid) => self
                    .registry
                    .functions
                    .get(fid)
                    .and_then(|info| info.param_types.get(index)),
                _ => None,
            };
            let value = if expected == Some(&HirType::String) {
                self.string_receiver(arg)?
            } else {
                ensure!(
                    !matches!(self.infer_expr_type(arg), HirType::Union(_) | HirType::Void),
                    "String-or-undefined arguments require a string parameter"
                );
                self.expression(arg)?
            };
            arg_vals.push(value);
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

            let outcome =
                abi::emit_internal_call(&mut self.body, self.block, callee_info, &arg_vals);
            self.block = outcome.err_block;
            self.emit_throw(outcome.payload);
            self.block = outcome.ok_block;
            if matches!(callee_info.success_type(), HirType::Void) {
                Ok(None)
            } else {
                let return_val = abi::decode_payload(
                    &mut self.body,
                    self.block,
                    outcome.payload,
                    matches!(
                        callee_info.success_type(),
                        HirType::Boolean | HirType::String
                    ),
                );
                Ok(Some(return_val))
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
            Expr::Compare { op, left, right }
                if self.is_string_or_undefined(left) || self.is_string_or_undefined(right) =>
            {
                self.string_comparison(*op, left, right)
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
                if self.is_string(expr) {
                    Ok(self.string_truthiness(val))
                } else if ty == Some(Type::I32) {
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
            Expr::String(s) => {
                let offset = self.string_pool.get(s).unwrap_or_else(|| {
                    panic!("String literal {s:?} was not interned in string pool")
                });
                Ok(self.op(Operator::I32Const { value: offset }, &[], &[Type::I32]))
            }
            Expr::TemplateStringCoerce(inner) | Expr::StringCoerce(inner) => {
                self.string_operand(inner)
            }
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
            Expr::Undefined => Ok(self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32])),
            Expr::LocalGet(id) => self
                .locals
                .get(id)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("Uninitialized local {:?}", id)),
            Expr::StringFromCodePoint(arg) => {
                let cp = self.expression(arg)?;
                let helpers = self
                    .registry
                    .string_helpers
                    .as_ref()
                    .expect("string helpers available");
                let func = helpers
                    .str_from_code_point
                    .expect("from_code_point helper available");
                Ok(self.op(
                    Operator::Call {
                        function_index: func,
                    },
                    &[cp],
                    &[Type::I32],
                ))
            }
            Expr::PropertyGet {
                object, property, ..
            } if property == "length" => {
                if self.is_string(object) {
                    let desc = self.string_receiver(object)?;
                    let scalar_len = self.string_length(desc);
                    Ok(self.op(Operator::F64ConvertI32U, &[scalar_len], &[Type::F64]))
                } else {
                    let arr_ptr = self.expression(object)?;
                    let count = self.op(
                        Operator::I32Load {
                            memory: MemoryArg {
                                align: 2,
                                offset: 4,
                                memory: self.registry.memory,
                            },
                        },
                        &[arr_ptr],
                        &[Type::I32],
                    );
                    Ok(self.op(Operator::F64ConvertI32U, &[count], &[Type::F64]))
                }
            }
            Expr::IndexGet { object, index, .. } => {
                if self.is_string(object) {
                    let desc = self.string_receiver(object)?;
                    let idx = self.position_argument(Some(index), f64::NAN)?;
                    let helpers = self
                        .registry
                        .string_helpers
                        .as_ref()
                        .expect("string helpers available");
                    Ok(self.op(
                        Operator::Call {
                            function_index: helpers.str_index,
                        },
                        &[desc, idx],
                        &[Type::I32],
                    ))
                } else {
                    self.array_index(object, index)
                }
            }
            Expr::Compare { .. } => self.condition(expr),
            Expr::Binary { op, left, right }
                if *op == BinaryOp::Add && (self.is_string(left) || self.is_string(right)) =>
            {
                let left_val = self.string_operand(left)?;
                let right_val = self.string_operand(right)?;
                let helpers = self
                    .registry
                    .string_helpers
                    .as_ref()
                    .expect("string helpers available");
                Ok(self.op(
                    Operator::Call {
                        function_index: helpers.str_concat,
                    },
                    &[left_val, right_val],
                    &[Type::I32],
                ))
            }
            Expr::Binary { op, left, right } => {
                let left_val = self.expression(left)?;
                let right_val = self.expression(right)?;
                ensure!(
                    [left_val, right_val]
                        .into_iter()
                        .all(|value| self.body.values[value].ty(&self.body.type_pool)
                            == Some(Type::F64)),
                    "Arithmetic operands must be numeric"
                );
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
        let payload = abi::encode_payload(&mut self.body, self.block, ret_val);
        abi::emit_completion(
            &mut self.body,
            self.block,
            CompletionStatus::Returned,
            payload,
        );
    }

    fn emit_terminal_throw(&mut self, err_val_f64: Value) {
        self.cleanup_resources();
        abi::emit_completion(
            &mut self.body,
            self.block,
            CompletionStatus::Threw,
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

fn collect_strings_in_module(hir: &HirModule, pool: &mut StringPool) {
    pool.intern("");
    pool.intern(",");
    for func in &hir.functions {
        for stmt in &func.body {
            collect_strings_stmt(stmt, pool);
        }
    }
    for stmt in &hir.init {
        collect_strings_stmt(stmt, pool);
    }
}

fn collect_strings_stmt(stmt: &Stmt, pool: &mut StringPool) {
    match stmt {
        Stmt::Expr(expr) | Stmt::Throw(expr) => collect_strings_expr(expr, pool),
        Stmt::Return(Some(expr)) => collect_strings_expr(expr, pool),
        Stmt::Let {
            init: Some(expr), ..
        } => collect_strings_expr(expr, pool),
        Stmt::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_strings_expr(condition, pool);
            for s in then_branch {
                collect_strings_stmt(s, pool);
            }
            if let Some(eb) = else_branch {
                for s in eb {
                    collect_strings_stmt(s, pool);
                }
            }
        }
        Stmt::While { condition, body } => {
            collect_strings_expr(condition, pool);
            for s in body {
                collect_strings_stmt(s, pool);
            }
        }
        Stmt::Try {
            body,
            catch,
            finally,
        } => {
            for s in body {
                collect_strings_stmt(s, pool);
            }
            if let Some(c) = catch {
                for s in &c.body {
                    collect_strings_stmt(s, pool);
                }
            }
            if let Some(f) = finally {
                for s in f {
                    collect_strings_stmt(s, pool);
                }
            }
        }
        _ => {}
    }
}

fn collect_strings_expr(expr: &Expr, pool: &mut StringPool) {
    match expr {
        Expr::String(s) => {
            pool.intern(s);
        }
        Expr::Binary { left, right, .. } | Expr::Compare { left, right, .. } => {
            collect_strings_expr(left, pool);
            collect_strings_expr(right, pool);
        }
        Expr::Unary { operand, .. } => {
            collect_strings_expr(operand, pool);
        }
        Expr::Await(expr) | Expr::TemplateStringCoerce(expr) | Expr::StringFromCodePoint(expr) => {
            collect_strings_expr(expr, pool);
        }
        Expr::Call { callee, args, .. } => {
            collect_strings_expr(callee, pool);
            for arg in args {
                collect_strings_expr(arg, pool);
            }
        }
        Expr::PropertyGet { object, .. } => {
            collect_strings_expr(object, pool);
        }
        Expr::IndexGet { object, index, .. } => {
            collect_strings_expr(object, pool);
            collect_strings_expr(index, pool);
        }
        Expr::LocalSet(_, expr) => {
            collect_strings_expr(expr, pool);
        }
        _ => {}
    }
}

fn type_has_string(ty: &HirType) -> bool {
    match ty {
        HirType::String => true,
        HirType::Promise(inner) => type_has_string(inner),
        HirType::Array(inner) => type_has_string(inner),
        HirType::Union(types) => types.iter().any(type_has_string),
        _ => false,
    }
}

fn scan_module_string_requirements(hir: &HirModule) -> RequiredStringHelpers {
    let mut reqs = RequiredStringHelpers::default();

    for func in &hir.functions {
        for param in &func.params {
            if type_has_string(&param.ty) {
                reqs.needs_strings = true;
            }
        }
        if type_has_string(&func.return_type) {
            reqs.needs_strings = true;
        }
        for stmt in &func.body {
            scan_stmt_requirements(stmt, &mut reqs);
        }
    }
    for stmt in &hir.init {
        scan_stmt_requirements(stmt, &mut reqs);
    }

    reqs
}

fn scan_stmt_requirements(stmt: &Stmt, reqs: &mut RequiredStringHelpers) {
    match stmt {
        Stmt::Expr(expr) | Stmt::Throw(expr) => scan_expr_requirements(expr, reqs),
        Stmt::Return(Some(expr)) => scan_expr_requirements(expr, reqs),
        Stmt::Let {
            init: Some(expr), ..
        } => scan_expr_requirements(expr, reqs),
        Stmt::If {
            condition,
            then_branch,
            else_branch,
        } => {
            scan_expr_requirements(condition, reqs);
            for s in then_branch {
                scan_stmt_requirements(s, reqs);
            }
            if let Some(eb) = else_branch {
                for s in eb {
                    scan_stmt_requirements(s, reqs);
                }
            }
        }
        Stmt::While { condition, body } => {
            scan_expr_requirements(condition, reqs);
            for s in body {
                scan_stmt_requirements(s, reqs);
            }
        }
        Stmt::Try {
            body,
            catch,
            finally,
        } => {
            for s in body {
                scan_stmt_requirements(s, reqs);
            }
            if let Some(c) = catch {
                for s in &c.body {
                    scan_stmt_requirements(s, reqs);
                }
            }
            if let Some(f) = finally {
                for s in f {
                    scan_stmt_requirements(s, reqs);
                }
            }
        }
        _ => {}
    }
}

fn scan_expr_requirements(expr: &Expr, reqs: &mut RequiredStringHelpers) {
    match expr {
        Expr::String(_) => {
            reqs.needs_strings = true;
        }
        Expr::StringFromCodePoint(arg) => {
            reqs.needs_strings = true;
            reqs.from_code_point = true;
            scan_expr_requirements(arg, reqs);
        }
        Expr::TemplateStringCoerce(arg) => {
            reqs.needs_strings = true;
            scan_expr_requirements(arg, reqs);
        }
        Expr::Call { callee, args, .. } => {
            if let Expr::PropertyGet { property, .. } = callee.as_ref() {
                match property.as_str() {
                    "indexOf" => {
                        reqs.needs_strings = true;
                        reqs.find_substring = true;
                    }
                    "codePointAt" => {
                        reqs.needs_strings = true;
                        reqs.code_point_at = true;
                    }
                    "toLowerCase" | "toUpperCase" => {
                        reqs.needs_strings = true;
                        reqs.case_convert = true;
                    }
                    "split" => {
                        reqs.needs_strings = true;
                        reqs.split = true;
                    }
                    "join" => {
                        reqs.needs_strings = true;
                        reqs.join = true;
                    }
                    "slice" | "charAt" | "charCodeAt" => {
                        reqs.needs_strings = true;
                    }
                    _ => {}
                }
            }
            scan_expr_requirements(callee, reqs);
            for arg in args {
                scan_expr_requirements(arg, reqs);
            }
        }
        Expr::Binary { left, right, .. } | Expr::Compare { left, right, .. } => {
            scan_expr_requirements(left, reqs);
            scan_expr_requirements(right, reqs);
        }
        Expr::Unary { operand, .. } => {
            scan_expr_requirements(operand, reqs);
        }
        Expr::Await(inner) => {
            scan_expr_requirements(inner, reqs);
        }
        Expr::PropertyGet {
            object, property, ..
        } => {
            if property == "length" {
                reqs.needs_strings = true;
            }
            scan_expr_requirements(object, reqs);
        }
        Expr::IndexGet { object, index, .. } => {
            scan_expr_requirements(object, reqs);
            scan_expr_requirements(index, reqs);
        }
        Expr::LocalSet(_, inner) => {
            scan_expr_requirements(inner, reqs);
        }
        _ => {}
    }
}
