//! Concrete function instantiations keyed by structural source types.
use super::walk::Node;
use super::*;
use perry_hir::ir::Function;
use perry_wit_diagnostics::{Diagnostic, codes, diag, ensure_diag};
use std::collections::VecDeque;

fn incomplete(ty: &HirType) -> bool {
    ty.any(&mut |ty| matches!(ty, HirType::Any | HirType::Unknown | HirType::TypeVar(_)))
}

fn arguments(
    function: &Function,
    args: &[Expr],
    explicit: &[HirType],
    env: &mut FunctionEnv<'_>,
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
        for (parameter, argument) in function.params.iter().zip(args) {
            let actual = expression(argument, env);
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
                .filter(|ty| !incomplete(ty) && !matches!(ty, HirType::Never))
                .ok_or_else(|| {
                    diag!(
                        codes::MISSING_TYPE_ANNOTATION,
                        "cannot infer type argument {} of {}",
                        param.name,
                        hir_compat::display_name(&function.name)
                    )
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

struct Instances<'a> {
    templates: &'a HashMap<FuncId, Function>,
    keys: HashMap<FuncId, HashMap<Vec<shapes::TypeId>, FuncId>>,
    blocked: Option<(Diagnostic, Option<String>)>,
    progressed: bool,
    pending: Vec<Function>,
    next_function: FuncId,
    next_local: LocalId,
    instantiations: Vec<(String, String, u32, Option<String>)>,
    remapped_locals: Vec<(LocalId, LocalId)>,
}

impl Instances<'_> {
    fn request(
        &mut self,
        function: &Function,
        arguments: Vec<HirType>,
        call_byte_offset: u32,
        caller: Option<(FuncId, &str)>,
        env: &mut FunctionEnv<'_>,
    ) -> Result<FuncId> {
        let key = shapes::TypeId::intern_slice(env.ctx, &arguments);
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
                for ((param, prior), next) in function.type_params.iter().zip(previous).zip(&key) {
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
        instances.insert(key, id);
        let bindings = function
            .type_params
            .iter()
            .zip(&arguments)
            .map(|(param, arg)| (param.name.as_str(), arg))
            .collect();
        let mut instance = function.clone();
        instance.id = id;
        instance.name = format!("{}_instance_{id}", function.name);
        if !env.metadata.locations.has_explicit_return(&function.name) {
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
                Node::Expression(expr) => materialize_constructor(expr, env.metadata, &bindings),
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
        walk::scan(body, env, None, None);
        walk::visit_body(body, &mut |node: Node<'_>| {
            if let Node::Expression(expr) = node {
                self.call(expr, caller, env)?;
            }
            Ok(())
        })
    }

    fn call(
        &mut self,
        expr: &mut Expr,
        caller: Option<(FuncId, &str)>,
        env: &mut FunctionEnv<'_>,
    ) -> Result<()> {
        if let Expr::Call {
            callee,
            args,
            type_args,
            byte_offset,
            ..
        } = expr
            && let Expr::FuncRef(id) = callee.as_mut()
            && let Some(function) = self.templates.get(id)
        {
            let arguments = match arguments(function, args, type_args, env) {
                Ok(arguments) => arguments,
                Err(error) if error.code() == codes::MISSING_TYPE_ANNOTATION => {
                    self.blocked.get_or_insert_with(|| {
                        (
                            env.metadata.locations.annotate_call(error, *byte_offset),
                            caller.map(|(_, name)| name.to_owned()),
                        )
                    });
                    return Ok(());
                }
                Err(error) => {
                    return Err(env.metadata.locations.annotate_call(error, *byte_offset));
                }
            };
            *id = self
                .request(function, arguments, *byte_offset, caller, env)
                .map_err(|error| env.metadata.locations.annotate_call(error, *byte_offset))?;
            type_args.clear();
            self.progressed = true;
        }
        Ok(())
    }

    fn expression(
        &mut self,
        expr: &mut Expr,
        caller: Option<(FuncId, &str)>,
        env: &mut FunctionEnv<'_>,
    ) -> Result<()> {
        walk::visit_expr(expr, &mut |node: Node<'_>| {
            if let Node::Expression(expr) = node {
                self.call(expr, caller, env)?;
            }
            Ok(())
        })
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
) -> Result<()> {
    if hir
        .functions
        .iter()
        .all(|function| function.type_params.is_empty())
    {
        return Ok(());
    }
    let mut base = perry_hir::HirTypeEnv::from_module(hir);
    let mut templates = HashMap::new();
    let mut next_function = 0;
    let mut next_local = hir.local_source_spans.keys().copied().max().unwrap_or(0);
    let mut collect_local = |node: Node<'_>| {
        if let Node::Binding(id, _) | Node::Local(id) = node {
            next_local = next_local.max(*id);
        }
        Ok(())
    };
    walk::visit_body(&mut hir.init, &mut collect_local)?;
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
        next_function = next_function.max(function.id);
        walk::function(&mut function, &mut collect_local)?;
        if function.type_params.is_empty() {
            hir.functions.push(function);
        } else {
            if function.is_exported {
                hir.functions.push(function.clone());
            }
            base.insert_function_return(function.id, HirType::Any);
            templates.insert(function.id, function);
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
        templates: &templates,
        keys: HashMap::new(),
        blocked: None,
        progressed: false,
        pending: Vec::new(),
        next_function: next_function
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
        if first_error.is_none() || !instances.progressed {
            break first_error;
        }
        std::mem::swap(&mut ready, &mut waiting);
    };
    for (old_id, new_id) in instances.remapped_locals {
        if let Some(span) = hir.local_source_spans.get(&old_id).copied() {
            hir.local_source_spans.insert(new_id, span);
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
    Ok(())
}
