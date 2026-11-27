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

pub(super) type Functions<'s> = indexmap::IndexMap<FuncId, Signature<'s>>;

#[derive(Clone, Copy)]
pub(super) enum Body<'s> {
    Initializer(&'s [Stmt]),
    Command,
    Source(&'s Function<'s>),
    Inline(&'s Function<'s>),
    Constructor(&'s crate::source::InstanceClass<'s>, &'s Function<'s>),
    Native(&'s crate::engine::NativeBody),
}

pub(super) struct Signature<'s> {
    pub(super) body: Body<'s>,
    pub(super) name: Identifier,
    /// `async`: callers start it with `perry.async_call` and get a promise of `result`.
    pub(super) is_async: bool,
    pub(super) await_initialization: bool,
    pub(super) params: Vec<ParamSig<'s>>,
    pub(super) returns_never: bool,
    pub(super) result: Option<BindingType>,
}

impl<'s> Signature<'s> {
    fn native(shapes: &Shapes<'_>, function: &'s crate::engine::NativeFunction) -> Result<Self> {
        Ok(Self {
            name: identifier(&function.name)?,
            body: Body::Native(&function.body),
            await_initialization: false,
            returns_never: false,
            is_async: function.is_async,
            params: function
                .params
                .iter()
                .map(|&p| ParamSig::native(shapes, p, Parameter::Native))
                .collect::<Result<_>>()?,
            result: shapes.solved_result(function.result, function.is_async)?,
        })
    }

    pub(super) fn source(&self) -> Option<&'s Function<'s>> {
        match self.body {
            Body::Source(f) | Body::Inline(f) | Body::Constructor(_, f) => Some(f),
            _ => None,
        }
    }

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
    decls: crate::shapes::Declarations<'_>,
    locations: &mut hir_compat::Locations<'_>,
    witnesses: &[hir_compat::Witness<'_, hir_compat::Target<FuncId>>],
    specialization: &crate::engine::Specialization,
    layouts: (crate::engine::types::TypeTable, perry_mir::RecordLayouts),
) -> Result<LoweredProgram> {
    let bodies = &specialization.bodies;
    let mut functions = HashMap::new();
    let mut command = None;
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
    let decls = decls.with_classes(
        bodies
            .classes
            .iter()
            .map(crate::shapes::ClassShape::Instance),
    );
    let (mut types, records) = layouts;
    types.applications.retain(|_, app| !app.reads_class_fields);
    let shapes = Shapes::new(
        module,
        declarations,
        &decls,
        locations,
        specialization,
        (&mut types, records),
    );
    locations.set_mir_frames(ctx, &module);
    let mut shapes = shapes.prepare(ctx);
    shapes.define_environments(ctx)?;
    let mut signatures = Functions::default();
    for (function, class) in bodies.functions.iter().map(|f| (f, None)).chain(
        bodies
            .classes
            .iter()
            .filter_map(|class| Some((class.constructor.as_ref()?, Some(class)))),
    ) {
        let display_name = locations.display_function_name(&function.name);
        let declaration = &specialization.templates[function.template];
        if declaration.is_generator {
            return Err(locations.annotate_caller(
                unsupported!("generator function {display_name} is not supported yet")
                    .with_fallback_span(locations.function_span(&function.name)),
                &function.name,
            ));
        }

        let mut params = Vec::with_capacity(declaration.params.len());
        if let Some(env) = function.environment {
            params.push(ParamSig::native(
                &shapes,
                crate::engine::NativeParam::Environment(env),
                Parameter::Environment(env),
            )?);
        }
        if let Some(receiver) = function.receiver {
            params.push(ParamSig::native(
                &shapes,
                crate::engine::NativeParam::Value(receiver),
                Parameter::Receiver,
            )?);
        }
        for (idx, param) in declaration.params.iter().enumerate() {
            use crate::engine::Storage;
            let ty = match specialization
                .instances
                .storage(BodyOwner::Function(function.id), param.id)
            {
                Some(Storage::ErasedFunction(_)) => continue,
                Some(Storage::Value(ty) | Storage::Boxed { ty, .. }) => ty,
                _ => bail_internal!("missing parameter layout for {display_name}"),
            };
            if param.is_rest && idx + 1 != declaration.params.len() {
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
            let (base, is_never) = shapes
                .native_param(crate::engine::NativeParam::Value(ty))
                .map_err(|error| {
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
        let body = if let Some(class) = class {
            Body::Constructor(class, function)
        } else if function.inline.is_some()
            || (function.environment.is_none()
                && declaration
                    .captures
                    .iter()
                    .any(|id| !shapes.globals.contains_key(id)))
        {
            Body::Inline(function)
        } else {
            Body::Source(function)
        };
        signatures.entry(function.id).or_insert(Signature {
            body,
            is_async: declaration.is_async,
            await_initialization: function.is_exported
                && declaration.is_async
                && declarations.has_top_level_await,
            name: identifier(&function.name)?,
            params,
            returns_never,
            result,
        });
    }
    for &(id, ref function) in &bodies.native {
        let annotate = |err: Diagnostic| {
            locations.annotate_caller(
                err.with_fallback_span(locations.function_span(&function.name)),
                &function.name,
            )
        };
        signatures
            .entry(id)
            .or_insert(Signature::native(&shapes, function).map_err(annotate)?);
    }
    // Module initialization of named built-in imports (`Writable` from `node:stream`) has
    // nothing to run (calls lower it to nothing); without other statements there is no main
    // function.
    let has_init = !specialization.resources.is_empty()
        || !bodies.static_fields.is_empty()
        || bodies.init.iter().any(|statement| {
            !matches!(statement, Stmt::Expr(Expr::Call { callee, .. })
                if hir_compat::is_import_initializer(callee))
        });
    if has_init {
        // Without exported functions, the top-level statements (with top-level `await`) form
        // the body of a command's `wasi:cli/run`. With exported functions they initialize the
        // module once before the first entry point, and `run` only awaits that initialization.
        let persistent =
            !specialization.resources.is_empty() || bodies.functions.iter().any(|f| f.is_exported);
        let async_init = declarations.has_top_level_await;
        let settled = BindingType::plain(void_type(ctx));
        let mut entries = vec![Signature {
            body: Body::Initializer(bodies.init),
            name: identifier(if persistent {
                perry_mir::INITIALIZER
            } else {
                MAIN
            })?,
            is_async: !persistent || async_init,
            await_initialization: false,
            params: vec![],
            returns_never: false,
            result: (!persistent || async_init).then_some(settled),
        }];
        if persistent {
            entries.push(Signature {
                body: Body::Command,
                name: identifier(MAIN)?,
                is_async: true,
                await_initialization: async_init,
                params: vec![],
                returns_never: false,
                result: Some(settled),
            });
        }
        for signature in &entries {
            let func = FunctionBuilder::lower_body(ctx, signature, &signatures, &mut shapes)
                .map_err(|err| err.with_internal_context("lowering top-level statements"))?;
            if signature.is_async {
                set_async(ctx, &func);
            }
            if !persistent || matches!(signature.body, Body::Command) {
                command = Some(func);
            }
            module.append_operation(ctx, func.get_operation(), 0);
        }
    }
    for (&id, signature) in &signatures {
        let name = signature.name.as_ref();
        let exported = match signature.body {
            Body::Source(f) | Body::Constructor(_, f) => {
                ensure_unsupported!(
                    f.environment.is_some()
                        || specialization.templates[f.template]
                            .captures
                            .iter()
                            .all(|id| shapes.globals.get(id).is_some_and(Option::is_some)),
                    "closures capturing activation-local values are unsupported: {name}"
                );
                f.is_exported
            }
            Body::Native(body) => matches!(body, crate::engine::NativeBody::Resource(_)),
            Body::Inline(_) | Body::Initializer(_) | Body::Command => continue,
        };
        let func =
            FunctionBuilder::lower_body(ctx, signature, &signatures, &mut shapes).map_err(|e| {
                locations.annotate_caller(
                    e.with_fallback_span(locations.function_span(name))
                        .with_internal_context(format!("lowering function {name}")),
                    name,
                )
            })?;
        locations.set_mir_location(ctx, &func, name);
        if let Some((params, results)) = signature.wit_constraints() {
            perry_mir::constrain_signature(ctx, &func, params, results);
        }
        if exported {
            perry_mir::set_exported(ctx, &func);
            functions.insert(id, func);
        }
        if signature.is_async {
            set_async(ctx, &func);
        }
        module.append_operation(ctx, func.get_operation(), 0);
    }
    let exports = witnesses
        .iter()
        .zip(&bodies.exports)
        .map(|(witness, id)| {
            let Some(id) = id else {
                return command.map(ExportTarget::Command).ok_or_else(|| {
                    diag!(
                        codes::MISSING_EXPORT,
                        "wasi:cli/run requires an implementation or top-level statements"
                    )
                });
            };
            let &f = functions
                .get(id)
                .ok_or_else(|| internal!("witness function was not emitted"))?;
            if matches!(witness.target, Some(hir_compat::Target::Destructor(..))) {
                return Ok(ExportTarget::Destructor(f, witness.name.clone()));
            }
            let symbol = f.get_symbol_name(ctx);
            let name = locations.display_function_name(symbol.as_ref());
            Ok(ExportTarget::Function(
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
            ))
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
