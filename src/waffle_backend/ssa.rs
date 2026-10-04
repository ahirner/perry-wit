//! Direct Perry HIR → WAFFLE SSA lowering.
//!
//! Lowers assignments, branches, loops, function calls, and returns directly to
//! typed basic blocks, SSA values, and block parameters without linear memory
//! overhead for primitive values.

mod arrays;
mod boolean;
mod bytes;
mod date;
mod decoder;
mod filesystem;
mod http;
mod loops;
mod objects;
mod optional;
mod options;
mod random;
mod requirements;
mod string_ops;
mod text_or_bytes;
mod time;
mod tuples;
mod typed;
mod types;
mod values;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use perry_hir::ir::{
    BinaryOp, CatchClause, CompareOp, Expr, Function, Module as HirModule, Stmt, UnaryOp, UpdateOp,
};
use perry_hir::types::{LocalId, Type as HirType};
use waffle::{
    Block, BlockTarget, Export, ExportKind, FunctionBody, MemoryArg, Module, Operator, Terminator,
    Type, Value,
};

use super::text_or_bytes::is_text_or_bytes;
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
    let mut reqs = scan_module_string_requirements(hir);
    reqs.objects |= contract.has_http() || contract.wit.is_some();
    reqs.objects |= contract
        .context_operations()
        .contains(&super::capabilities::ContextOperation::Environment);
    reqs.needs_strings |= contract.wit.is_some()
        || super::values::required(hir)
        || super::date::required(hir)
        || super::time::required(hir)
        || !contract.context_operations().is_empty()
        || contract.has_filesystem()
        || contract.has_http()
        || contract
            .random_operations()
            .contains(&super::capabilities::RandomOperation::Uuid);
    let mut string_pool = StringPool::new();
    let regex_tables = regex::compile_literals(hir)?;
    let (string_heap_base, regex_programs) = if reqs.needs_strings
        || contract.promises.is_some()
        || super::bytes::required(hir)
        || super::structured::required(hir)
        || contract.has_stream_input()
        || !contract.output_operations().is_empty()
        || contract.has_filesystem()
    {
        collect_strings_in_module(hir, &mut string_pool);
        if let Some(wit) = &contract.wit {
            wit.intern_keys(
                &mut string_pool,
                contract
                    .intrinsics
                    .values()
                    .filter_map(|intrinsic| match intrinsic {
                        super::resolve::TypedIntrinsic::WitImport { key, .. } => Some(key.clone()),
                        _ => None,
                    }),
            );
        }
        if contract.has_http() {
            string_pool.intern("http");
            string_pool.intern("https");
        }
        if reqs.objects {
            string_pool.intern("length");
        }
        if contract.has_filesystem() {
            for key in super::filesystem::OPTION_KEYS {
                string_pool.intern(key);
            }
        }
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
        let mut helper_libraries = Vec::new();
        if reqs.find_substring {
            helper_libraries.push(super::libraries::LibraryId::Search);
        }
        if reqs.code_point_at
            || reqs.from_code_point
            || reqs.case_convert
            || reqs.split
            || reqs.join
        {
            helper_libraries.push(super::libraries::LibraryId::Text);
        }
        if reqs.json {
            helper_libraries.push(super::libraries::LibraryId::Json);
        }
        if super::time::required(hir) {
            helper_libraries.push(super::libraries::LibraryId::Time);
        }
        if !helper_libraries.is_empty() {
            let mut placement = super::libraries::HelperMemory::new(next_free);
            for id in helper_libraries {
                placement.place(&super::libraries::Library::parse(id.bytes())?)?;
            }
            let aligned_heap_base = super::libraries::align_to(placement.stack_top()?, 65_536)?;
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
    let registry = ModuleRegistry::build(
        &mut module,
        hir,
        contract,
        string_heap_base,
        memory,
        reqs,
        &string_pool,
    )?;
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
            let wrapper = if let Some(wit) = &contract.wit {
                super::wit::build_export_wrapper(
                    &module,
                    info,
                    export,
                    &registry,
                    wit,
                    &wit.functions[&func.name],
                    &string_pool,
                )?
            } else {
                abi::build_export_wrapper(&module, info, export, &registry)?
            };
            module.funcs[export.func_index] =
                waffle::FuncDecl::Body(export.sig, format!("{}.export", export.name), wrapper);
            module.exports.push(Export {
                name: export.name.clone(),
                kind: ExportKind::Func(export.func_index),
            });
            if let Some(allocator) = registry.allocator {
                crate::waffle_backend::allocation::emit_post_return(
                    &mut module,
                    allocator,
                    export,
                )?;
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
    narrowings: BTreeMap<LocalId, HirType>,
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
        narrowings: BTreeMap::new(),
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

    let lowered = lowerer.statements(&func.body);
    if contract.wit.is_some() {
        lowered.with_context(|| format!("Lowering function {}", func.name))?;
    } else {
        lowered?;
    }

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
                    ty,
                    init: Some(expr),
                    ..
                } => {
                    let inferred = self.infer_expr_type(expr);
                    if self.contract.wit.is_some()
                        && *ty != HirType::Any
                        && !(ty == &HirType::Number
                            && inferred == HirType::Union(vec![HirType::Number, HirType::Void]))
                    {
                        self.check_typed_value(expr, ty)?;
                    }
                    if super::time::is_time(ty) {
                        ensure!(
                            ty == &inferred,
                            "Temporal initializers must match their declared type"
                        );
                    }
                    if super::http::is_response(ty) {
                        ensure!(
                            ty == &inferred,
                            "HTTP response initializers must match their declared type"
                        );
                    }
                    let (ty, val) = if self.contract.wit.is_some()
                        && (super::objects::is_object(ty)
                            || matches!(ty, HirType::Array(_) | HirType::Tuple(_)))
                    {
                        (ty.clone(), self.typed_operand(expr, ty)?)
                    } else if super::nullable::inner(ty).is_some() {
                        self.check_typed_value(expr, ty)?;
                        (ty.clone(), self.typed_operand(expr, ty)?)
                    } else if is_text_or_bytes(ty) || is_text_or_bytes(&inferred) {
                        (
                            super::text_or_bytes::value_type(),
                            self.text_or_bytes_operand(expr)?,
                        )
                    } else {
                        (
                            if matches!(ty, HirType::Tuple(_))
                                || (super::values::is_string_type(ty) && *ty != HirType::String)
                                || super::objects::is_object(ty)
                                    && super::objects::is_object(&inferred)
                            {
                                ty.clone()
                            } else {
                                inferred
                            },
                            self.expression(expr)?,
                        )
                    };
                    self.local_types.insert(*id, ty);
                    self.narrowings.remove(id);
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
                        .map(|expr| self.return_expression(expr))
                        .transpose()?;
                    self.emit_return(ret_val);
                }
                Stmt::Throw(expr) => {
                    let err_val = if super::values::is_dynamic(&self.infer_expr_type(expr)) {
                        self.unbox_value(expr, &HirType::Number)?
                    } else {
                        self.expression(expr)?
                    };
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

    fn return_expression(&mut self, expr: &Expr) -> Result<Value> {
        if super::nullable::inner(self.return_type).is_some() {
            return self.typed_operand(expr, &self.return_type.clone());
        }
        if self.contract.wit.is_some() {
            self.check_typed_value(expr, self.return_type)?;
            if super::objects::is_object(self.return_type)
                || matches!(self.return_type, HirType::Array(_) | HirType::Tuple(_))
            {
                return self.typed_operand(expr, &self.return_type.clone());
            }
        }
        if matches!(self.return_type, HirType::Tuple(_))
            || matches!(self.return_type,HirType::Array(inner) if **inner!=HirType::String)
        {
            self.check_typed_value(expr, self.return_type)?;
            return self.expression(expr);
        }
        if let HirType::Promise(result) = self.infer_expr_type(expr) {
            ensure!(
                self.is_async,
                "Returning a stored Promise requires an async function"
            );
            let value = self
                .await_expression(expr, false)?
                .ok_or_else(|| anyhow::anyhow!("Promise return has no value"))?;
            if super::values::is_dynamic(self.return_type) {
                return self.box_typed_value(value, &result);
            }
            if super::values::is_dynamic(&result) {
                return self.extract_value(value, &self.return_type.clone());
            }
            ensure!(
                super::text_or_bytes::equivalent(&result, self.return_type),
                "Returned Promise outcome does not match the function result type"
            );
            return Ok(value);
        }
        if super::values::is_dynamic(self.return_type) {
            let value = self.value_operand(expr)?;
            if self.is_async {
                let payload = self.call_completion(
                    self.registry
                        .value_helpers
                        .expect("dynamic values have helpers")
                        .async_result,
                    &[value],
                );
                return Ok(abi::decode_payload(
                    &mut self.body,
                    self.block,
                    payload,
                    true,
                ));
            }
            return Ok(value);
        }
        if super::values::is_dynamic(&self.infer_expr_type(expr)) {
            let expected = self.return_type.clone();
            return self.unbox_value(expr, &expected);
        }
        if is_text_or_bytes(self.return_type) {
            self.text_or_bytes_operand(expr)
        } else if super::values::is_string_type(self.return_type) {
            self.string_receiver(expr)
        } else if super::bytes::is_byte_view(self.return_type) {
            self.byte_receiver(expr)
        } else if super::decoder::is_decoder(self.return_type) {
            self.decoder_receiver(expr)
        } else if super::objects::is_object(self.return_type) {
            ensure!(
                super::objects::is_object(&self.infer_expr_type(expr)),
                "Object results require object values"
            );
            self.expression(expr)
        } else if super::filesystem::is_stats(self.return_type)
            || super::date::is_date(self.return_type)
            || super::time::is_time(self.return_type)
            || super::http::is_response(self.return_type)
            || matches!(self.return_type, HirType::Array(_))
        {
            ensure!(
                &self.infer_expr_type(expr) == self.return_type,
                "Returned value must match {:?}",
                self.return_type
            );
            self.expression(expr)
        } else {
            ensure!(
                !super::objects::is_object(&self.infer_expr_type(expr)),
                "Cannot return a plain object as {:?}",
                self.return_type
            );
            ensure!(
                !super::filesystem::is_stats(&self.infer_expr_type(expr))
                    && !matches!(self.infer_expr_type(expr), HirType::Array(_)),
                "Cannot return an object as {:?}",
                self.return_type
            );
            ensure!(
                !is_text_or_bytes(&self.infer_expr_type(expr)),
                "Cannot return a string-or-byte value as {:?}; narrow it first",
                self.return_type
            );
            ensure!(
                !super::date::is_date(&self.infer_expr_type(expr))
                    && !super::time::is_time(&self.infer_expr_type(expr))
                    && !super::http::is_response(&self.infer_expr_type(expr)),
                "Cannot return a Date, Temporal, or HTTP response value as {:?}",
                self.return_type
            );
            ensure!(
                !super::decoder::is_decoder(&self.infer_expr_type(expr)),
                "Cannot return a TextDecoder as {:?}",
                self.return_type
            );
            ensure!(
                !super::bytes::is_byte_view(&self.infer_expr_type(expr)),
                "Cannot return a Uint8Array as {:?}",
                self.return_type
            );
            ensure!(
                !self.is_string_or_undefined(expr) || self.return_type == &HirType::Void,
                "Cannot return a string-or-undefined value as {:?}",
                self.return_type
            );
            self.expression(expr)
        }
    }

    fn if_statement(
        &mut self,
        condition: &Expr,
        then_branch: &[Stmt],
        else_branch: &[Stmt],
    ) -> Result<()> {
        let cond_val = self.condition(condition)?;
        let incoming_locals = self.locals.clone();
        let incoming_narrowings = self.narrowings.clone();

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
        self.narrow_type_guard(condition, true);
        self.statements(then_branch)?;
        let then_reaches_join = self.body.blocks[self.block].terminator == Terminator::None;
        let then_narrowings = self.narrowings.clone();
        if self.body.blocks[self.block].terminator == Terminator::None {
            join.emit_branch(&mut self.body, self.block, &self.locals);
        }

        // Lower else branch (move incoming_locals)
        self.block = else_block;
        self.locals = incoming_locals;
        self.narrowings = incoming_narrowings;
        self.narrow_type_guard(condition, false);
        self.statements(else_branch)?;
        let else_reaches_join = self.body.blocks[self.block].terminator == Terminator::None;
        if self.body.blocks[self.block].terminator == Terminator::None {
            join.emit_branch(&mut self.body, self.block, &self.locals);
        }

        if self.body.blocks[join.block].preds.is_empty() {
            self.body
                .set_terminator(join.block, Terminator::Unreachable);
        }
        self.block = join.block;
        self.locals = join.bindings;
        if !else_reaches_join {
            self.narrowings = then_narrowings;
        } else if then_reaches_join {
            self.narrowings
                .retain(|id, ty| then_narrowings.get(id) == Some(ty));
        }
        Ok(())
    }

    fn call_operation(&mut self, callee: &Expr, args: &[Expr]) -> Result<Option<Value>> {
        if let Expr::ExternFuncRef { name, .. } = callee
            && let Some(super::resolve::TypedIntrinsic::Temporal(operation)) =
                self.contract.intrinsics.get(name)
        {
            return self.new_time(*operation, args).map(Some);
        }
        if let Expr::ExternFuncRef { name, .. } = callee
            && matches!(
                self.contract.intrinsics.get(name),
                Some(super::resolve::TypedIntrinsic::Capability(
                    super::capabilities::CapabilityOperation::Random(
                        super::capabilities::RandomOperation::Fill
                    )
                ))
            )
        {
            return self.random_fill(name, args).map(Some);
        }
        if let Expr::ExternFuncRef { name, .. } = callee
            && matches!(
                self.contract.intrinsics.get(name),
                Some(super::resolve::TypedIntrinsic::Capability(
                    super::capabilities::CapabilityOperation::HttpGet
                ))
            )
        {
            return self.http_get(args).map(Some);
        }
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
        if let Expr::ExternFuncRef { name, .. } = callee
            && matches!(
                self.contract.intrinsics.get(name),
                Some(super::resolve::TypedIntrinsic::DateNew)
            )
        {
            return self.new_date(args).map(Some);
        }
        if let Expr::PropertyGet {
            object, property, ..
        } = callee
        {
            if let Some(kind) = super::time::TimeKind::of(&self.infer_expr_type(object)) {
                return self.time_method(kind, object, property, args).map(Some);
            }
            if super::http::is_response(&self.infer_expr_type(object)) {
                return self.http_header(object, property, args).map(Some);
            }
            if super::date::is_date(&self.infer_expr_type(object)) {
                return self.date_method(object, property, args).map(Some);
            }
            if super::filesystem::is_stats(&self.infer_expr_type(object)) {
                return self.stats_method(object, property, args).map(Some);
            }
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
            if property == "push" && self.is_dense_array(object) {
                ensure!(args.len() == 1, "Typed array push requires one element");
                return self.dense_push(object, &args[0]).map(Some);
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
            if self.contract.wit.is_some()
                && let Some(expected) = expected
            {
                arg_vals.push(self.typed_operand(arg, expected)?);
                continue;
            }
            if let Some(expected) = expected
                && super::nullable::inner(expected).is_some()
            {
                self.check_typed_value(arg, expected)?;
                arg_vals.push(self.value_operand(arg)?);
                continue;
            }
            let argument_type = self.infer_expr_type(arg);
            if let Some(expected) = expected
                && (self.contract.wit.is_some() || matches!(expected, HirType::Tuple(_)))
            {
                self.check_typed_value(arg, expected)?;
            }
            if expected.is_some_and(super::values::is_dynamic) {
                arg_vals.push(self.value_operand(arg)?);
                continue;
            }
            if super::values::is_dynamic(&argument_type) {
                let expected = expected
                    .ok_or_else(|| {
                        anyhow::anyhow!("Dynamic arguments need a declared parameter type")
                    })?
                    .clone();
                arg_vals.push(self.unbox_value(arg, &expected)?);
                continue;
            }
            if expected.is_some_and(super::objects::is_object)
                || super::objects::is_object(&argument_type)
            {
                ensure!(
                    expected.is_some_and(super::objects::is_object)
                        && super::objects::is_object(&argument_type),
                    "Object parameters require object arguments"
                );
            }
            if expected.is_some_and(super::filesystem::is_stats)
                || super::filesystem::is_stats(&argument_type)
                || matches!(expected, Some(HirType::Array(_)))
                || matches!(&argument_type, HirType::Array(_))
            {
                ensure!(
                    expected
                        .is_some_and(|expected| super::wit::same_type(expected, &argument_type)),
                    "Object arguments must match their declared parameter types"
                );
            }
            if expected.is_some_and(super::http::is_response)
                || super::http::is_response(&argument_type)
            {
                ensure!(
                    expected == Some(&argument_type),
                    "HTTP response arguments must match their declared type"
                );
            }
            if expected.is_some_and(super::time::is_time) || super::time::is_time(&argument_type) {
                ensure!(
                    expected == Some(&argument_type),
                    "Temporal arguments must match their declared type"
                );
            }
            if expected.is_some_and(super::date::is_date) || super::date::is_date(&argument_type) {
                ensure!(
                    expected == Some(&argument_type),
                    "Date arguments must match Date parameters"
                );
            }
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
                    expected.is_some_and(|expected| super::text_or_bytes::equivalent(
                        expected,
                        &argument_type
                    )),
                    "Stored Promise arguments must match their declared outcome type"
                );
            }
            let value = if expected.is_some_and(is_text_or_bytes) {
                self.text_or_bytes_operand(arg)?
            } else if expected == Some(&HirType::String) {
                self.string_receiver(arg)?
            } else {
                ensure!(
                    !matches!(&argument_type, HirType::Union(_) | HirType::Void)
                        || super::objects::is_object(&argument_type)
                        || super::values::is_string_type(&argument_type),
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
            let kind = if types::is_reference(&task.result) {
                super::allocation::AllocationKind::ReferencePromise
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
                let payload = self.call_completion(func_idx, &arg_vals);
                if matches!(intrinsic, super::resolve::TypedIntrinsic::WitImport { .. })
                    && let Expr::ExternFuncRef { return_type, .. } = callee
                    && *return_type != HirType::Void
                {
                    let reference = super::registry::map_type_to_waffle(return_type)? == Type::I32;
                    return Ok(Some(abi::decode_payload(
                        &mut self.body,
                        self.block,
                        payload,
                        reference,
                    )));
                }
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
        if super::values::is_dynamic(&self.infer_expr_type(expr)) {
            let value = self.expression(expr)?;
            let payload = self.call_completion(
                self.registry
                    .value_helpers
                    .expect("dynamic values have helpers")
                    .async_result,
                &[value],
            );
            let value = abi::decode_payload(&mut self.body, self.block, payload, true);
            if let Some(runtime) = &self.registry.promises {
                self.op(
                    Operator::Call {
                        function_index: runtime.yield_thread,
                    },
                    &[],
                    &[],
                );
            }
            return Ok(self.continuation(if is_statement { None } else { Some(value) }));
        }
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
                        super::registry::map_type_to_waffle(&result)? == Type::I32,
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
        let transported_integer = |ty: &HirType| {
            *ty == HirType::BigInt || super::nullable::inner(ty) == Some(&HirType::BigInt)
        };
        ensure!(
            !transported_integer(&self.infer_expr_type(expr)),
            "WIT bigint values support transport only; truthiness is unsupported"
        );
        if let Expr::Compare { left, right, .. } = expr {
            let left_type = self.infer_expr_type(left);
            let right_type = self.infer_expr_type(right);
            ensure!(
                !(transported_integer(&left_type)
                    && !matches!(right_type, HirType::Null | HirType::Void))
                    && !(transported_integer(&right_type)
                        && !matches!(left_type, HirType::Null | HirType::Void)),
                "WIT bigint values support transport only; comparisons are unsupported"
            );
        }
        match expr {
            Expr::Bool(b) => {
                let v = if *b { 1 } else { 0 };
                Ok(self.op(Operator::I32Const { value: v }, &[], &[Type::I32]))
            }
            Expr::Unary {
                op: UnaryOp::Not,
                operand,
            } => {
                let value = self.condition(operand)?;
                Ok(self.op(Operator::I32Eqz, &[value], &[Type::I32]))
            }
            Expr::Compare { op, left, right }
                if super::values::is_dynamic(&self.infer_expr_type(left))
                    || super::values::is_dynamic(&self.infer_expr_type(right))
                    || super::nullable::inner(&self.infer_expr_type(left)).is_some()
                    || super::nullable::inner(&self.infer_expr_type(right)).is_some() =>
            {
                self.value_comparison(*op, left, right)
            }
            Expr::Compare { op, left, right }
                if is_text_or_bytes(&self.infer_expr_type(left))
                    || is_text_or_bytes(&self.infer_expr_type(right)) =>
            {
                self.text_or_bytes_comparison(*op, left, right)
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
                if super::values::is_dynamic(&self.infer_expr_type(expr))
                    || super::nullable::inner(&self.infer_expr_type(expr)).is_some()
                {
                    Ok(self.op(
                        Operator::Call {
                            function_index: self
                                .registry
                                .value_helpers
                                .expect("dynamic values have helpers")
                                .truthy,
                        },
                        &[val],
                        &[Type::I32],
                    ))
                } else if is_text_or_bytes(&self.infer_expr_type(expr)) {
                    Ok(self.text_or_bytes_truthiness(val))
                } else if self.is_string(expr) {
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
            Expr::Logical { op, left, right } => self.boolean_logic(*op, left, right),
            Expr::PropertySet { object, .. }
            | Expr::IndexSet { object, .. }
            | Expr::PutValueSet { target: object, .. }
                if super::http::is_response(&self.infer_expr_type(object)) =>
            {
                bail!("HTTP response metadata is read-only")
            }
            Expr::PropertyGet {
                object, property, ..
            } if super::time::is_time(&self.infer_expr_type(object)) => {
                self.time_property(object, property)
            }
            Expr::PropertySet { object, .. } | Expr::IndexSet { object, .. }
                if super::time::is_time(&self.infer_expr_type(object)) =>
            {
                bail!("Temporal values are immutable")
            }
            Expr::JsonParseWithReviver(_, reviver) | Expr::JsonParseReviver { reviver, .. }
                if !matches!(reviver.as_ref(), Expr::Null | Expr::Undefined) =>
            {
                bail!("JSON revivers are unsupported")
            }
            Expr::JsonParse(input)
            | Expr::JsonParseTyped { text: input, .. }
            | Expr::JsonParseWithReviver(input, _)
            | Expr::JsonParseReviver { text: input, .. } => {
                let input = self.string_receiver(input)?;
                let payload =
                    self.call_completion(self.registry.json_helpers.unwrap().parse, &[input]);
                Ok(abi::decode_payload(
                    &mut self.body,
                    self.block,
                    payload,
                    true,
                ))
            }
            Expr::JsonStringifyFull(_, replacer, space)
                if !matches!(replacer.as_ref(), Expr::Null | Expr::Undefined)
                    || !matches!(space.as_ref(), Expr::Null | Expr::Undefined) =>
            {
                bail!("JSON replacers and spacing are unsupported")
            }
            Expr::JsonStringify(input) | Expr::JsonStringifyFull(input, _, _) => {
                let input = self.value_operand(input)?;
                let payload =
                    self.call_completion(self.registry.json_helpers.unwrap().stringify, &[input]);
                Ok(abi::decode_payload(
                    &mut self.body,
                    self.block,
                    payload,
                    true,
                ))
            }
            Expr::IndexSet { object, .. } | Expr::PropertySet { object, .. }
                if matches!(self.infer_expr_type(object), HirType::Tuple(_)) =>
            {
                bail!("Tuple mutation is unsupported; construct a new fixed tuple")
            }
            Expr::PutValueSet {
                target,
                key,
                value,
                receiver,
                ..
            } if self.is_dense_array(target) => {
                ensure!(
                    self.same_object_reference(target, receiver),
                    "Unsupported array assignment receiver"
                );
                self.dense_set(target, key, value)
            }
            Expr::ArrayPush {
                array_id, value, ..
            } => self.dense_push(&Expr::LocalGet(*array_id), value),
            Expr::IndexSet {
                object,
                index,
                value,
            } if self.is_dense_array(object) => self.dense_set(object, index, value),
            Expr::PropertySet { object, .. }
                if matches!(self.infer_expr_type(object), HirType::Array(_)) =>
            {
                bail!("Typed array properties are read-only; use push to append elements")
            }
            Expr::Array(items) => self.new_value_array(items, None),
            Expr::PropertyGet {
                object, property, ..
            } if super::values::has_dynamic_properties(&self.infer_expr_type(object)) => {
                self.dynamic_get(object, &Expr::String(property.clone()))
            }
            Expr::IndexGet { object, index }
                if super::values::has_dynamic_properties(&self.infer_expr_type(object)) =>
            {
                self.dynamic_get(object, index)
            }
            Expr::PropertySet {
                object,
                property,
                value,
            } if super::values::has_dynamic_properties(&self.infer_expr_type(object)) => {
                self.dynamic_set(object, &Expr::String(property.clone()), value)
            }
            Expr::IndexSet {
                object,
                index,
                value,
            } if super::values::has_dynamic_properties(&self.infer_expr_type(object)) => {
                self.dynamic_set(object, index, value)
            }
            Expr::PutValueSet {
                target,
                key,
                value,
                receiver,
                ..
            } if super::values::has_dynamic_properties(&self.infer_expr_type(target))
                && self.same_object_reference(target, receiver) =>
            {
                self.dynamic_set(target, key, value)
            }
            Expr::ArrayIsArray(value) => {
                let value = self.value_operand(value)?;
                let (tag, _) = self.value_parts(value);
                let mut result = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
                for kind in [
                    super::values::ValueTag::Array,
                    super::values::ValueTag::StringArray,
                ] {
                    let expected =
                        self.op(Operator::I32Const { value: kind as u32 }, &[], &[Type::I32]);
                    let matches = self.op(Operator::I32Eq, &[tag, expected], &[Type::I32]);
                    result = self.op(Operator::I32Or, &[result, matches], &[Type::I32]);
                }
                Ok(result)
            }
            Expr::In { property, object }
                if super::values::has_dynamic_properties(&self.infer_expr_type(object)) =>
            {
                let key = self.value_operand(property)?;
                let object = self.value_operand(object)?;
                self.dynamic_has(object, key)
            }
            Expr::ObjectAssign { target, sources } => self.object_assign(target, sources),
            Expr::ObjectKeys(object) => self.object_enumerate(object, false),
            Expr::ObjectValues(object) => self.object_enumerate(object, true),
            Expr::In { property, object } => self.object_has(property, object),
            Expr::Object(_) => self.new_object(expr, None),
            Expr::New { class_name, .. }
                if self.contract.literal_shapes.contains_key(class_name) =>
            {
                self.new_object(expr, None)
            }
            Expr::PropertySet {
                object,
                property,
                value,
            } if super::objects::is_object(&self.infer_expr_type(object)) => {
                self.object_set(object, &Expr::String(property.clone()), value)
            }
            Expr::IndexSet {
                object,
                index,
                value,
            } if super::objects::is_object(&self.infer_expr_type(object)) => {
                self.object_set(object, index, value)
            }
            Expr::PutValueSet {
                target,
                key,
                value,
                receiver,
                ..
            } if super::objects::is_object(&self.infer_expr_type(target))
                && self.same_object_reference(target, receiver) =>
            {
                self.object_set(target, key, value)
            }
            Expr::Delete(_) => {
                bail!("Runtime delete is unsupported by the static TypeScript contract")
            }
            Expr::TypeOf(operand) => self.type_of(operand),
            Expr::Unary {
                op: UnaryOp::Not, ..
            } => self.condition(expr),
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
            Expr::PutValueSet {
                target,
                key,
                value,
                receiver,
                ..
            } if super::bytes::is_byte_view(&self.infer_expr_type(target))
                && matches!((target.as_ref(), receiver.as_ref()), (Expr::LocalGet(target), Expr::LocalGet(receiver)) if target == receiver) =>
            {
                self.byte_set(target, key, value)
            }
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
                let declared = self.local_types.get(id).cloned();
                if let Some(ty) = &declared
                    && super::nullable::inner(ty).is_some()
                {
                    self.check_typed_value(expr, ty)?;
                    let value = self.typed_operand(expr, ty)?;
                    self.locals.insert(*id, value);
                    self.narrowings.remove(id);
                    return Ok(value);
                }
                if let Some(ty) = &declared
                    && (self.contract.wit.is_some() || matches!(ty, HirType::Tuple(_)))
                {
                    self.check_typed_value(expr, ty)?;
                    if matches!(ty, HirType::Tuple(_) | HirType::Array(_))
                        || super::objects::is_object(ty)
                        || (super::values::is_string_type(ty) && *ty != HirType::String)
                    {
                        let value = self.typed_operand(expr, ty)?;
                        self.locals.insert(*id, value);
                        self.narrowings.remove(id);
                        return Ok(value);
                    }
                }
                if self
                    .local_types
                    .get(id)
                    .is_some_and(super::values::is_dynamic)
                {
                    let (original, tag, payload) = self.tagged_value(expr)?;
                    let value = if super::values::is_dynamic(&self.infer_expr_type(expr)) {
                        original
                    } else {
                        self.box_value(tag, payload)
                    };
                    self.locals.insert(*id, value);
                    self.narrowings.remove(id);
                    return Ok(original);
                }
                if self.local_types.get(id).is_some_and(is_text_or_bytes) {
                    let inferred = self.infer_expr_type(expr);
                    ensure!(
                        is_text_or_bytes(&inferred)
                            || inferred == HirType::String
                            || super::bytes::is_byte_view(&inferred),
                        "String-or-byte bindings require string or Uint8Array assignments"
                    );
                    let original = self.expression(expr)?;
                    let stored = self.tag_text_or_bytes(original, &inferred);
                    self.locals.insert(*id, stored);
                    self.narrowings.remove(id);
                    return Ok(original);
                }
                let inferred = self.infer_expr_type(expr);
                ensure!(
                    !is_text_or_bytes(&inferred),
                    "Assigning a string-or-byte value requires a string-or-byte binding"
                );
                if let Some(previous) = self.local_types.get(id)
                    && let Some(kind) =
                        types::identity_kind(previous).or_else(|| types::identity_kind(&inferred))
                {
                    ensure!(
                        super::text_or_bytes::equivalent(previous, &inferred),
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
                self.narrowings.remove(id);
                self.locals.insert(*id, value);
                Ok(value)
            }
            Expr::LocalGet(id) => {
                if self
                    .local_types
                    .get(id)
                    .is_some_and(|ty| super::nullable::inner(ty).is_some())
                    && let Some(narrowed) = self.narrowings.get(id).cloned()
                {
                    let stored = self.locals[id];
                    return self.extract_value(stored, &narrowed);
                }
                let stored = self
                    .locals
                    .get(id)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("Uninitialized local {:?}", id))?;
                Ok(
                    if self
                        .narrowings
                        .get(id)
                        .is_some_and(super::bytes::is_byte_view)
                    {
                        self.text_or_bytes_parts(stored).0
                    } else {
                        stored
                    },
                )
            }
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
            } if super::objects::is_object(&self.infer_expr_type(object)) => {
                self.object_get(object, &Expr::String(property.clone()))
            }
            Expr::PropertyGet {
                object, property, ..
            } if super::http::is_response(&self.infer_expr_type(object)) => {
                self.http_property(object, property)
            }
            Expr::PropertyGet {
                object, property, ..
            } if super::filesystem::is_stats(&self.infer_expr_type(object)) => {
                self.stats_property(object, property)
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
                if is_text_or_bytes(&self.infer_expr_type(object)) {
                    return self.text_or_bytes_length(object);
                }
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
                        matches!(
                            self.infer_expr_type(object),
                            HirType::Tuple(_) | HirType::Array(_)
                        ),
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
                if self.is_dense_array(object) {
                    return self.dense_index(object, index);
                }
                if let HirType::Tuple(types) = self.infer_expr_type(object) {
                    return self.tuple_index(object, index, &types);
                }
                if super::objects::is_object(&self.infer_expr_type(object)) {
                    return self.object_get(object, index);
                }
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
                let left_val = self.numeric_operand(left)?;
                let right_val = self.numeric_operand(right)?;
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
        let ret_val = if ret_val.is_none() && super::values::is_dynamic(self.return_type) {
            let tag = self.op(
                Operator::I32Const {
                    value: super::values::ValueTag::Undefined as u32,
                },
                &[],
                &[Type::I32],
            );
            let payload = self.op(
                Operator::F64Const {
                    value: 0.0f64.to_bits(),
                },
                &[],
                &[Type::F64],
            );
            Some(self.box_value(tag, payload))
        } else {
            ret_val
        };
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
        self.invalidate_narrowings(body);
        if let Some(catch) = catch {
            self.invalidate_narrowings(&catch.body);
        }
        if let Some(finally) = finally {
            self.invalidate_narrowings(finally);
        }
        let incoming_narrowings = self.narrowings.clone();
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
            self.narrowings = incoming_narrowings.clone();

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
            self.narrowings = incoming_narrowings.clone();

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
        self.narrowings = incoming_narrowings;
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
    pool.intern("undefined");
    for class in &hir.classes {
        for field in &class.fields {
            pool.intern(&field.name);
        }
    }
    let mut intern = |expr: &Expr| {
        if let Expr::String(text) = expr {
            pool.intern(text);
        } else if let Expr::Object(fields) = expr {
            for (name, _) in fields {
                pool.intern(name);
            }
        } else if let Expr::PropertyGet { property, .. } | Expr::PropertySet { property, .. } = expr
        {
            pool.intern(property);
        } else if matches!(expr, Expr::TypeOf(_)) {
            for label in ["string", "object", "number", "boolean", "undefined"] {
                pool.intern(label);
            }
        }
    };
    for function in &hir.functions {
        super::visit::visit_function_expressions(function, &mut intern);
    }
    super::visit::visit_statements(&hir.init, &mut intern);
}
