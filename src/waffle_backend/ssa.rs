//! Direct Perry HIR → WAFFLE SSA lowering.
//!
//! Lowers assignments, branches, loops, function calls, and returns directly to
//! typed basic blocks, SSA values, and block parameters without linear memory
//! overhead for primitive values.

mod arrays;
mod bytes;
mod decoder;
mod filesystem;
mod loops;
mod optional;
mod options;
mod requirements;
mod string_ops;
mod types;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail, ensure};
use perry_hir::ir::{
    BinaryOp, CatchClause, CompareOp, Expr, Function, Module as HirModule, Stmt, UpdateOp,
};
use perry_hir::types::{LocalId, Type as HirType};
use waffle::{
    Block, BlockTarget, Export, ExportKind, FunctionBody, MemoryArg, Module, Operator, Terminator,
    Type, Value,
};

use crate::waffle_backend::abi::{self, CompletionStatus};
use crate::waffle_backend::control_flow::{JoinPoint, create_block_parameters};
use crate::waffle_backend::exceptions::{
    self, ExitReason, TryClauseBlocks, TryScope, UnwindContext,
};
use crate::waffle_backend::regex::{self, RegexSearch};
use crate::waffle_backend::registry::{FunctionInfo, ModuleRegistry};
use crate::waffle_backend::resolve::ResolvedContract;
use crate::waffle_backend::strings::StringPool;
use requirements::scan_module_string_requirements;

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
    let regex_tables = regex::compile_literals(hir)?;
    let (string_heap_base, regex_programs) = if reqs.needs_strings
        || contract.promises.is_some()
        || super::bytes::required(hir)
        || contract.has_stream_input()
        || !contract.output_operations().is_empty()
        || contract.has_filesystem()
    {
        collect_strings_in_module(hir, &mut string_pool);
        if reqs.decoder {
            string_pool.intern("utf-8");
        }
        string_pool.populate_memory_segments(&mut module.memories[memory]);
        let (regex_programs, next_free) = regex::emit_tables(
            &mut module,
            memory,
            regex_tables,
            string_pool.next_free_address(),
        )?;
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
            (Some(aligned_heap_base), regex_programs)
        } else {
            (Some(next_free), regex_programs)
        }
    } else {
        (None, BTreeMap::new())
    };

    // 3. Build complete module declarations registry
    let registry =
        ModuleRegistry::build(&mut module, hir, contract, string_heap_base, memory, reqs)?;
    let regexes = regex::emit_runtime(&mut module, memory, regex_programs)?;

    // 5. Lower each function body using the established registry contracts
    for func in &hir.functions {
        let info = &registry.functions[&func.id];
        let body = lower_function_body(
            func,
            info,
            &registry,
            &module,
            &string_pool,
            regexes.as_ref(),
            contract,
        )?;
        module.funcs[info.func_index] = waffle::FuncDecl::Body(info.sig, func.name.clone(), body);

        if let Some(export) = &info.export {
            let lift_fn = registry.string_helpers.as_ref().map(|h| h.lift_canonical);
            let lift_bytes = registry.byte_helpers.as_ref().map(|h| h.lift_canonical);
            let wrapper = abi::build_export_wrapper(
                &module,
                info,
                export,
                registry.memory,
                lift_fn,
                lift_bytes,
            )?;
            module.funcs[export.func_index] =
                waffle::FuncDecl::Body(export.sig, format!("{}.export", export.name), wrapper);
            module.exports.push(Export {
                name: export.name.clone(),
                kind: ExportKind::Func(export.func_index),
            });
            if registry.allocator.is_some() {
                crate::waffle_backend::allocation::emit_post_return(&mut module, memory, export)?;
            }
        }
        if let Some(plan) = &contract.promises
            && let Some(task) = plan.tasks.get(&super::promises::TaskTarget::Guest(func.id))
        {
            module.exports.push(Export {
                name: task.symbol.clone(),
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
    contract: &'a ResolvedContract,
    string_pool: &'a StringPool,
    regexes: Option<&'a RegexSearch>,
    return_type: &'a HirType,
    is_async: bool,
    body: FunctionBody,
    block: Block,
    locals: BTreeMap<LocalId, Value>,
    local_types: BTreeMap<LocalId, HirType>,
    stream_parameter: Option<Value>,
    awaited_calls: usize,
    unwind_ctx: UnwindContext,
    loops: Vec<loops::LoopScope>,
    reference_values: BTreeSet<Value>,
    collection_blocks: BTreeSet<Block>,
}

fn lower_function_body(
    func: &Function,
    info: &FunctionInfo,
    registry: &ModuleRegistry,
    module: &Module<'static>,
    string_pool: &StringPool,
    regexes: Option<&RegexSearch>,
    contract: &ResolvedContract,
) -> Result<FunctionBody> {
    let body = FunctionBody::new(module, info.sig);
    let entry = body.entry;
    let mut locals = BTreeMap::new();
    let mut local_types = BTreeMap::new();
    let mut stream_parameter = None;
    let mut reference_values = BTreeSet::new();

    // Map entry block parameters to function parameters
    for (i, param) in func.params.iter().enumerate() {
        let val = body.blocks[entry].params[i].1;
        locals.insert(param.id, val);
        local_types.insert(param.id, param.ty.clone());
        if types::is_reference(&param.ty) {
            reference_values.insert(val);
        }
        if func.id == contract.entry_func_id
            && matches!(&param.ty, HirType::Named(n) if n == "ByteStream")
        {
            stream_parameter = Some(val);
        }
    }

    let mut lowerer = FunctionLowerer {
        module,
        registry,
        contract,
        string_pool,
        regexes,
        return_type: info.success_type(),
        is_async: func.is_async,
        body,
        block: entry,
        locals,
        local_types,
        stream_parameter,
        awaited_calls: 0,
        unwind_ctx: UnwindContext::new(),
        loops: Vec::new(),
        reference_values,
        collection_blocks: BTreeSet::new(),
    };

    if let (Some(stream_val), Some(helpers)) = (stream_parameter, registry.stream_helpers) {
        lowerer.op(
            Operator::Call {
                function_index: helpers.start,
            },
            &[stream_val],
            &[],
        );
    }

    lowerer.statements(&func.body)?;

    // If the block is not terminated, emit default return or ensure proper termination
    if lowerer.body.blocks[lowerer.block].terminator == Terminator::None {
        lowerer.emit_return(None);
    }

    if let Some(allocator) = registry.allocator {
        super::allocation::track_roots(
            &mut lowerer.body,
            registry.memory,
            allocator,
            &lowerer.reference_values,
            &lowerer.collection_blocks,
        )?;
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
                Stmt::Expr(expr @ Expr::LocalSet(..)) => {
                    self.expression(expr)?;
                }
                Stmt::Expr(Expr::Await(expr)) => {
                    self.await_expression(expr, true)?;
                }
                Stmt::Expr(expr) => {
                    ensure!(
                        !matches!(self.infer_expr_type(expr), HirType::Promise(_)),
                        "Detached async calls are unsupported; store and await their outcome"
                    );
                    self.expression(expr)?;
                }
                Stmt::Return(expr) => {
                    let ret_val = expr
                        .as_ref()
                        .map(|expr| {
                            if matches!(self.infer_expr_type(expr), HirType::Promise(_)) {
                                ensure!(
                                    self.is_async,
                                    "Returning a stored Promise requires an async function"
                                );
                                ensure!(self.infer_expr_type(expr) == HirType::Promise(Box::new(self.return_type.clone())), "Returned Promise outcome does not match the function result type");
                                return self
                                    .await_expression(expr, false)?
                                    .ok_or_else(|| anyhow::anyhow!("Promise return has no value"));
                            }
                            if self.return_type == &HirType::String {
                                self.string_receiver(expr)
                            } else if super::bytes::is_byte_view(self.return_type) {
                                self.byte_receiver(expr)
                            } else if super::decoder::is_decoder(self.return_type) {
                                self.decoder_receiver(expr)
                            } else {
                                ensure!(!super::decoder::is_decoder(&self.infer_expr_type(expr)), "Cannot return a TextDecoder as {:?}", self.return_type);
                                ensure!(!super::bytes::is_byte_view(&self.infer_expr_type(expr)), "Cannot return a Uint8Array as {:?}", self.return_type);
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
                    self.loop_statement(Some(condition), body, None)?;
                }
                Stmt::For {
                    init,
                    condition,
                    update,
                    body,
                } => {
                    if let Some(init) = init {
                        self.statements(std::slice::from_ref(init))?;
                    }
                    self.loop_statement(condition.as_ref(), body, update.as_ref())?;
                }
                Stmt::Break => self.loop_exit(ExitReason::Break)?,
                Stmt::Continue => self.loop_exit(ExitReason::Continue)?,
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

    fn call_operation(&mut self, callee: &Expr, args: &[Expr]) -> Result<Option<Value>> {
        if let Expr::ExternFuncRef { name, .. } = callee
            && let Some(super::resolve::TypedIntrinsic::Capability(
                super::capabilities::CapabilityOperation::Filesystem(operation),
            )) = self.contract.intrinsics.get(name)
        {
            return self.filesystem_operation(*operation, args);
        }
        if let Expr::ExternFuncRef { name, .. } = callee
            && matches!(
                self.contract.intrinsics.get(name),
                Some(super::resolve::TypedIntrinsic::DecoderNew)
            )
        {
            return self.new_decoder(args).map(Some);
        }
        if let Expr::PropertyGet {
            object, property, ..
        } = callee
        {
            if super::decoder::is_decoder(&self.infer_expr_type(object)) {
                ensure!(
                    property == "decode",
                    "Unsupported TextDecoder method '{property}'"
                );
                return self.decode_bytes(object, args).map(Some);
            }
            if super::bytes::is_byte_view(&self.infer_expr_type(object)) {
                return self.byte_method(object, property, args).map(Some);
            }
            ensure!(
                !matches!(self.infer_expr_type(object), HirType::Promise(_)),
                "Promise methods are unsupported; await the retained outcome"
            );
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
                Expr::ExternFuncRef { param_types, .. } => param_types.get(index),
                _ => None,
            };
            let argument_type = self.infer_expr_type(arg);
            if expected.is_some_and(super::decoder::is_decoder) {
                ensure!(
                    super::decoder::is_decoder(&argument_type),
                    "Decoder parameters require TextDecoder arguments"
                );
            }
            if matches!(expected, Some(HirType::Named(name)) if name == "ByteStream") {
                ensure!(
                    matches!(&argument_type, HirType::Named(name) if name == "ByteStream"),
                    "Stream parameters require ByteStream arguments"
                );
            }
            if expected.is_some_and(super::bytes::is_byte_view) {
                ensure!(
                    super::bytes::is_byte_view(&argument_type),
                    "Byte view parameters require Uint8Array arguments"
                );
            }
            if matches!(argument_type, HirType::Promise(_))
                || matches!(expected, Some(HirType::Promise(_)))
            {
                ensure!(
                    expected == Some(&argument_type),
                    "Stored Promise arguments must match their declared outcome type"
                );
            }
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

        if let Some(runtime) = &self.registry.promises
            && let Some(target) = super::promises::TaskTarget::from_callee(callee)
            && let Some(&start) = runtime.starts.get(&target)
        {
            let signature = &self.module.signatures[self.module.funcs[start].sig()];
            ensure!(
                arg_vals.len() + 1 == signature.params.len(),
                "Stored async call has incorrect argument count"
            );
            for (value, expected) in arg_vals.iter().zip(&signature.params[1..]) {
                ensure!(
                    self.body.values[*value].ty(&self.body.type_pool) == Some(*expected),
                    "Stored async call has an incompatible argument type"
                );
            }
            let task = &self.contract.promises.as_ref().unwrap().tasks[&target];
            let kind = if task.result == HirType::String {
                super::allocation::AllocationKind::StringPromise
            } else {
                super::allocation::AllocationKind::ScalarPromise
            };
            let kind = self.op(Operator::I32Const { value: kind as u32 }, &[], &[Type::I32]);
            let record = self.op(
                Operator::Call {
                    function_index: runtime.new,
                },
                &[kind],
                &[Type::I32],
            );
            self.reference_values.insert(record);
            arg_vals.insert(0, record);
            let status = self.op(
                Operator::Call {
                    function_index: start,
                },
                &arg_vals,
                &[Type::I32],
            );
            self.op(
                Operator::Call {
                    function_index: runtime.bind,
                },
                &[record, status],
                &[],
            );
            return Ok(Some(record));
        }

        if let Expr::ExternFuncRef { name, .. } = callee {
            let &func_idx = self
                .registry
                .intrinsics
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("Unknown extern function: {name}"))?;
            let signature = &self.module.signatures[self.module.funcs[func_idx].sig()];
            let intrinsic = &self.contract.intrinsics[name];
            let has_completion = intrinsic.has_completion();
            let name = intrinsic.name();
            ensure!(
                arg_vals.len() == signature.params.len(),
                "Intrinsic '{name}' expects {} arguments, got {}",
                signature.params.len(),
                arg_vals.len()
            );
            for (index, (value, expected)) in arg_vals.iter().zip(&signature.params).enumerate() {
                ensure!(
                    self.body.values[*value].ty(&self.body.type_pool) == Some(*expected),
                    "Intrinsic '{name}' argument {} must have core type {expected:?}",
                    index + 1
                );
            }
            if has_completion {
                self.call_completion(func_idx, &arg_vals);
                return Ok(None);
            }
            let ret_types = &signature.returns;
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

            let payload = self.call_completion(callee_info.func_index, &arg_vals);
            if matches!(callee_info.success_type(), HirType::Void) {
                Ok(None)
            } else {
                let return_val = abi::decode_payload(
                    &mut self.body,
                    self.block,
                    payload,
                    crate::waffle_backend::registry::map_type_to_waffle(
                        callee_info.success_type(),
                    )? == Type::I32,
                );
                Ok(Some(return_val))
            }
        } else {
            bail!("Unsupported call callee in WAFFLE lowering: {callee:?}");
        }
    }

    fn call_completion(&mut self, function: waffle::Func, args: &[Value]) -> Value {
        let outcome = abi::emit_fallible_call(&mut self.body, self.block, function, args);
        self.block = outcome.err_block;
        self.emit_throw(outcome.payload);
        self.block = outcome.ok_block;
        outcome.payload
    }

    fn await_expression(&mut self, expr: &Expr, is_statement: bool) -> Result<Option<Value>> {
        if let Some(runtime) = &self.registry.promises {
            let value = self.expression(expr)?;
            let result = if let HirType::Promise(result) = self.infer_expr_type(expr) {
                let payload = self.call_completion(runtime.await_result, &[value]);
                if is_statement {
                    None
                } else if matches!(result.as_ref(), HirType::Void) {
                    Some(self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]))
                } else {
                    Some(abi::decode_payload(
                        &mut self.body,
                        self.block,
                        payload,
                        matches!(result.as_ref(), HirType::Boolean | HirType::String),
                    ))
                }
            } else {
                self.op(
                    Operator::Call {
                        function_index: runtime.yield_thread,
                    },
                    &[],
                    &[],
                );
                if is_statement { None } else { Some(value) }
            };
            return Ok(self.continuation(result));
        }
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
                if types::identity_kind(&self.infer_expr_type(left)).is_some()
                    || types::identity_kind(&self.infer_expr_type(right)).is_some() =>
            {
                let left_kind = types::identity_kind(&self.infer_expr_type(left));
                let right_kind = types::identity_kind(&self.infer_expr_type(right));
                ensure!(
                    matches!(op, CompareOp::Eq | CompareOp::Ne),
                    "{} values support strict identity comparisons only",
                    left_kind.or(right_kind).unwrap()
                );
                let left = self.expression(left)?;
                let right = self.expression(right)?;
                if left_kind == right_kind {
                    let operator = if *op == CompareOp::Eq {
                        Operator::I32Eq
                    } else {
                        Operator::I32Ne
                    };
                    return Ok(self.op(operator, &[left, right], &[Type::I32]));
                }
                Ok(self.op(
                    Operator::I32Const {
                        value: u32::from(*op == CompareOp::Ne),
                    },
                    &[],
                    &[Type::I32],
                ))
            }
            Expr::Compare { op, left, right }
                if self.is_optional_number(left) || self.is_optional_number(right) =>
            {
                self.optional_number_comparison(*op, left, right)
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
                } else if types::identity_kind(&self.infer_expr_type(expr)).is_some() {
                    Ok(self.op(Operator::I32Const { value: 1 }, &[], &[Type::I32]))
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
        let reference = types::is_reference(&self.infer_expr_type(expr));
        let value = self.lower_expression(expr)?;
        if reference && self.body.values[value].ty(&self.body.type_pool) == Some(Type::I32) {
            self.reference_values.insert(value);
        }
        Ok(value)
    }

    fn lower_expression(&mut self, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::TextDecoderNew {
                label,
                fatal,
                ignore_bom,
            } => self.new_decoder(&[
                *label.clone(),
                Expr::Object(vec![
                    ("fatal".into(), *fatal.clone()),
                    ("ignoreBOM".into(), *ignore_bom.clone()),
                ]),
            ]),
            Expr::TextDecoderDecode { decoder, input } => {
                self.decode_bytes(decoder, std::slice::from_ref(input.as_ref()))
            }
            Expr::TextDecoderEncoding(decoder) => self.decoder_property(decoder, "encoding"),
            Expr::TextDecoderFatal(decoder) => self.decoder_property(decoder, "fatal"),
            Expr::TextDecoderIgnoreBom(decoder) => self.decoder_property(decoder, "ignoreBOM"),
            Expr::Uint8ArrayNew(argument) => self.new_bytes(argument.as_deref()),
            Expr::Uint8ArrayGet { array, index } => self.byte_index(array, index),
            Expr::Uint8ArraySet {
                array,
                index,
                value,
            } => self.byte_set(array, index, value),
            Expr::Uint8ArrayLength(array) => self.byte_property(array, "length"),
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
            Expr::ForOfToArray(input) => self.string_receiver(input),
            Expr::ArrayJoin { array, separator } => self.array_join(
                array,
                separator
                    .as_deref()
                    .map(std::slice::from_ref)
                    .unwrap_or_default(),
            ),
            Expr::Update { id, op, prefix } => {
                let previous = *self
                    .locals
                    .get(id)
                    .ok_or_else(|| anyhow::anyhow!("Uninitialized update binding {id}"))?;
                ensure!(
                    self.body.values[previous].ty(&self.body.type_pool) == Some(Type::F64),
                    "Update operands must be numeric"
                );
                let one = self.op(
                    Operator::F64Const {
                        value: 1f64.to_bits(),
                    },
                    &[],
                    &[Type::F64],
                );
                let operator = match op {
                    UpdateOp::Increment => Operator::F64Add,
                    UpdateOp::Decrement => Operator::F64Sub,
                };
                let updated = self.op(operator, &[previous, one], &[Type::F64]);
                self.locals.insert(*id, updated);
                Ok(if *prefix { updated } else { previous })
            }
            Expr::LocalSet(id, expr) => {
                let inferred = self.infer_expr_type(expr);
                if let Some(previous) = self.local_types.get(id)
                    && let Some(kind) =
                        types::identity_kind(previous).or_else(|| types::identity_kind(&inferred))
                {
                    ensure!(
                        previous == &inferred,
                        "A {} binding cannot change its logical type",
                        if kind == "Promise" {
                            "stored Promise"
                        } else {
                            kind
                        }
                    );
                }
                let value = self.expression(expr)?;
                self.local_types.insert(*id, inferred);
                self.locals.insert(*id, value);
                Ok(value)
            }
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
                ensure!(
                    self.body.values[cp].ty(&self.body.type_pool) == Some(Type::F64),
                    "fromCodePoint requires a numeric argument"
                );
                let payload = self.call_completion(func, &[cp]);
                Ok(abi::decode_payload(
                    &mut self.body,
                    self.block,
                    payload,
                    true,
                ))
            }
            Expr::PropertyGet {
                object, property, ..
            } if super::decoder::is_decoder(&self.infer_expr_type(object)) => {
                self.decoder_property(object, property)
            }
            Expr::PropertyGet {
                object, property, ..
            } if super::bytes::is_byte_view(&self.infer_expr_type(object)) => {
                self.byte_property(object, property)
            }
            Expr::PropertyGet {
                object, property, ..
            } if property == "length" => {
                ensure!(
                    !matches!(self.infer_expr_type(object), HirType::Promise(_)),
                    "Promise properties are unsupported; await the retained outcome"
                );
                if self.is_string(object) || self.is_scalar_iteration(object) {
                    let desc = if self.is_scalar_iteration(object) {
                        self.expression(object)?
                    } else {
                        self.string_receiver(object)?
                    };
                    let scalar_len = self.string_length(desc);
                    Ok(self.op(Operator::F64ConvertI32U, &[scalar_len], &[Type::F64]))
                } else {
                    ensure!(
                        self.infer_expr_type(object) == HirType::Array(Box::new(HirType::String)),
                        "Unsupported length receiver"
                    );
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
                if super::bytes::is_byte_view(&self.infer_expr_type(object)) {
                    return self.byte_index(object, index);
                }
                ensure!(
                    !matches!(self.infer_expr_type(object), HirType::Promise(_)),
                    "Promise indexing is unsupported; await the retained outcome"
                );
                if self.is_string(object) || self.is_scalar_iteration(object) {
                    let iteration = self.is_scalar_iteration(object);
                    let desc = if iteration {
                        self.expression(object)?
                    } else {
                        self.string_receiver(object)?
                    };
                    let idx = self.position_argument(Some(index), f64::NAN)?;
                    let helpers = self
                        .registry
                        .string_helpers
                        .as_ref()
                        .expect("string helpers available");
                    Ok(self.op(
                        Operator::Call {
                            function_index: if iteration {
                                helpers.str_char_at
                            } else {
                                helpers.str_index
                            },
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
        if let (Some(stream_val), Some(helpers)) =
            (self.stream_parameter, self.registry.stream_helpers)
        {
            self.op(
                Operator::Call {
                    function_index: helpers.drop,
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
        if types::is_reference(self.return_type)
            && let Some(value) = ret_val
        {
            self.reference_values.insert(value);
        }
        let payload = abi::encode_payload(&mut self.body, self.block, ret_val);
        if exceptions::route_cleanup(
            &mut self.body,
            self.block,
            self.unwind_ctx.target_for_exit(0),
            &self.locals,
            (ExitReason::Return, payload),
        ) {
            self.emit_terminal_return(ret_val);
        }
    }

    fn emit_finally_return(&mut self, payload: Value) {
        if exceptions::route_cleanup(
            &mut self.body,
            self.block,
            self.unwind_ctx.target_for_exit(0),
            &self.locals,
            (ExitReason::Return, payload),
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
                let exits = exceptions::emit_finally_dispatcher(
                    &mut self.body,
                    self.block,
                    environment.exit_reason,
                    blocks.join_block,
                    &blocks.scope_locals,
                    &self.locals,
                );
                join_reached = true;
                for (reason, block) in exits {
                    self.block = block;
                    match reason {
                        ExitReason::Return => self.emit_finally_return(environment.payload),
                        ExitReason::Throw => self.emit_throw(environment.payload),
                        ExitReason::Break | ExitReason::Continue if !self.loops.is_empty() => {
                            self.loop_exit(reason)?
                        }
                        _ => self.body.set_terminator(block, Terminator::Unreachable),
                    }
                }
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
    let mut intern = |expr: &Expr| {
        if let Expr::String(text) = expr {
            pool.intern(text);
        }
    };
    for function in &hir.functions {
        super::visit::visit_function_expressions(function, &mut intern);
    }
    super::visit::visit_statements(&hir.init, &mut intern);
}
