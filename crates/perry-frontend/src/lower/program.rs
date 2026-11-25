//! Module-level lowering: imported built-ins, function signatures and the main function.
use super::*;
use pliron::builtin::op_interfaces::SymbolOpInterface;

#[derive(Clone, Copy)]
pub(super) enum Parameter<'s> {
    Source(FuncId, &'s perry_hir::ir::Param),
    Receiver,
    Environment(crate::engine::types::FuncIdentity),
    Native,
}

pub(super) struct ParamSig<'s> {
    pub(super) origin: Parameter<'s>,
    pub(super) is_rest: bool,
    pub(super) is_never: bool,
    pub(super) binding: BindingType,
    pub(super) base: BindingType,
}

impl<'s> ParamSig<'s> {
    pub(super) fn default(&self) -> Option<(FuncId, &'s Expr)> {
        match self.origin {
            Parameter::Source(owner, param) => param.default.as_ref().map(|expr| (owner, expr)),
            _ => None,
        }
    }

    fn native(
        shapes: &Shapes<'_>,
        ty: crate::engine::NativeParam,
        origin: Parameter<'s>,
    ) -> Result<Self> {
        let (binding, is_never) = shapes.native_param(ty)?;
        Ok(Self {
            origin,
            binding,
            base: binding,
            is_never,
            is_rest: false,
        })
    }
}

pub(super) struct Signature<'s> {
    pub(super) inline: Option<&'s Function<'s>>,
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
            Some((last, fixed)) if last.is_rest => (fixed, Some(last)),
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

pub(crate) fn build_module(
    ctx: &mut Context,
    declarations: &crate::source::ModuleDeclarations,
    world: &crate::wit::WorldTypes<'_>,
    locations: &mut hir_compat::Locations<'_>,
    witnesses: &[hir_compat::Witness<'_, FuncId>],
    specialization: &crate::engine::Specialization,
    layouts: (crate::engine::types::TypeTable, perry_mir::RecordLayouts),
) -> Result<LoweredProgram> {
    let bodies = &specialization.bodies;
    let mut functions = Vec::new();
    let mut command = None;
    let index = bodies.index(locations, &specialization.instances);
    let module = ModuleOp::new(ctx, identifier(&module_name(&declarations.name))?);
    // Subclass instances flow into base-typed bindings, where methods would bind statically
    // and `instanceof` would read the declared type: reject inheritance instead of miscompiling.
    if let Some((class, base)) = bodies
        .classes
        .iter()
        .find_map(|class| Some((&class.name, class.declaration.extends_name.as_deref()?)))
    {
        bail_unsupported!("class {class} extends {base}: class inheritance is not supported yet");
    }
    let decls = crate::shapes::Declarations::new(
        &declarations.interfaces,
        &declarations.type_aliases,
        &declarations.imports,
        bodies
            .classes
            .iter()
            .map(crate::shapes::ClassShape::Instance),
        world,
    );
    let (mut types, records) = layouts;
    let shapes = Shapes::new(
        module,
        declarations,
        &decls,
        locations,
        specialization,
        (&mut types, records),
        index,
    );
    // Imported built-ins by local name: `setTimeout` from `node:timers/promises`, and the
    // functions of `node:fs/promises`.
    let mut builtins: HashMap<&str, Builtin> = HashMap::new();
    for import in &declarations.imports {
        let source = import.source.as_str();
        if hir_compat::is_timer_module(source) {
            continue;
        }
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
            return Err(unsupported!(
                "Unsupported filesystem import {source}: synchronous and callback APIs are not supported; use node:fs/promises"
            )
            .with_fallback_span(locations.import_span(source)));
        }
        if module.is_none() && builtin {
            return Err(unsupported!(
                "Unsupported capability import {source}: supported Node imports are callback timers from node:timers, setTimeout from node:timers/promises, asynchronous filesystem operations from node:fs/promises and Writable from node:stream"
            )
            .with_fallback_span(locations.import_span(source)));
        }
        let Some(module) = module else { continue };
        for specifier in &import.specifiers {
            let perry_hir::ir::ImportSpecifier::Named { imported, local } = specifier else {
                return Err(
                    unsupported!("only named imports from {source} are supported yet")
                        .with_fallback_span(locations.import_span(source)),
                );
            };
            let builtin = Builtin::from_import(module, imported).ok_or_else(|| {
                unsupported!("{imported} from {source} is not supported yet")
                    .with_fallback_span(locations.import_span(source))
            })?;
            builtins.insert(local.as_str(), builtin);
        }
    }
    locations.set_mir_frames(ctx, &module);
    let mut shapes = shapes.prepare(ctx);
    shapes.define_environments(ctx)?;
    let mut signatures = Functions::default();
    for function in &bodies.functions {
        let display_name = locations.display_function_name(&function.name);
        if function.is_generator {
            return Err(locations.annotate_caller(
                unsupported!("generator function {display_name} is not supported yet")
                    .with_fallback_span(locations.function_span(&function.name)),
                &function.name,
            ));
        }

        let mut params = Vec::with_capacity(function.params.len());
        if let Some(instance) = &function.signature {
            if let Some(env) = instance.environment {
                params.push(ParamSig::native(
                    &shapes,
                    crate::engine::NativeParam::Environment(env),
                    Parameter::Environment(env),
                )?);
            }
            if let Some(receiver) = instance.receiver {
                params.push(ParamSig::native(
                    &shapes,
                    crate::engine::NativeParam::Value(receiver),
                    Parameter::Receiver,
                )?);
            }
        }
        for (idx, &(param, solved)) in function.params.iter().enumerate() {
            if param.is_rest && idx + 1 != function.params.len() {
                return Err(locations.annotate_caller(
                    shapes.annotate_local(
                        diag!(
                            codes::PARSE_ERROR,
                            "rest parameter must be last in {display_name}"
                        ),
                        param.id,
                    ),
                    &function.name,
                ));
            }
            let (base, is_never) = shapes.param_type(solved, &param.ty).map_err(|error| {
                let source = declarations
                    .locals
                    .get(&param.id)
                    .and_then(|span| locations.source_at(span.span.lo.0));
                let label = if source.is_some_and(|source| source.starts_with(['{', '['])) {
                    format!("{}", idx + 1)
                } else {
                    format!("'{}'", param.name)
                };
                let error = shapes
                    .annotate_local(error, param.id)
                    .with_note(format!("in parameter {label} of '{display_name}'"));
                locations.annotate_caller(error, &function.name)
            })?;

            let carrier = if param.default.is_some() && !function.is_exported {
                crate::lower::coerce::admitting(ctx, base.carrier, perry_mir::Absence::Undefined)
            } else {
                base.carrier
            };
            params.push(ParamSig {
                is_rest: param.is_rest,
                origin: Parameter::Source(function.id, param),
                is_never,
                binding: BindingType { carrier, ..base },
                base,
            });
        }
        let (result, returns_never) = shapes.function_result(function).map_err(|error| {
            let origin = locations.function_span(&function.name);
            let declaration = locations.declaration_span(&function.name);
            let note = format!("in return type of '{display_name}'");
            let error = if let Some(span) = declaration.filter(|span| Some(span) != origin.as_ref())
            {
                error
                    .with_fallback_span(origin.or_else(|| Some(span.clone())))
                    .with_spanned_note(span, note)
            } else {
                error.with_fallback_span(origin).with_note(note)
            };
            locations.annotate_caller(error, &function.name)
        })?;
        signatures.insert(
            function.id,
            Callee::Source(
                function,
                Signature {
                    inline: (function
                        .signature
                        .as_ref()
                        .is_some_and(|s| s.inline.is_some())
                        || function
                            .captures
                            .iter()
                            .any(|id| !shapes.globals.contains_key(id)))
                    .then_some(function),
                    is_async: function.is_async,
                    await_initialization: function.is_exported
                        && function.is_async
                        && declarations.has_top_level_await,
                    top_level: false,
                    name: identifier(&function.name)?,
                    params,
                    returns_never,
                    result,
                },
            ),
        );
    }
    for &(id, ref function) in &bodies.native {
        let annotate = |err: Diagnostic| {
            locations.annotate_caller(
                err.with_fallback_span(locations.function_span(&function.name)),
                &function.name,
            )
        };
        let params = function
            .params
            .iter()
            .map(|param| ParamSig::native(&shapes, *param, Parameter::Native))
            .collect::<Result<_>>()
            .map_err(&annotate)?;
        let result = shapes
            .solved_result(function.result, function.is_async)
            .map_err(annotate)?;
        signatures.insert(
            id,
            Callee::Native(
                function,
                Signature {
                    inline: None,
                    name: identifier(&function.name)?,
                    is_async: function.is_async,
                    await_initialization: false,
                    top_level: false,
                    params,
                    returns_never: false,
                    result,
                },
            ),
        );
    }
    for class in bodies.classes.iter() {
        for function in class
            .methods
            .iter()
            .chain(&class.static_methods)
            .chain(&class.constructor)
            .chain(class.getters.iter().chain(&class.setters).map(|(_, f)| f))
        {
            signatures.insert(function.id, Callee::Member(&class.name, function));
        }
    }
    // Module initialization of named built-in imports (`Writable` from `node:stream`) has
    // nothing to run (calls lower it to nothing); without other statements there is no main
    // function.
    let has_init = bodies
        .classes
        .iter()
        .any(|class| !class.static_fields.is_empty())
        || bodies.init.iter().any(|statement| {
            !matches!(statement, Stmt::Expr(Expr::Call { callee, .. })
                if hir_compat::is_import_initializer(callee))
        });
    if has_init {
        // Without exported functions, the top-level statements (with top-level `await`) form
        // the body of a command's `wasi:cli/run`. With exported functions they initialize the
        // module once before the first entry point, and `run` only awaits that initialization.
        let persistent = bodies.functions.iter().any(|f| f.is_exported);
        let async_init = declarations.has_top_level_await;
        let settled = BindingType::plain(void_type(ctx));
        let mut entries = vec![(
            Signature {
                inline: None,
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
            bodies.init,
        )];
        if persistent {
            entries.push((
                Signature {
                    inline: None,
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
            let func = FunctionBuilder::lower_body(
                ctx,
                signature,
                builder::Body::Statements(BodyOwner::Initializer, body),
                &signatures,
                &mut shapes,
                &builtins,
            )
            .map_err(|err| err.with_internal_context("lowering top-level statements"))?;
            if signature.is_async {
                set_async(ctx, &func);
            }
            if !persistent || !signature.top_level {
                command = Some(func);
            }
            module.append_operation(ctx, func.get_operation(), 0);
        }
    }
    for function in &bodies.functions {
        let signature = signatures
            .get(&function.id)
            .ok_or_else(|| internal!("unknown function {}", function.id))?;
        if signature.inline.is_some() {
            continue;
        }
        let func = FunctionBuilder::lower(ctx, function, &signatures, &mut shapes, &builtins)
            .map_err(|error| {
                let error = error
                    .with_fallback_span(locations.function_span(&function.name))
                    .with_internal_context(format!("lowering function {}", function.name));
                locations.annotate_caller(error, &function.name)
            })?;
        locations.set_mir_location(ctx, &func, &function.name);
        if let Some((wit_params, wit_results)) = signature.wit_constraints() {
            perry_mir::constrain_signature(ctx, &func, wit_params, wit_results);
        }
        if function.is_exported {
            perry_mir::set_exported(ctx, &func);
            let index = function.id as usize;
            if functions.len() <= index {
                functions.resize(index + 1, None);
            }
            *functions
                .get_mut(index)
                .ok_or_else(|| internal!("missing export slot"))? = Some(func);
        }
        if function.is_async {
            set_async(ctx, &func);
        }
        module.append_operation(ctx, func.get_operation(), 0);
    }
    for (id, function) in &bodies.native {
        let signature = signatures
            .get(id)
            .ok_or_else(|| internal!("unknown function {}", *id))?;
        let name = signature.name.as_ref();
        let func = FunctionBuilder::lower_body(
            ctx,
            signature,
            builder::Body::Native(&function.body),
            &signatures,
            &mut shapes,
            &builtins,
        )
        .map_err(|error| {
            locations.annotate_caller(
                error.with_fallback_span(locations.function_span(name)),
                name,
            )
        })?;
        if function.is_async {
            set_async(ctx, &func);
        }
        locations.set_mir_location(ctx, &func, name);
        module.append_operation(ctx, func.get_operation(), 0);
    }
    let exports = witnesses
        .iter()
        .map(|witness| match witness.target {
            Some(id) => functions
                .get(id as usize)
                .copied()
                .flatten()
                .map(|f| {
                    if witness.is_destructor {
                        ExportTarget::Destructor(f, witness.name.clone())
                    } else {
                        let symbol = f.get_symbol_name(ctx);
                        let name = locations.display_function_name(symbol.as_ref());
                        ExportTarget::Function(
                            f,
                            witness
                                .signature
                                .iter()
                                .flat_map(|s| &s.annotations)
                                .map(|span| {
                                    span.and_then(|s| locations.span(s))
                                        .or_else(|| locations.declaration_span(symbol.as_ref()))
                                })
                                .collect(),
                            (name != symbol.as_ref()).then(|| name.to_owned()),
                        )
                    }
                })
                .ok_or_else(|| internal!("witness function was not emitted")),
            None => command.map(ExportTarget::Command).ok_or_else(|| {
                diag!(
                    codes::MISSING_EXPORT,
                    "wasi:cli/run requires an implementation or top-level statements"
                )
            }),
        })
        .collect::<Result<_>>()?;
    Ok(LoweredProgram { module, exports })
}

fn module_name(name: &str) -> String {
    let name: String = name
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
