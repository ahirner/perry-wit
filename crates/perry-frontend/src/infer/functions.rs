//! Concrete function instantiations keyed by structural source types.
use super::walk::Node;
use super::*;
use perry_hir::ir::Function;

pub(super) fn arguments(
    function: &Function,
    args: &[Expr],
    explicit: &[HirType],
    env: &mut FunctionEnv<'_>,
) -> Result<Vec<HirType>> {
    ensure!(
        explicit.len() <= function.type_params.len(),
        "too many type arguments for {}",
        function.name
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
                .filter(|ty| !contains_type_var(ty) && !matches!(ty, HirType::Any | HirType::Never))
                .with_context(|| {
                    format!(
                        "cannot infer type argument {} of {}",
                        param.name, function.name
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
    keys: HashMap<(FuncId, Vec<shapes::TypeId>), FuncId>,
    pending: Vec<Function>,
    next_function: FuncId,
    next_local: LocalId,
}

impl Instances<'_> {
    fn request(
        &mut self,
        function: &Function,
        arguments: Vec<HirType>,
        env: &mut FunctionEnv<'_>,
    ) -> Result<FuncId> {
        let key = (
            function.id,
            shapes::TypeId::intern_slice(env.ctx, &arguments),
        );
        if let Some(id) = self.keys.get(&key) {
            return Ok(*id);
        }
        ensure!(
            self.keys.len() < 256,
            "generic function instantiation expands indefinitely: {}",
            function.name
        );
        let id = self.next_function;
        self.next_function = id.checked_add(1).context("function identifier limit")?;
        self.keys.insert(key, id);
        let bindings = function
            .type_params
            .iter()
            .zip(&arguments)
            .map(|(param, arg)| (param.name.as_str(), arg))
            .collect();
        let mut instance = function.clone();
        instance.id = id;
        instance.name = format!("{}_instance_{id}", function.name);
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
        for param in &mut instance.params {
            if let Some(default) = &mut param.default {
                perry_hir::analysis::remap_local_ids_in_expr(default, &locals);
            }
        }
        perry_hir::analysis::remap_local_ids_in_stmts(&mut instance.body, &locals);
        if matches!(instance.return_type, HirType::Any | HirType::Unknown) {
            let locals = instance
                .params
                .iter()
                .map(|param| (param.id, param.ty.clone()))
                .collect();
            let caller_locals = std::mem::replace(env.locals, locals);
            infer_body_return(
                &instance.body,
                instance.is_async,
                &mut instance.return_type,
                env,
                env.decls,
            );
            *env.locals = caller_locals;
        }
        env.instantiated_returns
            .insert(id, instance.return_type.clone());
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
                self.next_local = next.checked_add(1).context("local identifier limit")?;
                *entry.insert(next)
            }
        };
        Ok(())
    }

    fn body(&mut self, body: &mut [Stmt], env: &mut FunctionEnv<'_>) -> Result<()> {
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
                self.call(expr, env)?;
            }
            Ok(())
        })
    }

    fn call(&mut self, expr: &mut Expr, env: &mut FunctionEnv<'_>) -> Result<()> {
        if let Expr::Call {
            callee,
            args,
            type_args,
            byte_offset,
        } = expr
            && let Expr::FuncRef(id) = callee.as_mut()
            && let Some(function) = self.templates.get(id)
        {
            let arguments = arguments(function, args, type_args, env)
                .map_err(|error| env.metadata.locations.annotate_call(error, *byte_offset))?;
            *id = self.request(function, arguments, env)?;
            type_args.clear();
        }
        Ok(())
    }

    fn expression(&mut self, expr: &mut Expr, env: &mut FunctionEnv<'_>) -> Result<()> {
        walk::visit_expr(expr, &mut |node: Node<'_>| {
            if let Node::Expression(expr) = node {
                self.call(expr, env)?;
            }
            Ok(())
        })
    }

    fn callable(
        &mut self,
        params: &mut [perry_hir::ir::Param],
        body: &mut [Stmt],
        env: &mut FunctionEnv<'_>,
    ) -> Result<()> {
        for param in params {
            if let Some(default) = &mut param.default {
                self.expression(default, env)?;
            }
        }
        self.body(body, env)
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
    metadata: &hir_compat::Metadata<'_>,
    foreign: &HashMap<String, crate::hir::Foreign>,
) -> Result<()> {
    if hir
        .functions
        .iter()
        .all(|function| function.type_params.is_empty())
    {
        return Ok(());
    }
    let open_arrays = arrays::open_bindings(hir);
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
        generic_functions: Some(&templates),
        open_arrays,
        local_spans: &hir.local_source_spans,
    };
    let mut instances = Instances {
        templates: &templates,
        keys: HashMap::new(),
        pending: Vec::new(),
        next_function: next_function
            .checked_add(1)
            .context("function identifier limit")?,
        next_local: next_local
            .checked_add(1)
            .context("local identifier limit")?,
    };
    let mut locals = HashMap::new();
    {
        let mut env = inference.function(&base, &hir.classes, &mut locals);
        instances.body(&mut hir.init, &mut env)?;
    }
    instances.append(&mut hir.functions, &mut base);
    for class_idx in 0..hir.classes.len() {
        base.set_current_class(Some(hir.classes[class_idx].name.clone()));
        let mut inits: Vec<Expr> = class_initializers(&mut hir.classes[class_idx])
            .map(|init| std::mem::replace(init, Expr::Undefined))
            .collect();
        for init in &mut inits {
            let mut env = inference.function(&base, &hir.classes, &mut locals);
            instances.expression(init, &mut env)?;
            instances.append(&mut hir.functions, &mut base);
        }
        for (dst, src) in class_initializers(&mut hir.classes[class_idx]).zip(inits) {
            *dst = src;
        }
        let mut class_fns: Vec<(Vec<perry_hir::ir::Param>, Vec<Stmt>)> =
            shapes::class_functions_mut(&mut hir.classes[class_idx])
                .map(|target| {
                    (
                        std::mem::take(&mut target.params),
                        std::mem::take(&mut target.body),
                    )
                })
                .collect();
        for (params, body) in &mut class_fns {
            let mut env = inference.function_with_params(&base, &hir.classes, &mut locals, params);
            instances.callable(params, body, &mut env)?;
            instances.append(&mut hir.functions, &mut base);
        }
        for (target, (params, body)) in
            shapes::class_functions_mut(&mut hir.classes[class_idx]).zip(class_fns)
        {
            target.params = params;
            target.body = body;
        }
    }
    base.set_current_class(None);
    let mut cursor = 0;
    loop {
        instances.append(&mut hir.functions, &mut base);
        let Some(function) = hir.functions.get_mut(cursor) else {
            break;
        };
        cursor += 1;
        if !function.type_params.is_empty() {
            continue;
        }
        let mut env =
            inference.function_with_params(&base, &hir.classes, &mut locals, &function.params);
        instances.callable(&mut function.params, &mut function.body, &mut env)?;
    }
    Ok(())
}
