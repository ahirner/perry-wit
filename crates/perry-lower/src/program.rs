//! Module-level lowering: imported built-ins, function signatures and the main function.
use super::*;
use crate::diagnostics::annotate;

pub(super) struct ParamSig<'s> {
    pub(super) source: &'s perry_frontend::hir::ir::Param,
    pub(super) binding: BindingType,
    pub(super) base: BindingType,
}

pub(super) struct Signature<'s> {
    pub(super) name: Identifier,
    /// `async`: callers start it with `perry.async_call` and get a promise of `result`.
    pub(super) is_async: bool,
    pub(super) await_initialization: bool,
    /// Runs the module's top-level statements and static field initializers.
    pub(super) top_level: bool,
    pub(super) params: Vec<ParamSig<'s>>,
    pub(super) returns_never: bool,
    pub(super) result: Option<BindingType>,
}

impl Signature<'_> {
    pub(super) fn parameters(&self) -> (&[ParamSig<'_>], Option<&ParamSig<'_>>) {
        match self.params.split_last() {
            Some((last, fixed)) if last.source.is_rest => (fixed, Some(last)),
            _ => (&self.params, None),
        }
    }

    pub(super) fn carrier_results(&self) -> Vec<TypeHandle> {
        self.result.iter().map(|r| r.carrier).collect()
    }

    pub(super) fn wit_constraints(&self) -> Option<(Vec<TypeHandle>, Vec<TypeHandle>)> {
        (self.params.iter().any(|p| p.binding.integer.is_some())
            || self.result.is_some_and(|r| r.integer.is_some()))
        .then(|| {
            (
                self.params.iter().map(|p| p.binding.wit_type()).collect(),
                self.result.iter().map(|r| r.wit_type()).collect(),
            )
        })
    }
}

pub(super) fn build_module(
    ctx: &mut Context,
    hir: &HirModule,
    foreign: &HashMap<String, crate::hir::Foreign>,
    metadata: &hir_compat::Metadata<'_>,
) -> Result<ModuleOp> {
    let locations = &metadata.locations;
    let module = ModuleOp::new(ctx, identifier(&hir_module_name(hir))?);
    // Subclass instances flow into base-typed bindings, where methods would bind statically
    // and `instanceof` would read the declared type: reject inheritance instead of miscompiling.
    if let Some(class) = hir
        .classes
        .iter()
        .find(|class| class.extends_name.is_some())
    {
        bail!(
            "class {} extends {}: class inheritance is not supported yet",
            class.name,
            class.extends_name.as_deref().unwrap_or_default()
        );
    }
    let shapes = Shapes::new(module, hir, foreign, metadata);
    // Imported built-ins by local name: `setTimeout` from `node:timers/promises`, and the
    // functions of `node:fs/promises`.
    let mut builtins: HashMap<&str, Builtin> = HashMap::new();
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
        if matches!(source, "fs" | "node:fs") {
            return Err(annotate(
                anyhow!(
                    "Unsupported filesystem import {source}: synchronous and callback APIs are not supported; use node:fs/promises"
                ),
                locations.import_span(source),
            ));
        }
        if module.is_none() && builtin {
            return Err(annotate(
                anyhow!(
                    "Unsupported capability import {source}: only node:timers/promises, node:fs/promises and Writable from node:stream are supported so far"
                ),
                locations.import_span(source),
            ));
        }
        let Some(module) = module else { continue };
        for specifier in &import.specifiers {
            let perry_frontend::hir::ir::ImportSpecifier::Named { imported, local } = specifier
            else {
                return Err(annotate(
                    anyhow!("only named imports from {source} are supported yet"),
                    locations.import_span(source),
                ));
            };
            let builtin = Builtin::from_import(module, imported)
                .with_context(|| format!("{imported} from {source} is not supported yet"))
                .map_err(|error| annotate(error, locations.import_span(source)))?;
            builtins.insert(local.as_str(), builtin);
        }
    }
    let mut signatures = HashMap::new();
    for function in &hir.functions {
        if function.is_generator {
            return Err(annotate(
                anyhow!("generator function {} is not supported yet", function.name),
                locations.function_span(&function.name),
            ));
        }

        let mut params = Vec::with_capacity(function.params.len());
        for (idx, param) in function.params.iter().enumerate() {
            if param.is_rest && idx + 1 != function.params.len() {
                return Err(shapes.annotate_local(
                    anyhow!("rest parameter must be last in {}", function.name),
                    param.id,
                ));
            }
            let base_carrier = shapes
                .value_type(ctx, &param.ty)
                .map_err(|error| shapes.annotate_local(error, param.id))
                .with_context(|| format!("parameter {} of {}", param.name, function.name))?;
            let integer = shapes.integer_type(ctx, &param.ty);
            let carrier = if param.default.is_some() && !function.is_exported {
                crate::coerce::admitting(ctx, base_carrier, perry_mir::Absence::Undefined)
            } else {
                base_carrier
            };
            params.push(ParamSig {
                source: param,
                binding: BindingType { carrier, integer },
                base: BindingType {
                    carrier: base_carrier,
                    integer,
                },
            });
        }
        let result_carrier = match (&function.return_type, function.is_async) {
            (HirType::Void, false) => Ok(None),
            // An async function settles its promise with its result; `void` settles `undefined`.
            (HirType::Promise(inner), true) => shapes.settled_type(ctx, inner).map(Some),
            (ty, true) => shapes.settled_type(ctx, ty).map(Some),
            (ty, false) => shapes.value_type(ctx, ty).map(Some),
        }
        .map_err(|error| annotate(error, locations.function_span(&function.name)))
        .with_context(|| format!("result of {}", function.name))?;
        let integer_result = (!function.is_async)
            .then(|| shapes.integer_type(ctx, &function.return_type))
            .flatten();
        let result = result_carrier.map(|carrier| BindingType {
            carrier,
            integer: integer_result,
        });
        signatures.insert(
            function.id,
            Signature {
                is_async: function.is_async,
                await_initialization: function.is_exported
                    && function.is_async
                    && locations.async_init(),
                top_level: false,
                name: identifier(&function.name)?,
                params,
                returns_never: matches!(function.return_type, HirType::Never),
                result,
            },
        );
    }
    // Module initialization of named built-in imports (`Writable` from `node:stream`) has
    // nothing to run (calls lower it to nothing); without other statements there is no main
    // function.
    let has_init = !shapes.static_fields.is_empty()
        || hir.init.iter().any(|statement| {
            !matches!(statement, Stmt::Expr(Expr::Call { callee, .. })
                if hir_compat::is_import_initializer(callee))
        });
    if has_init {
        // Without exported functions, the top-level statements (with top-level `await`) form
        // the body of a command's `wasi:cli/run`. With exported functions they initialize the
        // module once before the first entry point, and `run` only awaits that initialization.
        let persistent = hir.functions.iter().any(|f| f.is_exported);
        let async_init = locations.async_init();
        let settled = BindingType::plain(shapes.settled_type(ctx, &HirType::Void)?);
        let mut entries = vec![(
            Signature {
                name: identifier(if persistent {
                    perry_mir::INITIALIZER
                } else {
                    MAIN
                })?,
                is_async: !persistent || async_init,
                await_initialization: false,
                top_level: true,
                params: vec![],
                returns_never: false,
                result: (!persistent || async_init).then_some(settled),
            },
            hir.init.as_slice(),
        )];
        if persistent {
            entries.push((
                Signature {
                    name: identifier(MAIN)?,
                    is_async: true,
                    await_initialization: async_init,
                    top_level: false,
                    params: vec![],
                    returns_never: false,
                    result: Some(settled),
                },
                &[],
            ));
        }
        for (signature, body) in &entries {
            let func =
                FunctionBuilder::lower_body(ctx, signature, body, &signatures, &shapes, &builtins)
                    .context("lowering top-level statements")?;
            if signature.is_async {
                set_async(ctx, &func);
            }
            module.append_operation(ctx, func.get_operation(), 0);
        }
    }
    for function in &hir.functions {
        let func = FunctionBuilder::lower(ctx, function, &signatures, &shapes, &builtins)
            .map_err(|error| annotate(error, locations.function_span(&function.name)))
            .with_context(|| format!("lowering function {}", function.name))?;
        if let Some(span) = locations.function_span(&function.name) {
            perry_mir::set_source_span(ctx, &func, &span);
        }
        if let Some((wit_params, wit_results)) = signatures[&function.id].wit_constraints() {
            perry_mir::constrain_signature(ctx, &func, wit_params, wit_results);
        }
        if function.is_exported {
            perry_mir::set_exported(ctx, &func);
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
