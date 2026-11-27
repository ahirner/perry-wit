//! Module lowering: the state shared by all functions of a module, the order in which
//! functions, drivers and boundary glue are emitted, and the raw (test) export wrappers.
use crate::{
    DATA_BASE, Exports, HELPER_MODULE,
    aggregates::RecordLayouts,
    analysis::reachable_functions,
    async_split, boundary,
    function::FunctionLowering,
    host::Host,
    lease, memory,
    repr::{Repr, i32_type, repr, slot_kind, wasm_type},
    runtime::Runtime,
};
use perry_mir::{PlironResultExt as _, is_exported};
use perry_wit_diagnostics::{Diagnostic, Result, bail_internal, codes, diag, ensure_unsupported};
use pliron::{
    builtin::{
        op_interfaces::{OneRegionInterface, SingleBlockRegionInterface, SymbolOpInterface},
        ops::{FuncOp, ModuleOp},
        type_interfaces::FunctionTypeInterface,
        types::FunctionType,
    },
    context::{Context, Ptr},
    ident,
    identifier::Identifier,
    linked_list::ContainsLinkedList,
    op::Op,
    operation::Operation,
    pass::AnalysisManager,
    r#type::TypeHandle,
};
use pliron_wasm::{
    attrs::{MemKind, NumOpcode},
    control::ReturnOp,
    ops::{CallOp, ImportFuncOp, set_export},
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// The symbol of module initialization, checked to be an identifier at compile time.
pub(crate) const INITIALIZER: Identifier = Identifier::new(perry_mir::INITIALIZER);

/// Module initialization mode and the address of its state cell in static data.
#[derive(Clone, Copy)]
pub(crate) enum Initializer {
    Sync(u32),
    Async(u32),
}

/// Static data placed at [DATA_BASE].
#[derive(Default)]
pub(crate) struct Data {
    pub(crate) bytes: Vec<u8>,
    strings: HashMap<String, u32>,
    constants: HashMap<(usize, Vec<u8>), u32>,
    pub(crate) regexes: HashMap<perry_mir::RegexLiteral, u32>,
}

impl Data {
    /// A static string object followed by its bytes (layout of [perry_abi::layout::string]).
    pub(crate) fn string(&mut self, text: &str) -> u32 {
        if let Some(&address) = self.strings.get(text) {
            return address;
        }
        let address = self.align(8);
        let scalars = text.chars().count() as u32;
        for word in [
            text.len() as u32,
            scalars,
            address + perry_abi::layout::string::SIZE,
            0,
            0,
            0,
            0,
            0,
        ] {
            self.bytes.extend_from_slice(&word.to_le_bytes());
        }
        self.bytes.extend_from_slice(text.as_bytes());
        self.strings.insert(text.to_string(), address);
        address
    }

    /// Intern immutable data at its required alignment.
    pub(crate) fn constant(&mut self, alignment: usize, bytes: Vec<u8>) -> u32 {
        let key = (alignment, bytes);
        if let Some(&address) = self.constants.get(&key) {
            return address;
        }
        let address = self.append(alignment, &key.1);
        self.constants.insert(key, address);
        address
    }

    pub(crate) fn words(&mut self, words: impl IntoIterator<Item = u32>) -> u32 {
        self.constant(4, words.into_iter().flat_map(u32::to_le_bytes).collect())
    }

    pub(crate) fn pointer_shape(&mut self, offsets: &[u32]) -> u32 {
        if offsets.is_empty() {
            return 0;
        }
        self.words(
            [0, offsets.len() as u32]
                .into_iter()
                .chain(offsets.iter().copied()),
        )
    }

    /// Pad the data to a multiple of `alignment` and return the address of its end.
    fn align(&mut self, alignment: usize) -> u32 {
        while !self.bytes.len().is_multiple_of(alignment) {
            self.bytes.push(0);
        }
        DATA_BASE + self.bytes.len() as u32
    }

    /// Append `bytes` at the next multiple of `alignment` and return their address.
    pub(crate) fn append(&mut self, alignment: usize, bytes: &[u8]) -> u32 {
        let address = self.align(alignment);
        self.bytes.extend_from_slice(bytes);
        address
    }
}

/// What the lowering of a module accumulates across its functions. Configuration borrowed
/// from the [Exports] lives as long as `'e`, so lowering can hold it while emitting.
pub(crate) struct ModuleState<'e> {
    pub(crate) data: Data,
    pub(crate) wasm: ModuleOp,
    pub(crate) runtime: BTreeSet<Runtime>,
    pub(crate) host: BTreeSet<Host>,
    /// Interfaces imported by the WIT world, or `None` when compiling a standalone core module.
    imports: Option<HashSet<&'e str>>,
    /// Statically bound WIT imports by binding name.
    pub(crate) bindings: HashMap<&'e str, &'e boundary::Import>,
    /// The WIT resources the reachable functions use, by key.
    pub(crate) resources: HashMap<u32, &'e boundary::ResourceDefinition>,
    /// Async imports with an outstanding transfer, indexed by transfer kind.
    pub(crate) foreign_transfers: Vec<&'e boundary::Import>,
    /// Async functions in dispatch-kind order.
    pub(crate) async_functions: Vec<Identifier>,
    /// Resume handlers registered by runtime frame constructors during lowering.
    pub(crate) runtime_frames: BTreeMap<u32, Runtime>,
    /// Whether any function writes console output (finish and callbacks then drive it).
    pub(crate) uses_io: bool,
    /// Whether any async library export reports uncaught rejections to stderr.
    pub(crate) reports_rejection: bool,
    /// Whether any function starts a `node:fs` operation (callbacks pass it events).
    pub(crate) uses_fs: bool,
    /// Whether the module reads `stream<u8>` handles that receive callback events and close with the task.
    pub(crate) uses_streams: bool,
    /// Whether the module calls `fetch`, routing callback events and closing handles with the task.
    pub(crate) uses_fetch: bool,
    /// `json()` of a body: events also settle the bodies read to their end.
    pub(crate) uses_json_bodies: bool,
    /// Serving `wasi:http/handler`: events advance the exchange, and the export keeps
    /// running after `task.return` until the response body is written.
    pub(crate) uses_http_server: bool,
    /// Operations whose previous results are dead whenever they run again (`lease`).
    pub(crate) leased: HashSet<Ptr<Operation>>,
    pub(crate) layout_descriptors: HashMap<TypeHandle, u32>,
    pub(crate) record_layouts: &'e RecordLayouts,
    pub(crate) wit_slots: crate::boundary::WitSlots,
    pub(crate) json_copy_plans: HashMap<(TypeHandle, TypeHandle), u32>,
    pub(crate) json_result: Option<u32>,
    pub(crate) json_write_data: Option<u32>,
    pub(crate) retains_json: bool,
    /// Whether allocations use precise collectible storage.
    pub(crate) collectible: bool,
    pub(crate) program_facts: &'e perry_mir::ProgramFacts,
    pub(crate) live: &'e perry_mir::LiveValues,
    /// Whether to count compiled roots published at [DATA_BASE] for [Exports::RawChecked].
    pub(crate) count_roots: bool,
    pub(crate) memory_checks: bool,
    /// Address and slot type of each module binding.
    pub(crate) globals: HashMap<Identifier, (u32, TypeHandle)>,
    /// The initializer and the address of its state cell, if the module has one.
    pub(crate) initializer: Option<Initializer>,
    /// Static scratch cell for runtime calls returning a 64-bit numeric payload.
    pub(crate) numeric_result: Option<u32>,
}

impl ModuleState<'_> {
    pub(crate) fn may_throw(&self, function: &Identifier) -> bool {
        self.program_facts
            .effects(function)
            .is_none_or(|summary| summary.effects.failure.throws())
    }

    /// Whether the WIT world imports `interface` (always, without a world).
    pub(crate) fn can_import(&self, interface: &str) -> bool {
        self.imports
            .as_ref()
            .is_none_or(|imports| imports.contains(interface))
    }
}

/// Lower canonicalized MIR (after `perry_mir::canonicalize`) to the `wasm` dialect.
pub(crate) fn lower(
    ctx: &mut Context,
    mir: &ModuleOp,
    exports: &Exports,
    analyses: &mut AnalysisManager,
    allocation_report: Option<&mut String>,
) -> Result<(ModuleOp, Vec<u8>, bool)> {
    let name = mir.get_symbol_name(ctx);
    let functions = reachable_functions(ctx, mir, exports);
    let async_functions: Vec<Identifier> = functions
        .iter()
        .filter(|f| perry_mir::is_async(ctx, f))
        .map(|f| f.get_symbol_name(ctx))
        .collect();
    let root = mir.get_operation();
    analyses
        .compute_analysis::<perry_mir::ProgramFacts>(root, ctx)
        .to_diag(ctx)?;
    analyses
        .compute_analysis::<perry_mir::LiveValues>(root, ctx)
        .to_diag(ctx)?;
    let effects = analyses
        .try_get_analysis::<perry_mir::ProgramFacts>(root)
        .ok_or_else(|| Diagnostic::internal("computed program facts are missing"))?;
    let live = analyses
        .try_get_analysis::<perry_mir::LiveValues>(root)
        .ok_or_else(|| Diagnostic::internal("computed liveness is missing"))?;
    let layouts = RecordLayouts::prepare(ctx, &functions)?;
    let resource_keys = &layouts.resources;
    let raw = matches!(exports, Exports::Raw | Exports::RawChecked);
    let memory_checks = matches!(
        exports,
        Exports::Canonical {
            memory_checks: true,
            ..
        }
    );
    let mut state = ModuleState {
        data: Data {
            bytes: if matches!(exports, Exports::RawChecked) {
                vec![0; 8]
            } else {
                vec![]
            },
            ..Data::default()
        },
        wasm: ModuleOp::new(ctx, name),
        runtime: BTreeSet::new(),
        host: BTreeSet::new(),
        imports: match exports {
            Exports::Raw | Exports::RawChecked => None,
            Exports::Canonical { imports, .. } => {
                Some(imports.iter().map(String::as_str).collect())
            }
        },
        bindings: match exports {
            Exports::Canonical { bindings, .. } => bindings
                .iter()
                .map(|import| (import.binding.as_str(), import))
                .collect(),
            _ => HashMap::new(),
        },
        resources: match exports {
            Exports::Canonical { resources, .. } => resources
                .iter()
                .filter(|r| resource_keys.contains(&r.key))
                .map(|r| (r.key, r))
                .collect(),
            _ => HashMap::new(),
        },
        foreign_transfers: Vec::new(),
        async_functions,
        runtime_frames: BTreeMap::new(),
        uses_io: false,
        reports_rejection: false,
        uses_fs: false,
        uses_streams: false,
        uses_fetch: false,
        uses_json_bodies: false,
        uses_http_server: false,
        layout_descriptors: HashMap::new(),
        json_copy_plans: HashMap::new(),
        json_result: None,
        json_write_data: None,
        retains_json: functions.iter().any(|function| {
            perry_mir::operations(ctx, function).into_iter().any(|op| {
                if Operation::get_op::<perry_mir::FieldSetOp>(op, ctx)
                    .is_some_and(|set| set.updates_order(ctx))
                {
                    return true;
                }
                perry_mir::is_any::<(perry_mir::JsonDecodeOp, perry_mir::JsonCopyOp)>(ctx, op)
                    || Operation::get_op::<perry_mir::IntrinsicOp>(op, ctx).is_some_and(|op| {
                        matches!(
                            op.intrinsic(ctx),
                            perry_mir::Intrinsic::JsonSpread
                                | perry_mir::Intrinsic::JsonOrderCopy
                                | perry_mir::Intrinsic::JsonInsert
                                | perry_mir::Intrinsic::JsonExclude
                        )
                    })
            })
        }),
        leased: HashSet::new(),
        collectible: !resource_keys.is_empty()
            || matches!(exports, Exports::RawChecked)
            || memory_checks
            || memory::needs_collector(ctx, &functions, &effects)?,
        memory_checks,
        program_facts: &effects,
        live: &live,
        count_roots: matches!(exports, Exports::RawChecked),
        record_layouts: &layouts,
        wit_slots: Default::default(),
        globals: HashMap::new(),
        initializer: None,
        numeric_result: None,
    };
    if functions
        .iter()
        .any(|f| f.get_symbol_name(ctx) == INITIALIZER)
    {
        let address = state.data.append(8, &[0; 32]);
        state.initializer = Some(if state.async_functions.contains(&INITIALIZER) {
            Initializer::Async(address)
        } else {
            Initializer::Sync(address)
        });
    }
    for function in &functions {
        state.leased.extend(lease::leased(ctx, function, &live));
    }
    if let Some(report) = allocation_report {
        memory::explain_tiers(ctx, mir, &functions, &state, report)?;
    }
    let mut lowered_functions = HashMap::with_capacity(functions.len());
    for function in functions {
        let symbol = function.get_symbol_name(ctx);
        let wrap_err = |ctx: &Context, err: Diagnostic, phase: &str| {
            let err = err
                .with_fallback_span(perry_mir::source_span(ctx, &function))
                .with_internal_context(format!("lowering {phase} {symbol} to wasm"));
            perry_mir::attach_caller_notes(ctx, &function, err)
        };
        let wasm = if let Some(kind) = state.async_functions.iter().position(|f| *f == symbol) {
            let ramp = async_split::lower(ctx, &mut state, &function, kind as i32)
                .map_err(|err| wrap_err(ctx, err, "async function"))?;
            if raw && is_exported(ctx, &function) {
                let wrapper = async_export_wrapper(ctx, &mut state, &function, &symbol)?;
                state.wasm.append_operation(ctx, wrapper.get_operation(), 0);
            }
            ramp
        } else {
            let lowered = FunctionLowering::lower(ctx, &mut state, &function)
                .map_err(|err| wrap_err(ctx, err, "function"))?;
            state.wasm.append_operation(ctx, lowered.get_operation(), 0);
            if raw && is_exported(ctx, &function) {
                let wrapper = export_wrapper(ctx, &mut state, &lowered, &symbol)?;
                append_export(ctx, &mut state, &wrapper, &lowered, symbol.as_ref());
            }
            lowered
        };
        lowered_functions.insert(
            symbol.into(),
            LoweredFunction {
                mir: function,
                wasm,
            },
        );
    }
    if !state.async_functions.is_empty() {
        async_split::resume(ctx, &mut state);
    }
    if let Exports::Canonical { exports, .. } = exports {
        bind_exports(ctx, &mut state, &lowered_functions, exports)?;
    }
    if matches!(exports, Exports::RawChecked) {
        memory::export_checks(ctx, &mut state);
    }
    boundary::emit_drain(ctx, &mut state);
    let memory = !state.data.bytes.is_empty()
        || !state.runtime.is_empty()
        || perry_mir::functions(state.wasm.get_operation(), ctx)
            .iter()
            .any(|function| {
                perry_mir::operations(ctx, function).into_iter().any(|op| {
                    perry_mir::is_any::<(pliron_wasm::ops::LoadOp, pliron_wasm::ops::StoreOp)>(
                        ctx, op,
                    )
                })
            });
    // The imports come last: every function above may still add to them.
    for runtime in std::mem::take(&mut state.runtime) {
        let (params, results) = runtime.signature(ctx);
        let ty = FunctionType::get(ctx, params, results);
        let import = ImportFuncOp::new(ctx, runtime.symbol(), ty, HELPER_MODULE, runtime.name());
        state.wasm.append_operation(ctx, import.get_operation(), 0);
    }
    for host in std::mem::take(&mut state.host) {
        let (params, results) = host.signature(ctx);
        let ty = FunctionType::get(ctx, params, results);
        let symbol = host.symbol();
        let (module, field) = host.into_path();
        let import = ImportFuncOp::new(ctx, symbol, ty, &module, &field);
        state.wasm.append_operation(ctx, import.get_operation(), 0);
    }
    Ok((state.wasm, state.data.bytes, memory))
}

/// A MIR function paired with its lowered Wasm entry-point function (or async ramp).
#[derive(Clone, Copy)]
struct LoweredFunction {
    mir: FuncOp,
    wasm: FuncOp,
}

/// Emit the canonical-ABI glue of every WIT export.
fn bind_exports(
    ctx: &mut Context,
    state: &mut ModuleState<'_>,
    lowered_functions: &HashMap<String, LoweredFunction>,
    exports: &[boundary::Export],
) -> Result<()> {
    if state.uses_io
        && let Some(export) = exports.iter().find(|e| !e.is_async())
    {
        return Err(diag!(
            codes::ASYNC_MISMATCH,
            "console output needs async exports: WASI P3 output streams are asynchronous, but {} is a synchronous export",
            export.core_name
        )
        .with_optional_span(
            lowered_functions
                .get(&export.function)
                .and_then(|function| perry_mir::source_span(ctx, &function.mir)),
        )
        .with_help(
            "Declare the WIT export async or remove console output from this component.",
        ));
    }
    // A command reports uncaught exceptions on stderr.
    if exports.iter().any(boundary::Export::is_command) {
        state.uses_io = true;
    }
    if exports.iter().any(|e| {
        e.params
            .iter()
            .any(|p| matches!(p.ty.kind, boundary::AbiKind::ByteStream))
    }) {
        state.uses_streams = true;
    }
    let initialization = state
        .program_facts
        .effects(&Identifier::new(perry_mir::INITIALIZER));
    let initialization_escapes =
        initialization.is_some_and(|summary| summary.effects.failure.escapes());
    // Entry points wait for an asynchronous initializer, but not for work a synchronous one starts.
    if matches!(state.initializer, Some(Initializer::Sync(_)))
        && initialization
            .is_some_and(|summary| summary.effects.may_suspend || summary.effects.host_work)
    {
        return Err(Diagnostic::new(
            codes::ASYNC_MISMATCH,
            "top-level statements leave asynchronous work pending",
        )
        .with_optional_span(
            lowered_functions
                .get(perry_mir::INITIALIZER)
                .and_then(|function| perry_mir::source_span(ctx, &function.mir)),
        )
        .with_help(
            "Await the asynchronous work in the top-level statements, so every entry point waits for it.",
        ));
    }
    let stderr = state.can_import("wasi:cli/stderr@0.3.0");
    let mut resolved = Vec::with_capacity(exports.len());
    for export in exports {
        let &lowered = lowered_functions.get(&export.function).ok_or_else(|| {
            diag!(
                codes::MISSING_EXPORT,
                "WIT export {} has no function {}",
                export.core_name,
                export.function
            )
        })?;
        let effects = &state
            .program_facts
            .function_summary(ctx, &lowered.mir)
            .to_diag(ctx)?
            .effects;
        if export.is_async()
            && stderr
            && ((effects.host_work
                || initialization.is_some_and(|summary| summary.effects.host_work))
                && (effects.unhandled_await
                    || initialization.is_some_and(|summary| summary.effects.unhandled_await)))
            && export.exception_result().is_none()
        {
            state.reports_rejection = true;
        }
        if !export.is_async() && matches!(state.initializer, Some(Initializer::Async(_))) {
            return Err(diag!(
                codes::ASYNC_MISMATCH,
                "{} requires asynchronous module initialization",
                export.core_name
            )
            .with_optional_span(perry_mir::source_span(ctx, &lowered.mir))
            .with_help(
                "Declare this WIT export async so it can await module initialization and its error channel.",
            ));
        }
        if !export.is_command()
            && (initialization_escapes || effects.failure.escapes())
            && export.exception_result().is_none()
        {
            if export.is_async() && !stderr {
                return Err(diag!(
                    codes::MISSING_ERROR_CHANNEL,
                    "{} can throw without a compatible WIT error channel",
                    export.core_name
                )
                .with_optional_span(perry_mir::source_span(ctx, &lowered.mir))
                .with_help(
                    "Declare a WIT result error, or use an async export with wasi:cli/stderr support.",
                ));
            }
            if export.is_async() {
                state.reports_rejection = true;
            }
        }
        resolved.push((export, lowered));
    }
    for (export, lowered) in resolved {
        boundary::export(ctx, state, &lowered.mir, &lowered.wasm, export).map_err(|source| {
            source.wrap_boundary(
                perry_mir::source_span(ctx, &lowered.mir),
                format!("while binding WIT export {}", export.core_name),
                format!("binding WIT export {}", export.core_name),
            )
        })?;
    }
    Ok(())
}

/// A generated export whose entire body forwards its arguments and results contributes
/// only an export name. All marshalling, initialization and cleanup remain in real stubs.
pub(crate) fn append_export(
    ctx: &mut Context,
    state: &mut ModuleState,
    wrapper: &FuncOp,
    inner: &FuncOp,
    name: &str,
) {
    let transparent = (|| {
        let region = wrapper.get_region(ctx);
        let region = region.deref(ctx);
        let mut blocks = region.iter(ctx);
        let block = blocks.next()?;
        if blocks.next().is_some() || wrapper.get_type(ctx) != inner.get_type(ctx) {
            return None;
        }
        let block = block.deref(ctx);
        let mut ops = block.iter(ctx);
        let call = Operation::get_op::<CallOp>(ops.next()?, ctx)?;
        let ret = Operation::get_op::<ReturnOp>(ops.next()?, ctx)?;
        (ops.next().is_none()
            && call.get_attr_wasm_callee(ctx)?.as_ref() == &inner.get_symbol_name(ctx)
            && call
                .get_operation()
                .deref(ctx)
                .operands()
                .eq(block.arguments())
            && ret
                .get_operation()
                .deref(ctx)
                .operands()
                .eq(call.get_operation().deref(ctx).results()))
        .then_some(())
    })()
    .is_some();
    if transparent {
        set_export(ctx, inner, name);
        Operation::erase(wrapper.get_operation(), ctx);
    } else {
        set_export(ctx, wrapper, name);
        state.wasm.append_operation(ctx, wrapper.get_operation(), 0);
    }
}

/// `export_<name>`, exported as `<name>`: runs the function as one task.
fn export_wrapper(
    ctx: &mut Context,
    state: &mut ModuleState<'_>,
    inner: &FuncOp,
    name: &Identifier,
) -> Result<FuncOp> {
    let ty = crate::analysis::function_type(ctx, inner)?;
    let (params, results) = (ty.deref(ctx).arg_types(), ty.deref(ctx).res_types());
    let wrapper = FuncOp::new(ctx, ident!("export_") + name.clone(), ty);
    set_export(ctx, &wrapper, name.as_ref());
    let entry = wrapper.get_entry_block(ctx);
    let args: Vec<_> = entry.deref(ctx).arguments().collect();
    let mut b = FunctionLowering::glue(state, ctx, &wrapper);
    let scope = boundary::Scope::for_function(b.state, name);
    if scope == boundary::Scope::Task {
        b.task_enter(ctx);
    }
    b.initialize_module(ctx);
    let call = CallOp::new(ctx, name.clone(), args, results).get_operation();
    b.insert(ctx, call);
    let values: Vec<_> = call.deref(ctx).results().collect();
    if scope == boundary::Scope::Task {
        b.task_free(ctx);
    }
    b.ret(ctx, values);
    for param in params {
        ensure_unsupported!(
            repr(ctx, param)? != Repr::Ref,
            "export {name} takes a reference; reference arguments need the component boundary (P6.1)"
        );
    }
    Ok(wrapper)
}

/// Raw export of an async function: run it to completion without a host and return its value
/// and status (0 fulfilled, the exception code if rejected, 99 if still pending).
fn async_export_wrapper(
    ctx: &mut Context,
    state: &mut ModuleState<'_>,
    mir: &FuncOp,
    name: &Identifier,
) -> Result<FuncOp> {
    let (params, results) = crate::analysis::function_signature(ctx, mir)?;
    let &[value_ty] = results.as_slice() else {
        bail_internal!("async function {name} returns {} values", results.len());
    };
    let wasm_params = params
        .iter()
        .map(|t| wasm_type(ctx, *t))
        .collect::<Result<Vec<_>>>()?;
    let value_wasm = wasm_type(ctx, value_ty)?;
    let i32 = i32_type(ctx);
    let fn_ty = FunctionType::get(ctx, wasm_params, vec![value_wasm, i32]);
    let wrapper = FuncOp::new(ctx, ident!("export_") + name.clone(), fn_ty);
    set_export(ctx, &wrapper, name.as_ref());
    let args: Vec<_> = wrapper
        .get_entry_block(ctx)
        .deref(ctx)
        .arguments()
        .collect();
    let mut b = FunctionLowering::glue(state, ctx, &wrapper);
    b.task_enter(ctx);
    b.initialize_module(ctx);
    let call = CallOp::new(ctx, name.clone(), args, vec![i32]).get_operation();
    b.insert(ctx, call);
    let promise = call.deref(ctx).get_result(0);
    b.runtime_void(ctx, Runtime::TaskSetResult, vec![promise]);
    let settled = b.load(
        ctx,
        MemKind::I32,
        promise,
        perry_abi::layout::promise::STATE,
    );
    let fulfilled_state = b.i32_const(ctx, perry_abi::layout::promise::FULFILLED as i32);
    let rejected_state = b.i32_const(ctx, perry_abi::layout::promise::REJECTED as i32);
    let fulfilled = b.num(ctx, NumOpcode::I32Eq, vec![settled, fulfilled_state]);
    let rejected = b.num(ctx, NumOpcode::I32Eq, vec![settled, rejected_state]);
    let reason = b.load(
        ctx,
        MemKind::I32,
        promise,
        perry_abi::layout::promise::STATUS,
    );
    let pending = b.i32_const(ctx, 99);
    let failure = b.select(ctx, rejected, reason, pending);
    let zero = b.i32_const(ctx, 0);
    let status = b.select(ctx, fulfilled, zero, failure);
    let value = b.load(
        ctx,
        slot_kind(ctx, value_ty)?,
        promise,
        perry_abi::layout::promise::VALUE,
    );
    b.task_free(ctx);
    b.ret(ctx, vec![value, status]);
    Ok(wrapper)
}
