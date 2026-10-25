//! Static element constraints for empty arrays and their local aliases.
use super::*;
use crate::shapes::{
    TypeId,
    generics::{ARGUMENT_LIMIT, Declaration, Resolver, substitute_id},
};
use perry_hir::{
    ir::CallArg,
    walker::{stmt_any_expr, walk_expr_children},
};
use perry_wit_diagnostics::{DiagnosticCode, PerryDiagnostic};

pub(super) fn open_bindings(module: &HirModule) -> HashSet<LocalId> {
    let mut open = HashSet::new();
    let mut collect = |statement: &Stmt| {
        if let Stmt::Let { id, ty, .. } = statement
            && matches!(array_element(ty), Some(HirType::Any))
            && !matches!(ty, HirType::Generic { base, .. } if base == "ReadonlyArray")
        {
            open.insert(*id);
        }
    };
    walk::statements(&module.init, &mut collect);
    for function in &module.functions {
        walk::statements(&function.body, &mut collect);
    }
    for class in &module.classes {
        for function in shapes::class_functions(class) {
            walk::statements(&function.body, &mut collect);
        }
    }
    open
}

fn array_element(ty: &HirType) -> Option<&HirType> {
    match ty {
        HirType::Array(element) => Some(element),
        HirType::Generic { base, type_args }
            if matches!(base.as_str(), "Array" | "ReadonlyArray") =>
        {
            match type_args.as_slice() {
                [element] => Some(element),
                _ => None,
            }
        }
        _ => None,
    }
}

fn local(expr: &Expr) -> Option<LocalId> {
    match expr {
        Expr::LocalGet(id) => Some(*id),
        _ => None,
    }
}

#[derive(Default)]
struct Constraints<'s> {
    bindings: HashMap<LocalId, (&'s HirType, Option<&'s Expr>)>,
    aliases: Vec<(LocalId, LocalId)>,
    pushes: Vec<(LocalId, &'s Expr, bool)>,
    indexed: Vec<(LocalId, &'s Expr, &'s Expr)>,
}

impl<'s> Constraints<'s> {
    fn expression(&mut self, expr: &'s Expr) {
        if matches!(expr, Expr::Closure { .. }) {
            return;
        }
        match expr {
            Expr::LocalSet(id, value) => self.alias(*id, value),
            Expr::IndexSet {
                object,
                index,
                value,
            } => {
                if let Some(id) = local(object) {
                    self.indexed.push((id, index, value));
                }
            }
            Expr::PutValueSet {
                target,
                key,
                value,
                receiver,
                ..
            } => {
                if let Some(id) = local(target)
                    && local(receiver) == Some(id)
                {
                    self.indexed.push((id, key, value));
                }
            }
            Expr::ArrayPushSpread { array_id, source } => {
                self.pushes.push((*array_id, source, true));
            }
            Expr::Call { callee, args, .. } => {
                if let Expr::PropertyGet {
                    object, property, ..
                } = callee.as_ref()
                    && let Some(id) = local(object)
                    && let Ok(Some(crate::methods::Call::ArrayPush(values))) =
                        crate::methods::Call::parse(
                            crate::methods::Receiver::Value(Kind::Array),
                            hir_compat::method(property).name(),
                            args,
                        )
                {
                    self.pushes
                        .extend(values.iter().map(|value| (id, value, false)));
                }
            }
            Expr::CallSpread { callee, args, .. } => {
                if let Expr::PropertyGet {
                    object, property, ..
                } = callee.as_ref()
                    && hir_compat::method(property).name() == "push"
                    && let Some(id) = local(object)
                {
                    self.pushes
                        .extend(args.iter().map(|argument| match argument {
                            CallArg::Expr(value) => (id, value, false),
                            CallArg::Spread(value) => (id, value, true),
                        }));
                }
            }
            _ => {
                if let Some((receiver, crate::methods::Call::ArrayPush(values))) =
                    crate::methods::Call::expression(expr)
                {
                    let id = match receiver {
                        crate::methods::CallReceiver::Local(id) => Some(id),
                        crate::methods::CallReceiver::Expression(expr) => local(expr),
                    };
                    if let Some(id) = id {
                        self.pushes
                            .extend(values.iter().map(|value| (id, value, false)));
                    }
                }
            }
        }
        walk_expr_children(expr, &mut |child| self.expression(child));
    }

    fn alias(&mut self, id: LocalId, value: &Expr) {
        match value {
            Expr::LocalGet(source) => self.aliases.push((id, *source)),
            Expr::Conditional {
                then_expr,
                else_expr,
                ..
            } => {
                self.alias(id, then_expr);
                self.alias(id, else_expr);
            }
            Expr::Sequence(parts) => {
                if let Some(value) = parts.last() {
                    self.alias(id, value);
                }
            }
            _ => {}
        }
    }
}

fn root(parents: &HashMap<LocalId, LocalId>, mut id: LocalId) -> LocalId {
    while let Some(&parent) = parents.get(&id)
        && parent != id
    {
        id = parent;
    }
    id
}

fn diagnostic(
    env: &FunctionEnv<'_>,
    id: LocalId,
    code: DiagnosticCode,
    message: impl Into<String>,
) -> anyhow::Error {
    let mut error = PerryDiagnostic::error(code, message);
    if code == DiagnosticCode::MissingTypeAnnotation {
        error.help = Some("Add a concrete array element type supported by Perry.".into());
    }
    error.span = env.local_spans.get(&id).and_then(|span| {
        env.metadata
            .locations
            .offset_span(span.start, span.end.saturating_sub(span.start))
    });
    error.into()
}

pub(super) fn infer(
    body: &[Stmt],
    env: &mut FunctionEnv<'_>,
    complete: bool,
) -> Result<HashMap<LocalId, HirType>> {
    let mut constraints = Constraints::default();
    walk::statements(body, &mut |statement| {
        if let Stmt::Let { id, ty, init, .. } = statement {
            constraints.bindings.insert(*id, (ty, init.as_ref()));
            if let Some(init) = init {
                constraints.alias(*id, init);
            }
        }
    });
    let open =
        |id: &LocalId| env.open_arrays.contains(id) || env.metadata.inferred_bindings.contains(id);
    let mut candidates: Vec<_> = constraints
        .bindings
        .iter()
        .filter_map(|(&id, (_, init))| {
            (open(&id) && matches!(init, Some(Expr::Array(items, _)) if items.is_empty()))
                .then_some(id)
        })
        .collect();
    candidates.sort_unstable();
    let Some(&first_candidate) = candidates.first() else {
        return Ok(HashMap::new());
    };
    for statement in body {
        stmt_any_expr(statement, &mut |expr| {
            constraints.expression(expr);
            false
        });
    }
    let mut parents: HashMap<_, _> = constraints.bindings.keys().map(|&id| (id, id)).collect();
    for &(left, right) in &constraints.aliases {
        if parents.contains_key(&left) && parents.contains_key(&right) {
            let left = root(&parents, left);
            let right = root(&parents, right);
            parents.insert(left.max(right), left.min(right));
        }
    }
    let groups: HashSet<_> = candidates.iter().map(|&id| root(&parents, id)).collect();
    let push_groups: HashSet<_> = constraints
        .pushes
        .iter()
        .map(|(id, _, _)| root(&parents, *id))
        .collect();
    let mut members: Vec<_> = constraints
        .bindings
        .iter()
        .filter(|(id, _)| groups.contains(&root(&parents, **id)))
        .map(|(&id, &binding)| (id, binding))
        .collect();
    members.sort_unstable_by_key(|(id, _)| *id);
    let mut fixed = HashMap::new();
    for &(id, (ty, _)) in &members {
        if !open(&id)
            && let Some(array) = Normalizer::new(env, false).resolve(ty, false)?
            && let Some(element) = array_element(array.hir(env.ctx))
        {
            let element = TypeId::intern(env.ctx, element);
            let group = root(&parents, id);
            if let Some(other) = fixed.insert(group, element)
                && other != element
                && !matches!((other.hir(env.ctx), element.hir(env.ctx)),
                    (HirType::Union(left), HirType::Union(right))
                    if left.len() == right.len() && left.iter().all(|ty| right.contains(ty)))
            {
                return Err(diagnostic(
                    env,
                    id,
                    DiagnosticCode::UnsupportedType,
                    format!(
                        "array aliases have incompatible concrete element annotations: {:?} and {:?}",
                        other.hir(env.ctx),
                        element.hir(env.ctx)
                    ),
                ));
            }
        }
    }
    let mut inferred = HashMap::new();
    for _ in 0..=constraints.bindings.len() + constraints.pushes.len() + constraints.indexed.len() {
        walk::scan_statements(body, env, None, None);
        let mut evidence: HashMap<LocalId, Vec<TypeId>> = HashMap::new();
        for &(id, value, spread) in &constraints.pushes {
            let group = root(&parents, id);
            if !groups.contains(&group) || !parents.contains_key(&id) {
                continue;
            }
            let ty = if spread {
                shapes::generics::iterated_element(expression(value, env)).ok()
            } else {
                Some(argument_type(value, env))
            };
            if let Some(ty) = ty {
                let normalized =
                    Normalizer::new(env, true)
                        .resolve(&ty, false)
                        .map_err(|error| {
                            diagnostic(
                                env,
                                id,
                                DiagnosticCode::MissingTypeAnnotation,
                                format!("cannot infer the empty array's element type: {error}"),
                            )
                        })?;
                if let Some(ty) = normalized {
                    evidence.entry(group).or_default().push(ty);
                }
            }
        }
        for &(id, index, value) in &constraints.indexed {
            let group = root(&parents, id);
            if groups.contains(&group)
                && !push_groups.contains(&group)
                && matches!(expression(index, env), HirType::Number | HirType::Int32)
            {
                let ty = argument_type(value, env);
                if let Some(ty) = Normalizer::new(env, true).resolve(&ty, false)? {
                    evidence.entry(group).or_default().push(ty);
                }
            }
        }
        for &(id, (_, init)) in &members {
            if let Some(init) = init
                && !matches!(init, Expr::LocalGet(_) | Expr::Conditional { .. })
                && !matches!(init, Expr::Array(items, _) if items.is_empty())
                && let Some(element) = array_element(&expression(init, env))
                && let Some(element) = Normalizer::new(env, true).resolve(element, false)?
            {
                evidence
                    .entry(root(&parents, id))
                    .or_default()
                    .push(element);
            }
        }
        let mut changed = false;
        for &group in &groups {
            let element = if let Some(&fixed) = fixed.get(&group) {
                Some(fixed)
            } else {
                let types = evidence.remove(&group).unwrap_or_default();
                let mut unique = Vec::new();
                for ty in types {
                    if !unique.contains(&ty) {
                        unique.push(ty);
                    }
                }
                if !push_groups.contains(&group) && unique.len() > 1 {
                    return Err(diagnostic(
                        env,
                        group,
                        DiagnosticCode::UnsupportedType,
                        format!(
                            "indexed writes require one uniform array element type, found {:?}",
                            unique.iter().map(|ty| ty.hir(env.ctx)).collect::<Vec<_>>()
                        ),
                    ));
                }
                match unique.as_slice() {
                    [] => None,
                    [ty] => Some(*ty),
                    _ => Normalizer::new(env, true).resolve_union(&unique, false)?,
                }
            };
            if let Some(element) = element {
                changed |= inferred.insert(group, element) != Some(element);
            }
        }
        for &(id, _) in &members {
            if let Some(element) = inferred.get(&root(&parents, id)) {
                let element = element.hir(env.ctx);
                if !matches!(env.locals.get(&id), Some(HirType::Array(current)) if current.as_ref() == element)
                {
                    env.locals
                        .insert(id, HirType::Array(Box::new(element.clone())));
                }
            }
        }
        if !changed {
            if complete
                && let Some(&id) = candidates
                    .iter()
                    .find(|id| !inferred.contains_key(&root(&parents, **id)))
            {
                return Err(diagnostic(
                    env,
                    id,
                    DiagnosticCode::MissingTypeAnnotation,
                    "the empty array's element type could not be inferred from local pushes",
                ));
            }
            return Ok(members
                .into_iter()
                .filter(|(id, _)| open(id))
                .filter_map(|(id, _)| {
                    inferred
                        .get(&root(&parents, id))
                        .map(|element| (id, HirType::Array(Box::new(element.hir(env.ctx).clone()))))
                })
                .collect());
        }
    }
    Err(diagnostic(
        env,
        first_candidate,
        DiagnosticCode::MissingTypeAnnotation,
        "array element inference does not converge; a concrete annotation is required",
    ))
}

fn argument_type(expr: &Expr, env: &mut FunctionEnv<'_>) -> HirType {
    if let Expr::Conditional {
        then_expr,
        else_expr,
        ..
    } = expr
    {
        return HirType::Union(vec![
            argument_type(then_expr, env),
            argument_type(else_expr, env),
        ]);
    }
    let mut ty = expression(expr, env);
    if let HirType::Object(object) = &mut ty {
        let tag = match expr {
            Expr::Object(properties) => properties
                .iter()
                .find(|(name, _)| name == "tag")
                .map(|(_, value)| value),
            Expr::New {
                class_name, args, ..
            } if hir_compat::is_object_literal(class_name) => env
                .classes
                .iter()
                .find(|class| class.name == *class_name)
                .and_then(|class| class.fields.iter().position(|field| field.name == "tag"))
                .and_then(|index| args.get(index)),
            _ => None,
        };
        if let Some(Expr::String(tag)) = tag
            && let Some(property) = object.properties.get_mut("tag")
        {
            property.ty = HirType::StringLiteral(tag.clone());
        }
    }
    ty
}

struct Normalizer<'a, 'e> {
    env: &'a FunctionEnv<'e>,
    widen: bool,
    remaining: usize,
    depth: usize,
    expanding: HashSet<TypeId>,
}

impl<'a, 'e> Normalizer<'a, 'e> {
    fn new(env: &'a FunctionEnv<'e>, widen: bool) -> Self {
        Self {
            env,
            widen,
            remaining: ARGUMENT_LIMIT,
            depth: 0,
            expanding: HashSet::new(),
        }
    }

    fn resolve(&mut self, ty: &HirType, literal: bool) -> Result<Option<TypeId>> {
        ensure!(
            self.remaining > 0 && self.depth < 64,
            "array element type expands indefinitely"
        );
        self.remaining -= 1;
        self.depth += 1;
        let result = self.resolve_inner(ty, literal);
        self.depth -= 1;
        result
    }

    fn resolve_union(&mut self, members: &[TypeId], literal: bool) -> Result<Option<TypeId>> {
        let env = self.env;
        let mut normalized = Vec::new();
        for ty in Resolver::new(env.ctx, env.decls).flatten(members, 0) {
            let Some(ty) = self.resolve(ty.hir(env.ctx), literal)? else {
                return Ok(None);
            };
            if !normalized.contains(&ty) {
                normalized.push(ty);
            }
        }
        if let [ty] = normalized.as_slice() {
            return Ok(Some(*ty));
        }
        Ok(Some(TypeId::from_owned(
            env.ctx,
            HirType::Union(
                normalized
                    .into_iter()
                    .map(|ty| ty.hir(env.ctx).clone())
                    .collect(),
            ),
        )))
    }

    fn resolve_record<'t>(
        &mut self,
        fields: impl IntoIterator<Item = (impl Into<String> + AsRef<str>, &'t HirType, bool)>,
        index: Option<&HirType>,
    ) -> Result<Option<TypeId>> {
        let ctx = self.env.ctx;
        let mut object = perry_hir::types::ObjectType::default();
        for (name, ty, optional) in fields {
            let Some(ty) = self.resolve(ty, name.as_ref() == "tag")? else {
                return Ok(None);
            };
            object.properties.insert(
                name.into(),
                perry_hir::types::PropertyInfo {
                    ty: ty.hir(ctx).clone(),
                    optional,
                    readonly: false,
                },
            );
        }
        if let Some(index) = index {
            let Some(ty) = self.resolve(index, false)? else {
                return Ok(None);
            };
            object.index_signature = Some(Box::new(ty.hir(ctx).clone()));
        }
        Ok(Some(TypeId::from_owned(ctx, HirType::Object(object))))
    }

    fn resolve_inner(&mut self, ty: &HirType, literal: bool) -> Result<Option<TypeId>> {
        let env = self.env;
        let ty = match ty {
            HirType::Any | HirType::TypeVar(_) => return Ok(None),
            HirType::Unknown | HirType::Never if self.widen => return Ok(None),
            HirType::Int32 if self.widen => HirType::Number,
            HirType::StringLiteral(_) if self.widen && !literal => HirType::String,
            HirType::Array(inner) => {
                let Some(inner) = self.resolve(inner, false)? else {
                    return Ok(None);
                };
                HirType::Array(Box::new(inner.hir(env.ctx).clone()))
            }
            HirType::Object(object) => {
                return self.resolve_record(
                    object
                        .properties
                        .iter()
                        .map(|(name, property)| (name.as_str(), &property.ty, property.optional)),
                    object.index_signature.as_deref(),
                );
            }
            HirType::Union(members) => {
                return self.resolve_union(&TypeId::intern_slice(env.ctx, members), literal);
            }
            HirType::Named(name) | HirType::Generic { base: name, .. } => {
                if self.widen
                    && matches!(ty, HirType::Named(_))
                    && let Some(carrier) = env.decls.integer_carrier(name)
                {
                    return Ok(Some(TypeId::from_owned(
                        env.ctx,
                        if carrier == HirType::BigInt {
                            HirType::BigInt
                        } else {
                            HirType::Number
                        },
                    )));
                }
                let arguments = if let HirType::Generic { type_args, .. } = ty {
                    type_args.as_slice()
                } else {
                    &[]
                };
                if let Some((_, alias)) = env.decls.type_aliases.get(name.as_str()) {
                    let id = TypeId::intern(env.ctx, ty);
                    if !self.expanding.insert(id) {
                        return Ok(Some(id));
                    }
                    let args = TypeId::intern_slice(env.ctx, arguments);
                    let bindings = Declaration::Alias(alias).bind(env.ctx, name, &args)?;
                    let expanded =
                        substitute_id(env.ctx, &alias.ty, &bindings, &mut self.remaining)?;
                    let result = self.resolve(expanded.hir(env.ctx), literal);
                    self.expanding.remove(&id);
                    return result;
                }
                if let Some(element) = array_element(ty) {
                    let Some(element) = self.resolve(element, false)? else {
                        return Ok(None);
                    };
                    return Ok(Some(TypeId::from_owned(
                        env.ctx,
                        HirType::Array(Box::new(element.hir(env.ctx).clone())),
                    )));
                }
                if env.decls.interfaces.contains_key(name.as_str())
                    && let Some(fields) =
                        Resolver::new(env.ctx, env.decls).record(TypeId::intern(env.ctx, ty), 0)
                {
                    let id = TypeId::intern(env.ctx, ty);
                    if !self.expanding.insert(id) {
                        return Ok(Some(id));
                    }
                    let result = self.resolve_record(
                        fields
                            .into_iter()
                            .map(|(name, ty, optional)| (name, ty.hir(env.ctx), optional)),
                        None,
                    );
                    self.expanding.remove(&id);
                    return result;
                }
                if !is_resolvable_type(ty) {
                    return Ok(None);
                }
                return Ok(Some(TypeId::intern(env.ctx, ty)));
            }
            other => {
                if !is_resolvable_type(other)
                    && !matches!(
                        other,
                        HirType::Null | HirType::Void | HirType::Unknown | HirType::Never
                    )
                {
                    return Ok(None);
                }
                return Ok(Some(TypeId::intern(env.ctx, other)));
            }
        };
        Ok(Some(TypeId::from_owned(env.ctx, ty)))
    }
}
