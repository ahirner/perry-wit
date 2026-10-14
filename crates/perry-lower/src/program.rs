//! Module-level lowering: imported built-ins, function signatures and the main function.
use super::*;

/// The HIR runtime call that initializes a named built-in import (`Writable`).
pub(super) const ESM_EXPORT_INIT: &str = "js_native_module_named_esm_export_value";

pub(super) struct Signature {
    pub(super) name: Identifier,
    /// `async`: callers start it with `perry.async_call` and get a promise of `results[0]`.
    pub(super) is_async: bool,
    pub(super) await_initialization: bool,
    pub(super) params: Vec<TypeHandle>,
    pub(super) results: Vec<TypeHandle>,
}

pub(super) fn build_module(
    ctx: &mut Context,
    hir: &HirModule,
    foreign: &HashMap<String, HirType>,
    inferred: HashSet<LocalId>,
    locations: &crate::locations::Locations<'_>,
) -> Result<ModuleOp> {
    let module = ModuleOp::new(ctx, identifier(&hir_module_name(hir))?);
    let shapes = Shapes::new(module, hir, foreign, inferred, locations);
    // Imported built-ins by local name: `setTimeout` from `node:timers/promises`, and the
    // functions of `node:fs/promises`.
    let mut builtins: HashMap<String, Builtin> = HashMap::new();
    for import in &hir.imports {
        let source = import.source.as_str();
        let module = match source {
            "timers/promises" | "node:timers/promises" => Some(BuiltinModule::Timers),
            "fs/promises" | "node:fs/promises" => Some(BuiltinModule::Fs),
            "stream" | "node:stream" => Some(BuiltinModule::Stream),
            "perry:http-server" => Some(BuiltinModule::HttpServer),
            _ => None,
        };
        let builtin = source.starts_with("node:")
            || matches!(
                source,
                "timers" | "fs" | "fs/promises" | "stream" | "process" | "crypto"
            );
        ensure!(
            !matches!(source, "fs" | "node:fs"),
            "Unsupported filesystem import {source}: synchronous and callback APIs are not supported; use node:fs/promises"
        );
        ensure!(
            module.is_some() || !builtin,
            "Unsupported capability import {source}: only node:timers/promises, node:fs/promises and Writable from node:stream are supported so far"
        );
        let Some(module) = module else { continue };
        for specifier in &import.specifiers {
            let perry_hir::ir::ImportSpecifier::Named { imported, local } = specifier else {
                bail!("only named imports from {source} are supported yet");
            };
            let builtin = Builtin::from_import(module, imported)
                .with_context(|| format!("{imported} from {source} is not supported yet"))?;
            builtins.insert(local.clone(), builtin);
        }
    }
    let mut signatures = HashMap::new();
    for function in &hir.functions {
        ensure!(
            !function.is_generator,
            "generator function {} is not supported yet",
            function.name
        );

        let params = function
            .params
            .iter()
            .map(|param| {
                ensure!(
                    param.default.is_none() && !param.is_rest,
                    "default and rest parameters are not supported yet: {}",
                    function.name
                );
                shapes
                    .value_type(ctx, &param.ty)
                    .map_err(|error| shapes.annotate_local(error, param.id))
                    .with_context(|| format!("parameter {} of {}", param.name, function.name))
            })
            .collect::<Result<Vec<_>>>()?;
        let results = match (&function.return_type, function.is_async) {
            (HirType::Void, false) => Ok(vec![]),
            // An async function settles its promise with its result; `void` settles `undefined`.
            (HirType::Promise(inner), true) => shapes.settled_type(ctx, inner).map(|ty| vec![ty]),
            (ty, true) => shapes.settled_type(ctx, ty).map(|ty| vec![ty]),
            (ty, false) => shapes.value_type(ctx, ty).map(|ty| vec![ty]),
        }
        .map_err(|error| locations.annotate_function(error, &function.name))
        .with_context(|| format!("result of {}", function.name))?;
        signatures.insert(
            function.id,
            Signature {
                is_async: function.is_async,
                await_initialization: function.is_exported
                    && function.is_async
                    && locations.async_init(),
                name: identifier(&function.name)?,
                params,
                results,
            },
        );
    }
    // Module initialization of named built-in imports (`Writable` from `node:stream`) has
    // nothing to run (calls lower it to nothing); without other statements there is no main
    // function.
    let has_init = hir.init.iter().any(|statement| {
        !matches!(statement, Stmt::Expr(Expr::Call { callee, .. })
            if matches!(&**callee, Expr::ExternFuncRef { name, .. } if name == ESM_EXPORT_INIT))
    });
    if has_init {
        // Top-level statements (with top-level `await`) form the async main function of a
        // command (`wasi:cli/run`), settling `undefined`.
        let persistent = hir.functions.iter().any(|f| f.is_exported);
        let signature = Signature {
            name: identifier(if persistent {
                perry_mir::globals::INITIALIZER
            } else {
                MAIN
            })?,
            is_async: !persistent || locations.async_init(),
            await_initialization: false,
            params: vec![],
            results: if persistent && !locations.async_init() {
                vec![]
            } else {
                vec![shapes.settled_type(ctx, &HirType::Void)?]
            },
        };
        let func = FunctionBuilder::lower_body(
            ctx,
            &signature,
            &[],
            &hir.init,
            &signatures,
            &shapes,
            &builtins,
        )
        .context("lowering top-level statements")?;
        if signature.is_async {
            set_async(ctx, &func);
        }
        module.append_operation(ctx, func.get_operation(), 0);
    }
    for function in &hir.functions {
        let func = FunctionBuilder::lower(ctx, function, &signatures, &shapes, &builtins)
            .map_err(|error| locations.annotate_function(error, &function.name))
            .with_context(|| format!("lowering function {}", function.name))?;
        if let Some(span) = locations.function_span(&function.name) {
            perry_mir::diagnostics::set_source_span(ctx, &func, &span);
        }
        if function.is_exported {
            perry_mir::calls::set_exported(ctx, &func);
        }
        if function.is_async {
            set_async(ctx, &func);
        }
        module.append_operation(ctx, func.get_operation(), 0);
    }
    Ok(module)
}

fn hir_module_name(hir: &HirModule) -> String {
    let name: String = hir
        .name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        format!("m_{name}")
    } else {
        name
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BuiltinModule {
    Timers,
    Fs,
    Stream,
    /// `perry:http-server`: the services the compiler's `wasi:http/handler` adapter uses.
    HttpServer,
}

/// A function imported from a built-in module.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Builtin {
    SetTimeout,
    ReadFile,
    WriteFile,
    Stat,
    Readdir,
    Mkdir,
    Unlink,
    Rmdir,
    /// The `Writable` class, for `Writable.toWeb`.
    Writable,
    /// A service of `perry:http-server`.
    Serve(Intrinsic),
}

impl Builtin {
    fn from_import(module: BuiltinModule, name: &str) -> Option<Self> {
        Some(match (module, name) {
            (BuiltinModule::Timers, "setTimeout") => Builtin::SetTimeout,
            (BuiltinModule::Fs, "readFile") => Builtin::ReadFile,
            (BuiltinModule::Fs, "writeFile") => Builtin::WriteFile,
            (BuiltinModule::Fs, "stat") => Builtin::Stat,
            (BuiltinModule::Fs, "readdir") => Builtin::Readdir,
            (BuiltinModule::Fs, "mkdir") => Builtin::Mkdir,
            (BuiltinModule::Fs, "unlink") => Builtin::Unlink,
            (BuiltinModule::Fs, "rmdir") => Builtin::Rmdir,
            (BuiltinModule::Stream, "Writable") => Builtin::Writable,
            (BuiltinModule::HttpServer, name) => Builtin::Serve(match name {
                "request" => Intrinsic::ServeRequest,
                "body" => Intrinsic::ServeBody,
                "method" => Intrinsic::ServeMethod,
                "methodOther" => Intrinsic::ServeMethodOther,
                "hasScheme" => Intrinsic::ServeHasScheme,
                "scheme" => Intrinsic::ServeScheme,
                "schemeOther" => Intrinsic::ServeSchemeOther,
                "authority" => Intrinsic::ServeAuthority,
                "path" => Intrinsic::ServePath,
                "headerCount" => Intrinsic::ServeHeaderCount,
                "headerName" => Intrinsic::ServeHeaderName,
                "headerValue" => Intrinsic::ServeHeaderValue,
                "addHeader" => Intrinsic::ServeAddHeader,
                "respond" => Intrinsic::ServeRespond,
                _ => return None,
            }),
            _ => return None,
        })
    }
}
