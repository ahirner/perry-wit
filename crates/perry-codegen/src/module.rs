//! Module lowering: the state shared by all functions of a module, the order in which
//! functions, drivers and boundary glue are emitted, and the raw (test) export wrappers.
use crate::{
    DATA_BASE, Exports, HELPER_MODULE,
    analysis::reachable_functions,
    async_split, boundary,
    function::FunctionLowering,
    host::Host,
    lease, memory,
    repr::{Repr, i32_type, repr, slot_kind, wasm_type},
    runtime::Runtime,
};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use perry_mir::calls::is_exported;
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
    printable::Printable,
    r#type::{TypeHandle, TypedHandle},
};
use pliron_wasm::{
    attrs::{MemKind, NumOpcode},
    control::ReturnOp,
    ops::{CallOp, ImportFuncOp, set_export},
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// The symbol of module initialization, checked to be an identifier at compile time.
const INITIALIZER: Identifier = Identifier::new(perry_mir::globals::INITIALIZER);

/// Static data placed at [DATA_BASE].
#[derive(Default)]
pub(crate) struct Data {
    pub(crate) bytes: Vec<u8>,
    strings: HashMap<String, u32>,
    pub(crate) regexes: HashMap<(String, String), u32>,
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

    /// Pad the data to a multiple of `alignment` and return the address of its end.
    pub(crate) fn align(&mut self, alignment: usize) -> u32 {
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
    /// Interfaces the WIT world imports; `None` when compiling without a world.
    imports: Option<HashSet<&'e str>>,
    /// Statically bound WIT imports by binding name.
    pub(crate) bindings: HashMap<&'e str, &'e boundary::Import>,
    /// The WIT resources the reachable functions use, by key.
    pub(crate) resources: HashMap<u32, &'e boundary::ResourceDefinition>,
    /// Async imports with an outstanding transfer; the index is the transfer's kind.
    pub(crate) foreign_transfers: Vec<&'e boundary::Import>,
    /// Async functions in dispatch-kind order.
    pub(crate) async_functions: Vec<Identifier>,
    /// Resume handlers registered by runtime frame constructors during lowering.
    pub(crate) runtime_frames: BTreeMap<u32, Runtime>,
    pub(crate) shapes: HashMap<String, i32>,
    /// Whether any function writes console output (finish and callbacks then drive it).
    pub(crate) uses_io: bool,
    /// Whether any function starts a `node:fs` operation (callbacks pass it events).
    pub(crate) uses_fs: bool,
    /// Whether the module reads `stream<u8>`s (callbacks pass them events; tasks close them).
    pub(crate) uses_streams: bool,
    /// Whether the module calls `fetch` (callbacks pass it events; tasks close its handles).
    pub(crate) uses_fetch: bool,
    /// `json()` of a body: events also settle the bodies read to their end.
    pub(crate) uses_json_bodies: bool,
    /// Serving `wasi:http/handler`: events advance the exchange, and the export keeps
    /// running after `task.return` until the response body is written.
    pub(crate) uses_http_server: bool,
    /// Operations whose previous results are dead whenever they run again (`lease`).
    pub(crate) leased: HashSet<Ptr<Operation>>,
    /// Generated `JSON.stringify` serializers by MIR type.
    pub(crate) native_descriptors: HashMap<TypeHandle, u32>,
    pub(crate) json_serializers: HashMap<TypeHandle, Identifier>,
    pub(crate) retains_json: bool,
    /// Whether allocations use precise collectible storage rather than task storage.
    pub(crate) collectible: bool,
    pub(crate) program_facts: &'e perry_mir::facts::ProgramFacts,
    pub(crate) live: &'e perry_mir::liveness::LiveValues,
    /// Address of the compiled-roots counter of [Exports::RawChecked].
    pub(crate) root_metrics: Option<u32>,
    pub(crate) memory_checks: bool,
    /// Pointer-map addresses of record types in collectible storage.
    pub(crate) pointer_maps: HashMap<TypeHandle, u32>,
    /// Address and slot type of each module binding.
    pub(crate) globals: HashMap<Identifier, (u32, TypeHandle)>,
    /// The initializer and the address of its state cell, if the module has one.
    pub(crate) initializer: Option<(Identifier, u32)>,
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

/// Lower canonicalized MIR (after `perry_mir::pipeline::canonicalize`) to the `wasm` dialect.
pub(crate) fn lower(
    ctx: &mut Context,
    mir: &ModuleOp,
    exports: &Exports,
    analyses: &mut AnalysisManager,
) -> Result<(ModuleOp, Vec<u8>, bool)> {
    let name = mir.get_symbol_name(ctx);
    let functions = reachable_functions(ctx, mir, exports);
    let async_functions: Vec<Identifier> = functions
        .iter()
        .filter(|f| perry_mir::calls::is_async(ctx, f))
        .map(|f| f.get_symbol_name(ctx))
        .collect();
    analyses
        .get_analysis::<perry_mir::facts::ProgramFacts>(mir.get_operation(), ctx)
        .map_err(|error| anyhow!("{}", error.disp(ctx)))?;
    analyses
        .get_analysis::<perry_mir::liveness::LiveValues>(mir.get_operation(), ctx)
        .map_err(|error| anyhow!("{}", error.disp(ctx)))?;
    let effects = analyses
        .try_get_analysis::<perry_mir::facts::ProgramFacts>(mir.get_operation())
        .expect("runtime effects");
    let live = analyses
        .try_get_analysis::<perry_mir::liveness::LiveValues>(mir.get_operation())
        .expect("liveness");
    let resource_keys = boundary::used_keys(ctx, &functions);
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
        shapes: HashMap::new(),
        uses_io: false,
        uses_fs: false,
        uses_streams: false,
        uses_fetch: false,
        uses_json_bodies: false,
        uses_http_server: false,
        json_serializers: HashMap::new(),
        native_descriptors: HashMap::new(),
        retains_json: functions.iter().any(|function| {
            crate::analysis::function_ops(ctx, function)
                .into_iter()
                .any(|op| {
                    Operation::get_op::<perry_mir::intrinsics::IntrinsicOp>(op, ctx).is_some_and(
                        |op| {
                            op.intrinsic(ctx).is_some_and(|i| {
                                matches!(
                                    i,
                                    perry_mir::intrinsics::Intrinsic::JsonRetain
                                        | perry_mir::intrinsics::Intrinsic::JsonResidue
                                )
                            })
                        },
                    )
                })
        }),
        leased: HashSet::new(),
        collectible: !resource_keys.is_empty()
            || matches!(exports, Exports::RawChecked)
            || memory_checks
            || memory::needs_collector(ctx, &functions, &effects),
        memory_checks,
        program_facts: &effects,
        live: &live,
        root_metrics: matches!(exports, Exports::RawChecked).then_some(DATA_BASE),
        pointer_maps: HashMap::new(),
        globals: HashMap::new(),
        initializer: None,
    };
    if functions
        .iter()
        .any(|f| f.get_symbol_name(ctx) == INITIALIZER)
    {
        let address = state.data.append(8, &[0; 32]);
        state.initializer = Some((INITIALIZER, address));
        state.collectible = true;
    }
    for function in &functions {
        state.leased.extend(lease::leased(ctx, function, &live));
    }
    for function in &functions {
        let symbol = function.get_symbol_name(ctx);
        if let Some(kind) = state.async_functions.iter().position(|f| *f == symbol) {
            async_split::lower(ctx, &mut state, function, kind as i32)
                .with_context(|| format!("lowering async function {symbol} to wasm"))?;
            if raw && is_exported(ctx, function) {
                let wrapper = async_export_wrapper(ctx, &mut state, function, &symbol)?;
                state.wasm.append_operation(ctx, wrapper.get_operation(), 0);
            }
            continue;
        }
        let lowered = FunctionLowering::lower(ctx, &mut state, function)
            .with_context(|| format!("lowering function {symbol} to wasm"))?;
        state.wasm.append_operation(ctx, lowered.get_operation(), 0);
        if raw && is_exported(ctx, function) {
            let wrapper = export_wrapper(ctx, &mut state, &lowered, &symbol)?;
            append_export(ctx, &mut state, &wrapper, &lowered, symbol.as_ref());
        }
    }
    if !state.async_functions.is_empty() {
        async_split::resume(ctx, &mut state);
    }
    if let Exports::Canonical { exports, .. } = exports {
        bind_exports(ctx, &mut state, &functions, exports)?;
    }
    if matches!(exports, Exports::RawChecked) {
        memory::export_checks(ctx, &mut state);
    }
    boundary::emit_drain(ctx, &mut state);
    let memory = !state.data.bytes.is_empty()
        || !state.runtime.is_empty()
        || crate::analysis::module_functions(ctx, &state.wasm)
            .iter()
            .any(|function| {
                crate::analysis::function_ops(ctx, function)
                    .into_iter()
                    .any(|op| {
                        Operation::get_op::<pliron_wasm::ops::LoadOp>(op, ctx).is_some()
                            || Operation::get_op::<pliron_wasm::ops::StoreOp>(op, ctx).is_some()
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

/// Emit the canonical-ABI glue of every WIT export.
fn bind_exports(
    ctx: &mut Context,
    state: &mut ModuleState<'_>,
    functions: &[FuncOp],
    exports: &[boundary::Export],
) -> Result<()> {
    if state.uses_io
        && let Some(export) = exports.iter().find(|e| e.task_return.is_none())
    {
        bail!(
            "console output needs async exports: WASI P3 output streams are asynchronous, but {} is a synchronous export",
            export.core_name
        );
    }
    // A command reports uncaught exceptions on stderr.
    if exports.iter().any(|e| e.command) {
        state.uses_io = true;
    }
    if exports.iter().any(|e| {
        e.params
            .iter()
            .any(|p| matches!(p, boundary::AbiType::ByteStream))
    }) {
        state.uses_streams = true;
    }
    let mir_functions: HashMap<String, FuncOp> = functions
        .iter()
        .map(|f| (f.get_symbol_name(ctx).into(), *f))
        .collect();
    let initialization = state
        .program_facts
        .effects(&Identifier::new(perry_mir::globals::INITIALIZER));
    let initialization_escapes =
        initialization.is_some_and(|summary| summary.effects.failure.escapes());
    let stderr = state.imports.as_ref().is_some_and(|imports| {
        imports
            .iter()
            .any(|name| name.starts_with("wasi:cli/stderr@"))
    });
    for export in exports {
        let function = mir_functions.get(&export.function).ok_or_else(|| {
            anyhow!(
                "WIT export {} has no function {}",
                export.core_name,
                export.function
            )
        })?;
        let effects = &state
            .program_facts
            .effects(&function.get_symbol_name(ctx))
            .expect("function effects")
            .effects;
        if export.task_return.is_some()
            && stderr
            && ((effects.host_work
                || initialization.is_some_and(|summary| summary.effects.host_work))
                && (effects.unhandled_await
                    || initialization.is_some_and(|summary| summary.effects.unhandled_await)))
            && !export
                .result
                .as_ref()
                .is_some_and(boundary::AbiType::accepts_exception)
        {
            state.uses_io = true;
        }
        if export.task_return.is_none()
            && state
                .async_functions
                .iter()
                .any(|name| name.as_ref() == perry_mir::globals::INITIALIZER)
        {
            return Err(perry_wit_diagnostics::PerryDiagnostic::error(
                perry_wit_diagnostics::DiagnosticCode::MissingErrorChannel,
                format!("{} requires asynchronous module initialization", export.core_name))
                .with_help("Declare this WIT export async so it can await module initialization and its error channel.").into());
        }
        if !export.command
            && (initialization_escapes || effects.failure.escapes())
            && !export
                .result
                .as_ref()
                .is_some_and(boundary::AbiType::accepts_exception)
        {
            if export.task_return.is_some() && !stderr {
                let mut diagnostic = perry_wit_diagnostics::PerryDiagnostic::error(
                    perry_wit_diagnostics::DiagnosticCode::MissingErrorChannel,
                    format!("{} can throw without a compatible WIT error channel", export.core_name))
                    .with_help("Declare a WIT result error, or use an async export with wasi:cli/stderr support.");
                diagnostic.span = functions
                    .iter()
                    .find(|function| function.get_symbol_name(ctx).as_ref() == export.function)
                    .and_then(|function| perry_mir::diagnostics::source_span(ctx, function));
                return Err(diagnostic.into());
            }
            if export.task_return.is_some() {
                state.uses_io = true;
            }
        }
    }
    let lowered: HashMap<_, _> = crate::analysis::module_functions(ctx, &state.wasm)
        .into_iter()
        .map(|function| (function.get_symbol_name(ctx), function))
        .collect();
    for export in exports {
        let function = mir_functions.get(&export.function).ok_or_else(|| {
            anyhow!(
                "WIT export {} has no function {}",
                export.core_name,
                export.function
            )
        })?;
        boundary::export(
            ctx,
            state,
            function,
            &lowered[&function.get_symbol_name(ctx)],
            export,
        )
        .with_context(|| format!("binding WIT export {}", export.core_name))?;
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
    let ty = TypedHandle::<FunctionType>::from_handle(inner.get_type(ctx), ctx)
        .map_err(|_| anyhow!("function type"))?;
    let (params, results) = {
        let ty = ty.deref(ctx);
        (ty.arg_types(), ty.res_types())
    };
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
    let ret = ReturnOp::new(ctx, values).get_operation();
    b.insert(ctx, ret);
    for param in params {
        ensure!(
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
    let ty = TypedHandle::<FunctionType>::from_handle(mir.get_type(ctx), ctx)
        .map_err(|_| anyhow!("function type"))?;
    let (params, results) = {
        let ty = ty.deref(ctx);
        (ty.arg_types(), ty.res_types())
    };
    let &[value_ty] = results.as_slice() else {
        bail!("async function {name} returns {} values", results.len());
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
    let ret = ReturnOp::new(ctx, vec![value, status]).get_operation();
    b.insert(ctx, ret);
    Ok(wrapper)
}
