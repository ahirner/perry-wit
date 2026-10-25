use super::*;
use perry_hir::analysis::{collect_local_refs_expr, collect_local_refs_stmt};
use std::convert::Infallible;
use walk::{Node, Visitor};

#[derive(Clone, Copy, PartialEq, Eq)]
enum AggregateKind {
    Array,
    Dictionary,
}

#[derive(Default)]
enum ObservedValues {
    #[default]
    Empty,
    Known(Vec<HirType>),
    Unknown,
}

impl ObservedValues {
    fn observe(&mut self, ty: &HirType) {
        if !is_resolvable_type(ty) {
            *self = Self::Unknown;
        } else {
            match self {
                Self::Empty => *self = Self::Known(vec![ty.clone()]),
                Self::Known(types) if !types.contains(ty) => types.push(ty.clone()),
                _ => {}
            }
        }
    }

    fn inferred(&self, kind: AggregateKind) -> Option<HirType> {
        match self {
            Self::Known(types) if types.len() == 1 => types.first().cloned(),
            Self::Known(types) if kind == AggregateKind::Array => {
                Some(HirType::Union(types.clone()))
            }
            _ => None,
        }
    }
}

struct Write {
    kind: AggregateKind,
    value: HirType,
}

#[derive(Default)]
struct Constraints {
    fresh: HashMap<LocalId, AggregateKind>,
    aliases: HashMap<LocalId, LocalId>,
    writes: HashMap<LocalId, Vec<Write>>,
    escaped: HashSet<LocalId>,
    element_reads: HashSet<LocalId>,
}

impl Constraints {
    fn root(&self, mut id: LocalId) -> LocalId {
        for _ in 0..self.aliases.len() {
            let Some(source) = self.aliases.get(&id) else {
                break;
            };
            id = *source;
        }
        id
    }

    fn bindings(self) -> HashMap<LocalId, HirType> {
        let escaped: HashSet<_> = self.escaped.iter().map(|id| self.root(*id)).collect();
        let mut elements: HashMap<LocalId, ObservedValues> = HashMap::new();
        for (id, writes) in &self.writes {
            let root = self.root(*id);
            let Some(kind) = self.fresh.get(&root) else {
                continue;
            };
            let values = elements.entry(root).or_default();
            for write in writes {
                if escaped.contains(&root) || write.kind != *kind {
                    *values = ObservedValues::Unknown;
                } else {
                    values.observe(&write.value);
                }
            }
        }
        let read_roots: HashSet<_> = self.element_reads.iter().map(|id| self.root(*id)).collect();
        self.fresh
            .keys()
            .chain(self.aliases.keys())
            .filter_map(|id| {
                let root = self.root(*id);
                let value = elements.get(&root)?.inferred(*self.fresh.get(&root)?)?;
                if read_roots.contains(&root)
                    && self.fresh.get(&root) == Some(&AggregateKind::Dictionary)
                    && !matches!(
                        value,
                        HirType::Number | HirType::String | HirType::Boolean | HirType::BigInt
                    )
                {
                    return None;
                }
                let ty = match self.fresh.get(&root)? {
                    AggregateKind::Array => HirType::Array(Box::new(value)),
                    AggregateKind::Dictionary => HirType::Object(perry_hir::types::ObjectType {
                        index_signature: Some(Box::new(value)),
                        ..Default::default()
                    }),
                };
                Some((*id, ty))
            })
            .collect()
    }
}

fn unresolved_array(ty: &HirType) -> bool {
    match ty {
        HirType::Any | HirType::Unknown => true,
        HirType::Array(element) => matches!(**element, HirType::Any | HirType::Unknown),
        HirType::Generic { base, type_args } if base == "Array" => {
            type_args.is_empty()
                || matches!(type_args.as_slice(), [HirType::Any | HirType::Unknown])
        }
        _ => false,
    }
}

fn element_type(value: &Expr, env: &mut FunctionEnv<'_>) -> HirType {
    let mut ty = match value {
        Expr::Conditional {
            then_expr,
            else_expr,
            ..
        }
        | Expr::Logical {
            left: then_expr,
            right: else_expr,
            ..
        } => {
            let left = element_type(then_expr, env);
            if left == element_type(else_expr, env) {
                left
            } else {
                HirType::Any
            }
        }
        Expr::Array(values, _) => {
            let Some(first) = values.first() else {
                return HirType::Any;
            };
            let element = element_type(first, env);
            if values[1..]
                .iter()
                .all(|value| element_type(value, env) == element)
            {
                HirType::Array(Box::new(element))
            } else {
                HirType::Any
            }
        }
        _ => {
            let mut ty = expression(value, env);
            if let HirType::Object(object) = &mut ty {
                let fields: Option<Vec<_>> = match value {
                    Expr::New { class_name, .. } => env
                        .classes
                        .iter()
                        .find(|class| {
                            class.name == *class_name && hir_compat::is_object_literal(class_name)
                        })
                        .map(|class| {
                            class
                                .fields
                                .iter()
                                .map(|field| field.name.as_str())
                                .collect()
                        }),
                    _ => None,
                };
                if let Some(pairs) = hir_compat::object_literal_pairs(value, |_| fields.as_deref())
                {
                    for (name, value) in pairs {
                        if let Some(property) = object.properties.get_mut(name) {
                            property.ty = element_type(value, env);
                        }
                    }
                }
            }
            ty
        }
    };
    ty.transform(&mut |ty| match ty {
        HirType::StringLiteral(_) => *ty = HirType::String,
        HirType::Int32 => *ty = HirType::Number,
        HirType::Union(members) => {
            let mut distinct = Vec::with_capacity(members.len());
            for member in members.drain(..) {
                if !distinct.contains(&member) {
                    distinct.push(member);
                }
            }
            *ty = match <[HirType; 1]>::try_from(distinct) {
                Ok([member]) => member,
                Err(distinct) => HirType::Union(distinct),
            };
        }
        _ => {}
    });
    ty
}

struct Collect<'a, 'e> {
    constraints: &'a mut Constraints,
    env: FunctionEnv<'e>,
}

impl Collect<'_, '_> {
    fn write(&mut self, id: LocalId, kind: AggregateKind, value: &Expr) {
        self.constraints.writes.entry(id).or_default().push(Write {
            kind,
            value: element_type(value, &mut self.env),
        });
    }

    fn key_kind(&mut self, key: &Expr) -> Option<AggregateKind> {
        match expression(key, &mut self.env) {
            HirType::Number | HirType::Int32 => Some(AggregateKind::Array),
            HirType::String | HirType::StringLiteral(_) => Some(AggregateKind::Dictionary),
            _ => None,
        }
    }

    fn empty_object(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Object(fields) => fields.is_empty(),
            Expr::New {
                class_name, args, ..
            } => {
                args.is_empty()
                    && hir_compat::is_object_literal(class_name)
                    && self
                        .env
                        .classes
                        .iter()
                        .any(|class| class.name == *class_name && class.fields.is_empty())
            }
            _ => false,
        }
    }
}

impl Visitor for Collect<'_, '_> {
    type Error = Infallible;

    fn statement(&mut self, statement: &mut Stmt) -> Result<(), Infallible> {
        if let Stmt::Return(Some(Expr::LocalGet(id))) = statement
            && self.constraints.fresh.get(&self.constraints.root(*id))
                == Some(&AggregateKind::Array)
        {
            return Ok(());
        }
        if let Stmt::Let {
            id,
            ty,
            mutable: false,
            init: Some(init),
            ..
        } = statement
            && (self.env.metadata.inferred_bindings.contains(id)
                || matches!(ty, HirType::Any | HirType::Unknown))
            && !matches!(init, Expr::Array(values, _) if values.is_empty())
            && !self.empty_object(init)
        {
            let inferred = element_type(init, &mut self.env);
            self.env.locals.insert(*id, inferred);
        }
        if let Stmt::Let {
            id,
            ty,
            init: Some(init),
            ..
        } = statement
            && (unresolved_array(ty)
                || matches!(ty, HirType::Object(object) if object.properties.is_empty() && object.index_signature.is_none()))
        {
            match init {
                Expr::Array(values, _) if values.is_empty() => {
                    self.constraints.fresh.insert(*id, AggregateKind::Array);
                }
                init if self.empty_object(init) => {
                    self.constraints
                        .fresh
                        .insert(*id, AggregateKind::Dictionary);
                }
                Expr::LocalGet(source) => {
                    self.constraints.aliases.insert(*id, *source);
                    return Ok(());
                }
                _ => {}
            }
        }
        walk::visit_stmt(statement, self)
    }

    fn expression(&mut self, expr: &mut Expr) -> Result<(), Infallible> {
        match expr {
            Expr::ArrayPush {
                array_id, value, ..
            } => {
                self.write(*array_id, AggregateKind::Array, value);
                return self.expression(value);
            }
            Expr::IndexSet {
                object,
                index,
                value,
            } => {
                if let Expr::LocalGet(id) = **object
                    && let Some(kind) = self.key_kind(index)
                {
                    self.write(id, kind, value);
                    self.expression(index)?;
                    return self.expression(value);
                }
            }
            Expr::PutValueSet {
                target,
                key,
                value,
                receiver,
                ..
            } => {
                if let (Expr::LocalGet(id), Expr::LocalGet(receiver)) = (&**target, &**receiver)
                    && id == receiver
                    && let Some(kind) = self.key_kind(key)
                {
                    self.write(*id, kind, value);
                    self.expression(key)?;
                    return self.expression(value);
                }
            }
            Expr::PropertySet { object, value, .. } if let Expr::LocalGet(id) = **object => {
                self.write(id, AggregateKind::Dictionary, value);
                return self.expression(value);
            }
            Expr::Call { callee, args, .. } => {
                if let Expr::PropertyGet {
                    object, property, ..
                } = callee.as_ref()
                    && let Expr::LocalGet(id) = **object
                    && hir_compat::method(property).name() == "push"
                {
                    for arg in args {
                        self.write(id, AggregateKind::Array, arg);
                        self.expression(arg)?;
                    }
                    return Ok(());
                }
            }
            Expr::ObjectKeys(value) | Expr::ArrayIsArray(value)
                if matches!(**value, Expr::LocalGet(_)) =>
            {
                return Ok(());
            }
            Expr::JsonStringify(value) if matches!(**value, Expr::LocalGet(_)) => return Ok(()),
            Expr::JsonStringifyFull(value, replacer, space)
                if matches!(**value, Expr::LocalGet(_))
                    && matches!(**replacer, Expr::Null | Expr::Undefined) =>
            {
                return self.expression(space);
            }
            Expr::PropertyGet {
                object, property, ..
            } if let Expr::LocalGet(id) = **object => {
                if property != "length"
                    || self.constraints.fresh.get(&self.constraints.root(id))
                        != Some(&AggregateKind::Array)
                {
                    self.constraints.element_reads.insert(id);
                }
                return Ok(());
            }
            Expr::IndexGet { object, index } => {
                if let Expr::LocalGet(id) = **object {
                    self.constraints.element_reads.insert(id);
                    if let Some(kind) = self.key_kind(index) {
                        if self
                            .constraints
                            .fresh
                            .get(&self.constraints.root(id))
                            .is_some_and(|expected| *expected != kind)
                        {
                            self.constraints.escaped.insert(id);
                        }
                    } else {
                        self.constraints.escaped.insert(id);
                    }
                    return self.expression(index);
                }
            }
            Expr::Closure { captures, .. } => {
                self.constraints.escaped.extend(captures.iter().copied());
                let mut references = Vec::new();
                collect_local_refs_expr(expr, &mut references, &mut HashSet::new());
                self.constraints.escaped.extend(references);
                return Ok(());
            }
            _ => {}
        }
        walk::visit_expr(expr, self)
    }

    fn node(&mut self, node: Node<'_>) -> Result<(), Infallible> {
        if let Node::Expression(
            Expr::LocalGet(id)
            | Expr::LocalSet(id, _)
            | Expr::Update { id, .. }
            | Expr::ArrayPushSpread { array_id: id, .. }
            | Expr::ArrayUnshift { array_id: id, .. }
            | Expr::ArraySplice { array_id: id, .. }
            | Expr::ArrayCopyWithin { array_id: id, .. }
            | Expr::ArrayPop(id)
            | Expr::ArrayShift(id)
            | Expr::SetAdd { set_id: id, .. },
        ) = node
        {
            self.constraints.escaped.insert(*id);
        }
        Ok(())
    }
}

pub(super) fn resolve(
    init: &mut [Stmt],
    functions: &mut [perry_hir::ir::Function],
    classes: &mut [perry_hir::ir::Class],
    globals: &[perry_hir::ir::Global],
    base: &mut perry_hir::HirTypeEnv,
    inference: &Inference<'_>,
) -> bool {
    let mut constraints = Constraints::default();
    let mut locals = HashMap::new();
    let mut collect = |body: &mut [Stmt], mut env: FunctionEnv<'_>| {
        walk::scan(body, &mut env, None, None);
        let Ok(()) = walk::visit_body(
            body,
            &mut Collect {
                constraints: &mut constraints,
                env,
            },
        );
    };
    collect(init, inference.function(base, classes, &mut locals));
    for function in &mut *functions {
        collect(
            &mut function.body,
            inference.function_with_params(base, classes, &mut locals, &function.params),
        );
    }
    for class_index in 0..classes.len() {
        base.set_current_class(Some(classes[class_index].name.clone()));
        let mut bodies: Vec<_> = shapes::class_functions_mut(&mut classes[class_index])
            .map(|function| std::mem::take(&mut function.body))
            .collect();
        for (function, body) in shapes::class_functions(&classes[class_index]).zip(&mut bodies) {
            collect(
                body,
                inference.function_with_params(base, classes, &mut locals, &function.params),
            );
        }
        for (function, body) in shapes::class_functions_mut(&mut classes[class_index]).zip(bodies) {
            function.body = body;
        }
    }
    base.set_current_class(None);
    let mut escaped = Vec::new();
    let computed_members = || classes.iter().flat_map(|class| &class.computed_members);
    for expr in globals
        .iter()
        .filter_map(|global| global.init.as_ref())
        .chain(classes.iter().flat_map(|class| {
            class
                .fields
                .iter()
                .chain(&class.static_fields)
                .flat_map(|field| field.init.iter().chain(&field.key_expr))
                .chain(class.extends_expr.as_deref())
                .chain(
                    class
                        .decorators
                        .iter()
                        .flat_map(|decorator| &decorator.args),
                )
        }))
        .chain(computed_members().map(|member| &member.key_expr))
        .chain(
            functions
                .iter()
                .chain(classes.iter().flat_map(shapes::class_functions))
                .chain(computed_members().map(|member| &member.function))
                .flat_map(|function| {
                    function
                        .params
                        .iter()
                        .filter_map(|param| param.default.as_ref())
                }),
        )
    {
        collect_local_refs_expr(expr, &mut escaped, &mut HashSet::new());
    }
    for statement in computed_members().flat_map(|member| &member.function.body) {
        collect_local_refs_stmt(statement, &mut escaped, &mut HashSet::new());
    }
    constraints.escaped.extend(escaped);
    let mut bindings = constraints.bindings();
    let mut changed = false;
    let mut apply = |id: LocalId, ty: &mut HirType| {
        if let Some(inferred) = bindings.remove(&id) {
            *ty = inferred;
            changed = true;
        }
    };
    walk::locals_mut(init, &mut apply);
    for function in &mut *functions {
        walk::locals_mut(&mut function.body, &mut apply);
    }
    for class in &mut *classes {
        for function in shapes::class_functions_mut(class) {
            walk::locals_mut(&mut function.body, &mut apply);
        }
    }
    changed
}
