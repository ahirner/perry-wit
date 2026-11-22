//! Concrete function instantiations keyed by structural source types.
use super::walk::{Node, Visitor};
use super::*;
use perry_hir::ir::Function;
use perry_wit_diagnostics::{Diagnostic, codes, diag, ensure_diag};
use std::collections::VecDeque;

mod callables;
mod guards;
use crate::narrowing::Guards;
use callables::Callables;
use std::rc::Rc;

fn incomplete(ty: &HirType) -> bool {
    ty.any(&mut |ty| matches!(ty, HirType::Any | HirType::Unknown | HirType::TypeVar(_)))
}

fn accepts(expected: &HirType, actual: &HirType) -> bool {
    if expected == actual || matches!(actual, HirType::Never) {
        return true;
    }
    match (expected, actual) {
        (_, HirType::Union(alternatives)) => alternatives.iter().all(|ty| accepts(expected, ty)),
        (HirType::Union(alternatives), _) => alternatives.iter().any(|ty| accepts(ty, actual)),
        (HirType::String, HirType::StringLiteral(_)) | (HirType::Number, HirType::Int32) => true,
        _ => false,
    }
}

fn arguments(
    function: &Function,
    actual: impl IntoIterator<Item = HirType>,
    explicit: &[HirType],
) -> Result<Vec<HirType>> {
    ensure_diag!(
        explicit.len() <= function.type_params.len(),
        codes::TYPE_MISMATCH,
        "too many type arguments for {}",
        hir_compat::display_name(&function.name)
    );
    let mut bindings: HashMap<&str, HirType> = function
        .type_params
        .iter()
        .zip(explicit)
        .map(|(param, argument)| (param.name.as_str(), argument.clone()))
        .collect();
    if bindings.len() < function.type_params.len() {
        for (parameter, actual) in function.params.iter().zip(actual) {
            unify(&parameter.ty, actual, &function.type_params, &mut bindings);
        }
    }
    function
        .type_params
        .iter()
        .map(|param| {
            bindings
                .remove(param.name.as_str())
                .or_else(|| param.default.as_deref().cloned())
                .filter(|ty| {
                    matches!(ty, HirType::Function(_))
                        || (!incomplete(ty) && !matches!(ty, HirType::Never))
                })
                .ok_or_else(|| {
                    let function = hir_compat::display_name(&function.name);
                    match param.name.strip_prefix('@') {
                        Some(name) => diag!(
                            codes::MISSING_TYPE_ANNOTATION,
                            "cannot infer the type of parameter '{name}' of {function}"
                        ),
                        None => diag!(
                            codes::MISSING_TYPE_ANNOTATION,
                            "cannot infer type argument {} of {function}",
                            param.name
                        ),
                    }
                })
        })
        .collect()
}

pub(super) fn class_arguments(
    class: &perry_hir::ir::Class,
    args: &[Expr],
    env: &mut FunctionEnv<'_>,
) -> Option<Vec<HirType>> {
    let mut bindings = HashMap::new();
    if let Some(ctor) = &class.constructor {
        for (parameter, argument) in ctor.params.iter().zip(args) {
            let actual = expression(argument, env);
            unify(&parameter.ty, actual, &class.type_params, &mut bindings);
        }
    }
    class
        .type_params
        .iter()
        .map(|param| {
            bindings
                .remove(param.name.as_str())
                .filter(is_resolvable_type)
                .or_else(|| {
                    param
                        .default
                        .as_deref()
                        .or(param.constraint.as_deref())
                        .cloned()
                })
        })
        .collect()
}

fn unify<'a>(
    pattern: &'a HirType,
    actual: HirType,
    type_params: &[perry_hir::types::TypeParam],
    bindings: &mut HashMap<&'a str, HirType>,
) {
    match (pattern, actual) {
        (HirType::TypeVar(name), actual) if !matches!(actual, HirType::Any | HirType::Never) => {
            bindings.entry(name).or_insert(actual);
        }
        (HirType::Named(name), actual)
            if type_params.iter().any(|tp| tp.name == *name)
                && !matches!(actual, HirType::Any | HirType::Never) =>
        {
            bindings.entry(name).or_insert(actual);
        }
        (HirType::Array(pattern), HirType::Array(actual))
        | (HirType::Promise(pattern), HirType::Promise(actual)) => {
            unify(pattern, *actual, type_params, bindings)
        }
        (HirType::Tuple(pattern), HirType::Tuple(actual)) => {
            for (pattern, actual) in pattern.iter().zip(actual) {
                unify(pattern, actual, type_params, bindings);
            }
        }
        (
            HirType::Generic { base, type_args },
            HirType::Generic {
                base: other,
                type_args: actual,
            },
        ) if *base == other => {
            for (pattern, actual) in type_args.iter().zip(actual) {
                unify(pattern, actual, type_params, bindings);
            }
        }
        (HirType::Object(pattern), HirType::Object(mut actual)) => {
            for (name, property) in &pattern.properties {
                if let Some(actual) = actual.properties.remove(name) {
                    unify(&property.ty, actual.ty, type_params, bindings);
                }
            }
            if let (Some(pattern), Some(actual)) =
                (&pattern.index_signature, actual.index_signature)
            {
                unify(pattern, *actual, type_params, bindings);
            }
        }
        (HirType::Function(pattern), HirType::Function(actual)) => {
            for ((_, pattern, _), (_, actual, _)) in pattern.params.iter().zip(actual.params) {
                unify(pattern, actual, type_params, bindings);
            }
            unify(
                &pattern.return_type,
                *actual.return_type,
                type_params,
                bindings,
            );
        }
        _ => {}
    }
}

#[derive(PartialEq, Eq, Hash)]
struct InstanceKey {
    types: Vec<shapes::TypeId>,
    callables: Vec<(usize, FuncId)>,
}

struct CallArguments {
    types: Vec<HirType>,
    callables: Vec<(usize, FuncId)>,
}

struct Instances {
    value_uses: HashMap<LocalId, swc_common::Span>,
    inline_callbacks: HashMap<FuncId, HirType>,
    callables: Callables,
    result_contracts: Vec<(FuncId, HirType, String, u32)>,
    captured_writes: HashSet<LocalId>,
    keys: HashMap<FuncId, HashMap<InstanceKey, FuncId>>,
    blocked: Option<(Diagnostic, Option<String>)>,
    progressed: bool,
    pending: Vec<Function>,
    next_function: FuncId,
    next_local: LocalId,
    instantiations: Vec<(String, String, u32, Option<String>)>,
    remapped_locals: Vec<(LocalId, LocalId)>,
}

impl Instances {
    fn request(
        &mut self,
        function: &Function,
        arguments: Vec<HirType>,
        callables: Vec<(usize, FuncId)>,
        call_byte_offset: u32,
        caller: Option<(FuncId, &str)>,
        env: &mut FunctionEnv<'_>,
    ) -> Result<FuncId> {
        let key = InstanceKey {
            types: shapes::TypeId::intern_slice(env.ctx, &arguments),
            callables,
        };
        let instances = self.keys.entry(function.id).or_default();
        if let Some(id) = instances.get(&key) {
            return Ok(*id);
        }
        if instances.len() >= 256 {
            let mut error = Diagnostic::unsupported(format!(
                "generic specialization limit of 256 distinct argument lists reached for {}",
                hir_compat::display_name(&function.name)
            ));
            if let Some((caller, _)) = caller
                && let Some((previous, _)) = instances.iter().find(|(_, id)| **id == caller)
            {
                for ((param, prior), next) in function
                    .type_params
                    .iter()
                    .zip(&previous.types)
                    .zip(&key.types)
                {
                    if prior != next && next.hir(env.ctx).any(&mut |ty| ty == prior.hir(env.ctx)) {
                        error = error.with_note(format!(
                            "this recursive call grows type argument {} by nesting its previous type",
                            param.name
                        ));
                        break;
                    }
                }
            }
            return Err(error);
        }
        let id = self.next_function;
        self.next_function = id
            .checked_add(1)
            .ok_or_else(|| Diagnostic::internal("function identifier limit"))?;

        let bindings = function
            .type_params
            .iter()
            .zip(&arguments)
            .map(|(param, arg)| (param.name.as_str(), arg))
            .collect();
        let mut instance = function.clone();
        let mut bound = HashMap::new();
        for &(index, target) in &key.callables {
            bound.insert(instance.params[index].id, target);
            if let Some(callback) = self
                .callables
                .templates
                .get(self.callables.origins.get(&target).unwrap_or(&target))
            {
                instance.captures.extend_from_slice(&callback.captures);
            }
        }
        instance.captures.sort_unstable();
        instance.captures.dedup();
        for capture in &instance.captures {
            if let Some(ty) = env.local_type(*capture) {
                self.callables.captures.insert(*capture, ty.clone());
            }
        }
        instance.params.retain(|p| !bound.contains_key(&p.id));
        instances.insert(key, id);
        instance.id = id;
        instance.name = format!("{}_instance_{id}", function.name);
        if let Some(result) = self.inline_callbacks.get(&function.id) {
            instance.return_type = result.clone();
        } else if !env.metadata.locations.has_explicit_return(&function.name) {
            instance.return_type = HirType::Any;
        }
        instance.type_params.clear();
        instance.is_exported = false;
        // Fresh declarations lie above every source id, so the reference remap leaves them intact.
        let mut locals = HashMap::new();
        walk::function(&mut instance, &mut |node: Node<'_>| {
            match node {
                Node::Binding(local, ty) => {
                    substitute_type_vars(ty, &bindings);
                    self.rename_local(local, &mut locals)?;
                }
                Node::Local(local) => self.rename_local(local, &mut locals)?,
                Node::Type(ty) => substitute_type_vars(ty, &bindings),
                Node::Expression(Expr::Closure { func_id, .. }) => {
                    *func_id = self.next_function;
                    self.next_function = func_id
                        .checked_add(1)
                        .ok_or_else(|| Diagnostic::internal("function identifier limit"))?;
                }
                Node::Expression(Expr::LocalSet(local, _) | Expr::Update { id: local, .. })
                    if bound.contains_key(local) =>
                {
                    let param = function
                        .params
                        .iter()
                        .find(|p| p.id == *local)
                        .ok_or_else(|| Diagnostic::internal("callback parameter binding"))?;
                    return Err(Diagnostic::unsupported(format!(
                        "cannot reassign callback parameter '{}'",
                        param.name
                    )));
                }
                Node::Expression(expr) => {
                    if let Expr::LocalGet(local) = expr
                        && let Some(target) = bound.get(local)
                    {
                        *expr = Expr::FuncRef(*target);
                    } else {
                        materialize_constructor(expr, env.metadata, &bindings);
                    }
                }
            }
            Ok(())
        })?;
        self.instantiations.push((
            instance.name.clone(),
            function.name.clone(),
            call_byte_offset,
            caller.map(|(_, name)| name.to_owned()),
        ));
        for param in &mut instance.params {
            if let Some(default) = &mut param.default {
                perry_hir::analysis::remap_local_ids_in_expr(default, &locals);
            }
        }
        perry_hir::analysis::remap_local_ids_in_stmts(&mut instance.body, &locals);
        for (&old, &new) in &locals {
            if let Some(span) = self.value_uses.get(&old).copied() {
                self.value_uses.insert(new, span);
            }
            if self.callables.invoked.contains(&old) {
                self.callables.invoked.insert(new);
            }
            if self.captured_writes.contains(&old) {
                self.captured_writes.insert(new);
            }
        }
        self.remapped_locals.extend(locals);
        self.pending.push(instance);
        Ok(id)
    }

    fn rename_local(
        &mut self,
        id: &mut LocalId,
        locals: &mut HashMap<LocalId, LocalId>,
    ) -> Result<()> {
        *id = match locals.entry(*id) {
            std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let next = self.next_local;
                self.next_local = next
                    .checked_add(1)
                    .ok_or_else(|| Diagnostic::internal("local identifier limit"))?;
                *entry.insert(next)
            }
        };
        Ok(())
    }

    fn body(
        &mut self,
        body: &mut [Stmt],
        caller: Option<(FuncId, &str)>,
        env: &mut FunctionEnv<'_>,
    ) -> Result<()> {
        walk::visit_body(body, &mut |node: Node<'_>| {
            if let Node::Binding(id, ty) = node {
                if contains_type_var(ty) {
                    *ty = HirType::Any;
                } else if !matches!(ty, HirType::Any | HirType::Unknown) {
                    env.locals.insert(*id, ty.clone());
                }
            }
            Ok(())
        })?;
        self.callables.prepare(body, env)?;
        walk::scan(body, env, None, None);
        guards::Calls::new(env, |expr, guards, env| {
            self.call(expr, caller, env, guards)
        })
        .body(body)
    }

    fn call(
        &mut self,
        expr: &mut Expr,
        caller: Option<(FuncId, &str)>,
        env: &mut FunctionEnv<'_>,
        guards: &Guards,
    ) -> Result<()> {
        let offset = match expr {
            Expr::Call { byte_offset, .. } => *byte_offset,
            _ => 0,
        };
        if let Some((callback, types)) = closures::inputs(expr, env) {
            if let Expr::Closure { params, .. } = callback
                && params.iter().enumerate().all(|(i, p)| {
                    !incomplete(&p.ty) || types.get(i).is_some_and(|ty| !incomplete(ty))
                })
            {
                let result = closures::result(callback, types.iter().cloned(), env);
                if !incomplete(&result) {
                    self.callables.lift(callback, None);
                    if let Expr::FuncRef(id) = callback {
                        self.inline_callbacks.insert(*id, result);
                    }
                }
            }
            if let Expr::FuncRef(id) = callback
                && let Some(function) = self.callables.templates.get(id).cloned()
                && let Some(target) = self.resolve(
                    &function,
                    CallArguments {
                        types,
                        callables: Vec::new(),
                    },
                    &[],
                    offset,
                    caller,
                    env,
                )?
            {
                *id = target;
            }
        }
        let Expr::Call {
            callee,
            args,
            type_args,
            byte_offset,
            ..
        } = expr
        else {
            return Ok(());
        };
        let Expr::FuncRef(id) = callee.as_mut() else {
            return Ok(());
        };
        let Some(function) = self.callables.templates.get(id).cloned() else {
            return Ok(());
        };
        if function.type_params.iter().any(|p| p.name.starts_with('@'))
            && args.len() != function.params.len()
        {
            return Err(env.metadata.locations.annotate_call(
                diag!(
                    codes::MISSING_TYPE_ANNOTATION,
                    "cannot infer parameters of {} from a call with {} arguments (expected {})",
                    hir_compat::display_name(&function.name),
                    args.len(),
                    function.params.len()
                ),
                *byte_offset,
            ));
        }
        let mut callables = Vec::new();
        let actual = args
            .iter()
            .enumerate()
            .map(|(index, arg)| {
                if let Expr::FuncRef(target) = arg
                    && let Some(signature) = self.callables.signatures.get(target)
                {
                    callables.push((index, *target));
                    HirType::Function(signature.clone())
                } else {
                    guards::argument(arg, expression(arg, env), guards, &self.captured_writes)
                }
            })
            .collect::<Vec<_>>();
        ensure_diag!(
            callables
                .iter()
                .all(|(index, _)| *index < function.params.len()),
            codes::TYPE_MISMATCH,
            "extra callback argument to {}",
            function.name
        );
        for (param, actual) in function.params.iter().zip(&actual) {
            if matches!(param.ty, HirType::Function(_))
                && !matches!(actual, HirType::Function(_))
                && !incomplete(actual)
            {
                return Err(env.metadata.locations.annotate_call(
                    diag!(
                        codes::TYPE_MISMATCH,
                        "parameter '{}' of {} requires a statically known function",
                        param.name,
                        hir_compat::display_name(&function.name)
                    ),
                    *byte_offset,
                ));
            }
        }
        for (index, target) in &mut callables {
            let parameter = &function.params[*index];
            if let Some(span) = self.value_uses.get(&parameter.id) {
                return Err(env.metadata.locations.annotate(
                    Diagnostic::unsupported(format!(
                        "callback '{}' of {} escapes its static call site",
                        parameter.name,
                        hir_compat::display_name(&function.name)
                    )),
                    *span,
                ));
            }
            let mismatch = || {
                env.metadata.locations.annotate_call(
                    diag!(
                        codes::TYPE_MISMATCH,
                        "callback does not match parameter '{}' of {}",
                        parameter.name,
                        hir_compat::display_name(&function.name)
                    ),
                    *byte_offset,
                )
            };
            match &parameter.ty {
                HirType::Function(expected) => {
                    let signature = &self.callables.signatures[target];
                    if signature.is_async != expected.is_async
                        || signature.is_generator != expected.is_generator
                        || signature.params.iter().filter(|p| !p.2).count() > expected.params.len()
                        || !signature
                            .params
                            .iter()
                            .zip(&expected.params)
                            .all(|((_, a, _), (_, b, _))| incomplete(a) || accepts(a, b))
                    {
                        return Err(mismatch());
                    }
                    let expected_result = &*expected.return_type;
                    if let Some(template) = self.callables.templates.get(target).cloned() {
                        let types = arguments(
                            &template,
                            expected.params.iter().map(|(_, ty, _)| ty.clone()),
                            &[],
                        )
                        .map_err(|_| mismatch())?;
                        let bound =
                            self.request(&template, types, Vec::new(), *byte_offset, caller, env)?;
                        self.callables.origins.insert(bound, *target);
                        self.callables
                            .signatures
                            .entry(bound)
                            .or_insert_with(|| expected.clone());
                        *target = bound;
                    }
                    if !matches!(expected_result, HirType::Void) {
                        self.result_contracts.push((
                            *target,
                            expected_result.clone(),
                            parameter.name.clone(),
                            *byte_offset,
                        ));
                    }
                }
                HirType::TypeVar(_) | HirType::Any | HirType::Unknown => {}
                HirType::Named(name) if function.type_params.iter().any(|p| &p.name == name) => {}
                _ => return Err(mismatch()),
            }
        }
        let Some(instance) = self.resolve(
            &function,
            CallArguments {
                types: actual,
                callables,
            },
            type_args,
            *byte_offset,
            caller,
            env,
        )?
        else {
            return Ok(());
        };
        args.retain(
            |arg| !matches!(arg, Expr::FuncRef(id) if self.callables.signatures.contains_key(id)),
        );
        *id = instance;
        type_args.clear();
        Ok(())
    }

    fn resolve(
        &mut self,
        function: &Function,
        actual: CallArguments,
        explicit: &[HirType],
        offset: u32,
        caller: Option<(FuncId, &str)>,
        env: &mut FunctionEnv<'_>,
    ) -> Result<Option<FuncId>> {
        let arguments = match arguments(function, actual.types, explicit) {
            Ok(arguments) => arguments,
            Err(error) if error.code() == codes::MISSING_TYPE_ANNOTATION => {
                self.blocked.get_or_insert_with(|| {
                    (
                        env.metadata.locations.annotate_call(error, offset),
                        caller.map(|(_, name)| name.to_owned()),
                    )
                });
                return Ok(None);
            }
            Err(error) => return Err(env.metadata.locations.annotate_call(error, offset)),
        };
        let id = self
            .request(function, arguments, actual.callables, offset, caller, env)
            .map_err(|error| env.metadata.locations.annotate_call(error, offset))?;
        self.progressed = true;
        Ok(Some(id))
    }

    fn expression(
        &mut self,
        expr: &mut Expr,
        caller: Option<(FuncId, &str)>,
        env: &mut FunctionEnv<'_>,
    ) -> Result<()> {
        guards::Calls::new(env, |expr, guards, env| {
            self.call(expr, caller, env, guards)
        })
        .expression(expr)
    }

    fn callable(
        &mut self,
        params: &mut [perry_hir::ir::Param],
        body: &mut [Stmt],
        caller: Option<(FuncId, &str)>,
        env: &mut FunctionEnv<'_>,
    ) -> Result<()> {
        for param in params {
            if let Some(default) = &mut param.default {
                self.expression(default, caller, env)?;
            }
        }
        self.body(body, caller, env)
    }

    fn class(
        &mut self,
        class_idx: usize,
        classes: &mut [perry_hir::ir::Class],
        base: &mut perry_hir::HirTypeEnv,
        inference: &Inference<'_>,
        locals: &mut HashMap<LocalId, HirType>,
    ) -> Result<()> {
        let missing_class = || Diagnostic::internal("generic worklist refers to an unknown class");
        let class = classes.get_mut(class_idx).ok_or_else(missing_class)?;
        base.set_current_class(Some(class.name.clone()));
        let mut inits: Vec<Expr> = class_initializers(class)
            .map(|init| std::mem::replace(init, Expr::Undefined))
            .collect();
        for init in &mut inits {
            let mut env = inference.function(base, classes, locals);
            self.expression(init, None, &mut env)?;
        }
        let class = classes.get_mut(class_idx).ok_or_else(missing_class)?;
        for (dst, src) in class_initializers(class).zip(inits) {
            *dst = src;
        }
        let mut class_fns: Vec<_> = shapes::class_functions_mut(class)
            .map(|target| {
                (
                    std::mem::take(&mut target.params),
                    std::mem::take(&mut target.body),
                )
            })
            .collect();
        for (params, body) in &mut class_fns {
            let mut env = inference.function_with_params(base, classes, locals, params);
            self.callable(params, body, None, &mut env)?;
        }
        let class = classes.get_mut(class_idx).ok_or_else(missing_class)?;
        for (target, (params, body)) in shapes::class_functions_mut(class).zip(class_fns) {
            target.params = params;
            target.body = body;
        }
        base.set_current_class(None);
        Ok(())
    }

    fn append(&mut self, functions: &mut Vec<Function>, base: &mut perry_hir::HirTypeEnv) {
        for function in self.pending.drain(..) {
            base.insert_function_return(function.id, function.return_type.clone());
            functions.push(function);
        }
    }
}

fn materialize_constructor(
    expr: &mut Expr,
    metadata: &hir_compat::Metadata<'_>,
    bindings: &HashMap<&str, &HirType>,
) {
    let (name, mut types) = match metadata.constructors.get(expr) {
        Some(hir_compat::Constructor::Map {
            types: Some((key, value)),
            ..
        }) => ("\0perry_Map", vec![key.clone(), value.clone()]),
        Some(hir_compat::Constructor::Set {
            element: Some(element),
            ..
        }) => ("\0perry_Set", vec![element.clone()]),
        _ => return,
    };
    for ty in &mut types {
        substitute_type_vars(ty, bindings);
    }
    let original = std::mem::replace(expr, Expr::Undefined);
    let argument = match original {
        Expr::New { mut args, .. } | Expr::NewDynamic { mut args, .. } => args.pop(),
        Expr::MapNewFromArray(argument) | Expr::SetNewFromArray(argument) => Some(*argument),
        _ => None,
    };
    let init = match argument {
        Some(Expr::Sequence(mut items)) => items
            .pop()
            .filter(|expr| !matches!(expr, Expr::Null | Expr::Undefined)),
        init => init,
    };
    *expr = Expr::New {
        class_name: name.into(),
        type_args: types,
        args: init.into_iter().collect(),
        byte_offset: 0,
        byte_end: 0,
        cap_args_appended: 0,
    };
}

fn class_initializers(class: &mut perry_hir::ir::Class) -> impl Iterator<Item = &mut Expr> {
    class
        .fields
        .iter_mut()
        .filter_map(|field| field.init.as_mut())
        .chain(
            class
                .static_fields
                .iter_mut()
                .filter_map(|field| field.init.as_mut()),
        )
}

pub(crate) fn specialize_functions(
    ctx: &Context,
    hir: &mut HirModule,
    metadata: &mut hir_compat::Metadata<'_>,
    foreign: &HashMap<String, crate::hir::Foreign>,
    mut bindings: hir_compat::CallableBindings,
) -> Result<()> {
    let mut base = perry_hir::HirTypeEnv::from_module(hir);
    let mut callables = Callables {
        invoked: bindings.invoked,
        ..Default::default()
    };
    let mut next_function = 0;
    let mut next_local = hir.local_source_spans.keys().copied().max().unwrap_or(0);
    let module_vars: HashSet<_> = hir
        .init
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::Let {
                id, mutable: true, ..
            } => Some(*id),
            _ => None,
        })
        .collect();
    let mut captured_writes = HashSet::new();
    let mut closure_max = 0;
    let mut collect_local = |node: Node<'_>, module_init: bool| {
        if !module_init
            && let Node::Expression(Expr::LocalSet(id, _) | Expr::Update { id, .. }) = &node
            && module_vars.contains(id)
        {
            captured_writes.insert(*id);
        }
        if let Node::Expression(Expr::Closure {
            func_id,
            mutable_captures,
            ..
        }) = &node
        {
            closure_max = closure_max.max(*func_id);
            captured_writes.extend(mutable_captures.iter().copied());
        }
        if let Node::Binding(id, _) | Node::Local(id) = node {
            next_local = next_local.max(*id);
        }
        Ok(())
    };
    walk::visit_body(&mut hir.init, &mut |node: Node<'_>| {
        collect_local(node, true)
    })?;
    let mut collect_local = |node: Node<'_>| collect_local(node, false);
    for class in &mut hir.classes {
        for function in shapes::class_functions_mut(class) {
            next_function = next_function.max(function.id);
            walk::function(function, &mut collect_local)?;
        }
        for initializer in class_initializers(class) {
            walk::visit_expr(initializer, &mut collect_local)?;
        }
    }
    for mut function in std::mem::take(&mut hir.functions) {
        if !function.is_exported
            && let Some(positions) = bindings.implicit.remove(function.name.as_str())
        {
            callables::implicit_parameters(&mut function, |index| positions.contains(&index));
        }
        next_function = next_function.max(function.id);
        walk::function(&mut function, &mut collect_local)?;
        callables
            .signatures
            .insert(function.id, callables::signature(&function));
        if function.type_params.is_empty()
            && (function.is_exported
                || !function
                    .params
                    .iter()
                    .any(|p| matches!(p.ty, HirType::Function(_))))
        {
            hir.functions.push(function);
        } else {
            if function.is_exported {
                hir.functions.push(function.clone());
            }
            base.insert_function_return(function.id, HirType::Any);
            callables.templates.insert(function.id, Rc::new(function));
        }
    }
    let decls = shapes::Declarations::new(
        &hir.interfaces,
        &hir.type_aliases,
        &hir.imports,
        &hir.classes,
        foreign,
    );
    let inference = Inference {
        ctx,
        decls: &decls,
        metadata,
    };
    let mut instances = Instances {
        value_uses: bindings.value_uses,
        inline_callbacks: HashMap::new(),
        callables,
        result_contracts: Vec::new(),
        captured_writes,
        keys: HashMap::new(),
        blocked: None,
        progressed: false,
        pending: Vec::new(),
        next_function: next_function
            .max(closure_max)
            .checked_add(1)
            .ok_or_else(|| Diagnostic::internal("function identifier limit"))?,
        next_local: next_local
            .checked_add(1)
            .ok_or_else(|| Diagnostic::internal("local identifier limit"))?,
        instantiations: Vec::new(),
        remapped_locals: Vec::new(),
    };
    #[derive(Clone, Copy)]
    enum Body {
        Init,
        Class(usize),
        Function(usize),
    }
    let mut ready = VecDeque::from([Body::Init]);
    ready.extend((0..hir.classes.len()).map(Body::Class));
    ready.extend((0..hir.functions.len()).map(Body::Function));
    let mut waiting = VecDeque::new();
    let mut locals = HashMap::new();
    let failure = loop {
        instances.progressed = false;
        let mut first_error = None;
        while let Some(body) = ready.pop_front() {
            let before = hir.functions.len();
            let mut open_result = false;
            match body {
                Body::Init => {
                    let mut env = inference.function(&base, &hir.classes, &mut locals);
                    instances.body(&mut hir.init, None, &mut env)?;
                    for (id, ty) in locals.drain() {
                        if base.local_type(id).is_some_and(incomplete) && !incomplete(&ty) {
                            base.insert_local(id, ty);
                            instances.progressed = true;
                        }
                    }
                }
                Body::Class(index) => {
                    instances.class(index, &mut hir.classes, &mut base, &inference, &mut locals)?
                }
                Body::Function(index) => {
                    let function = hir.functions.get_mut(index).ok_or_else(|| {
                        Diagnostic::internal("generic worklist refers to an unknown function")
                    })?;
                    if !function.type_params.is_empty() {
                        continue;
                    }
                    let mut env = inference.function_with_params(
                        &base,
                        &hir.classes,
                        &mut locals,
                        &function.params,
                    );
                    for capture in &function.captures {
                        if let Some(ty) = instances.callables.captures.get(capture) {
                            env.locals.insert(*capture, ty.clone());
                        }
                    }
                    instances.callable(
                        &mut function.params,
                        &mut function.body,
                        Some((function.id, &function.name)),
                        &mut env,
                    )?;
                    if incomplete(&function.return_type) {
                        let mut result = HirType::Any;
                        if instances.blocked.is_none()
                            && infer_body_return(
                                &function.body,
                                function.is_async,
                                &mut result,
                                &mut env,
                                &decls,
                            )
                            && !incomplete(&result)
                        {
                            base.insert_function_return(function.id, result.clone());
                            function.return_type = result;
                            instances.progressed = true;
                        } else {
                            open_result = true;
                        }
                    }
                }
            }
            instances.append(&mut hir.functions, &mut base);
            ready.extend((before..hir.functions.len()).map(Body::Function));
            if let Some(error) = instances.blocked.take() {
                first_error.get_or_insert(error);
                waiting.push_back(body);
            } else if open_result {
                // Newly discovered callees publish results before their callers retry.
                waiting.push_front(body);
            }
        }
        if waiting.is_empty() || !instances.progressed {
            break first_error;
        }
        std::mem::swap(&mut ready, &mut waiting);
    };
    for (old_id, new_id) in instances.remapped_locals {
        if let Some(span) = hir.local_source_spans.get(&old_id).copied() {
            hir.local_source_spans.insert(new_id, span);
        }
        if metadata.deferred_bindings.contains(&old_id) {
            metadata.deferred_bindings.insert(new_id);
        }
        if metadata.inferred_bindings.contains(&old_id) {
            metadata.inferred_bindings.insert(new_id);
        }
    }
    for (instance_name, template_name, byte_offset, caller) in instances.instantiations {
        metadata
            .locations
            .record_instantiation(instance_name, template_name, byte_offset, caller);
    }
    if let Some((error, caller)) = failure {
        return Err(match caller {
            Some(caller) => metadata.locations.annotate_caller(error, &caller),
            None => error,
        });
    }
    for (id, expected, param, offset) in instances.result_contracts {
        if base
            .function_return_type(id)
            .is_none_or(|actual| !accepts(&expected, actual))
        {
            return Err(metadata.locations.annotate_call(
                diag!(
                    codes::TYPE_MISMATCH,
                    "callback result does not match the declared result of parameter '{param}'"
                ),
                offset,
            ));
        }
    }
    for (template, instantiations) in instances.keys {
        if instances.inline_callbacks.contains_key(&template) {
            metadata
                .inline_callbacks
                .extend(instantiations.into_values());
        }
    }
    metadata.lifted_captures = instances.callables.lifted_captures;
    metadata.locations.generic_templates = instances
        .callables
        .templates
        .into_iter()
        .map(|(id, function)| (id, Rc::unwrap_or_clone(function).name))
        .collect();
    Ok(())
}
