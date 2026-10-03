//! Direct Perry HIR → WAFFLE SSA lowering.
//!
//! Lowers assignments, branches, loops, function calls, and returns directly to
//! typed basic blocks, SSA values, and block parameters without linear memory
//! overhead for primitive values.

use std::collections::BTreeMap;

use anyhow::{Result, bail, ensure};
use perry_hir::ir::{BinaryOp, CompareOp, Expr, Function, Module as HirModule, Stmt};
use perry_hir::types::{FuncId, LocalId, Type as HirType};
use waffle::{
    Block, BlockTarget, Export, ExportKind, Func, FuncDecl, FunctionBody, Import, ImportKind,
    Module, Operator, SignatureData, Terminator, Type, Value,
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
        let signature = module.signatures.push(SignatureData {
            params,
            returns,
        });
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

    // 2. Pre-declare all module functions to allow mutual / intra-module calls
    let mut func_signatures = BTreeMap::new();
    let mut func_decls = BTreeMap::new();

    for func in &hir.functions {
        let params = func
            .params
            .iter()
            .map(|p| map_type_to_waffle(&p.ty))
            .collect::<Result<Vec<_>>>()?;
        let returns = map_return_type_to_waffle(&func.return_type)?;
        let sig = module.signatures.push(SignatureData { params, returns });
        func_signatures.insert(func.id, sig);
    }

    // 3. Lower each function body
    for func in &hir.functions {
        let sig = func_signatures[&func.id];
        let body = lower_function_body(
            func,
            sig,
            &module,
            contract,
            &intrinsic_funcs,
            &func_decls,
            stream_helpers,
        )?;
        let func_decl = module
            .funcs
            .push(FuncDecl::Body(sig, func.name.clone(), body));
        func_decls.insert(func.id, func_decl);

        if func.is_exported || func.id == contract.entry_func_id {
            let export_name = if func.name == "main" || func.name == "experiment" || func.id == contract.entry_func_id {
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
    stream_helpers: Option<(Func, Func)>,
    stream_parameter: Option<LocalId>,
    awaited_calls: usize,
}

fn lower_function_body(
    func: &Function,
    sig: waffle::Signature,
    module: &Module<'static>,
    contract: &ResolvedContract,
    intrinsic_funcs: &BTreeMap<String, Func>,
    func_decls: &BTreeMap<FuncId, Func>,
    stream_helpers: Option<(Func, Func)>,
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

    let mut lowerer = FunctionLowerer {
        module,
        _contract: contract,
        body,
        block: entry,
        locals,
        intrinsic_funcs,
        func_decls,
        stream_helpers,
        stream_parameter,
        awaited_calls: 0,
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
        let ret_types = &module.signatures[sig].returns;
        if ret_types.is_empty() {
            lowerer.body.set_terminator(
                lowerer.block,
                Terminator::Return { values: vec![] },
            );
        } else if ret_types.len() == 1 && ret_types[0] == Type::F64 {
            let zero = lowerer.op(
                Operator::F64Const { value: 0f64.to_bits() },
                &[],
                &[Type::F64],
            );
            lowerer.body.set_terminator(
                lowerer.block,
                Terminator::Return { values: vec![zero] },
            );
        } else {
            bail!("Unterminated block in function '{}' with non-void return", func.name);
        }
    }

    lowerer.body.validate()?;
    lowerer.body.verify_reducible()?;

    Ok(lowerer.body)
}

impl<'a> FunctionLowerer<'a> {
    fn statements(&mut self, stmts: &[Stmt]) -> Result<()> {
        for stmt in stmts {
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
                Stmt::Return(Some(expr)) => {
                    let val = self.expression(expr)?;
                    if let (Some(stream_id), Some((drop, _))) =
                        (self.stream_parameter, self.stream_helpers)
                    {
                        if let Some(&stream_val) = self.locals.get(&stream_id) {
                            self.op(
                                Operator::Call {
                                    function_index: drop,
                                },
                                &[stream_val],
                                &[],
                            );
                        }
                    }
                    self.body.set_terminator(
                        self.block,
                        Terminator::Return { values: vec![val] },
                    );
                    let dead = self.body.add_block();
                    self.block = dead;
                }
                Stmt::Return(None) => {
                    if let (Some(stream_id), Some((drop, _))) =
                        (self.stream_parameter, self.stream_helpers)
                    {
                        if let Some(&stream_val) = self.locals.get(&stream_id) {
                            self.op(
                                Operator::Call {
                                    function_index: drop,
                                },
                                &[stream_val],
                                &[],
                            );
                        }
                    }
                    self.body.set_terminator(
                        self.block,
                        Terminator::Return { values: vec![] },
                    );
                    let dead = self.body.add_block();
                    self.block = dead;
                }
                Stmt::If {
                    condition,
                    then_branch,
                    else_branch,
                } => {
                    self.if_statement(condition, then_branch, else_branch.as_deref().unwrap_or(&[]))?;
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
            let args = self.locals.keys().map(|id| self.locals[id]).collect();
            self.branch(join_block, args);
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
            bail!("Only awaiting a call expression is supported, got: {expr:?}");
        };

        let callee_name = match callee.as_ref() {
            Expr::ExternFuncRef { name, .. } => name.as_str(),
            _ => bail!("Await callee must be a declared intrinsic, got: {callee:?}"),
        };

        let intrinsic_func = self
            .intrinsic_funcs
            .get(callee_name)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("Unknown async intrinsic: {callee_name}"))?;

        // Evaluate arguments left-to-right
        let mut arg_values = Vec::new();
        for arg in args {
            arg_values.push(self.expression(arg)?);
        }

        let ret_types = &self.module.signatures[self.module.funcs[intrinsic_func].sig()].returns;
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

    fn continuation(&mut self, result: Option<Value>) -> Option<Value> {
        self.awaited_calls += 1;
        let resumed = self.body.add_block();
        self.body.blocks[resumed].desc = "await continuation".into();
        let resumed_locals = self.block_parameters(resumed, &self.locals.clone());
        let mut args: Vec<_> = self.locals.values().copied().collect();

        let resumed_result = result.map(|value| {
            args.push(value);
            self.body.add_blockparam(resumed, Type::F64)
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
                let operator = match op {
                    CompareOp::Eq | CompareOp::LooseEq => Operator::F64Eq,
                    CompareOp::Ne | CompareOp::LooseNe => Operator::F64Ne,
                    CompareOp::Lt => Operator::F64Lt,
                    CompareOp::Le => Operator::F64Le,
                    CompareOp::Gt => Operator::F64Gt,
                    CompareOp::Ge => Operator::F64Ge,
                };
                Ok(self.op(operator, &[left_val, right_val], &[Type::I32]))
            }
            _ => bail!("Unsupported condition expression: {expr:?}"),
        }
    }

    fn expression(&mut self, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::Number(n) => Ok(self.op(
                Operator::F64Const {
                    value: n.to_bits(),
                },
                &[],
                &[Type::F64],
            )),
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
                    let ret_types = &self.module.signatures[self.module.funcs[func_idx].sig()].returns;
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
                    let mut arg_vals = Vec::new();
                    for a in args {
                        arg_vals.push(self.expression(a)?);
                    }
                    let ret_types = &self.module.signatures[self.module.funcs[func_idx].sig()].returns;
                    Ok(self.op(
                        Operator::Call {
                            function_index: func_idx,
                        },
                        &arg_vals,
                        ret_types,
                    ))
                } else {
                    bail!("Unsupported call callee in WAFFLE lowering: {callee:?}");
                }
            }
            _ => bail!("Unsupported expression in WAFFLE lowering: {expr:?}"),
        }
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
