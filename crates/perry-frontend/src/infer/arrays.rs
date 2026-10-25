use super::*;
use perry_hir::analysis::collect_local_refs_expr;
use std::convert::Infallible;
use walk::{Node, Visitor};

#[derive(Default)]
struct Constraints {
    fresh: HashSet<LocalId>,
    aliases: HashMap<LocalId, LocalId>,
    writes: HashMap<LocalId, Vec<HirType>>,
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
        let mut elements: HashMap<LocalId, Option<HirType>> = HashMap::new();
        for (id, writes) in &self.writes {
            let root = self.root(*id);
            if !self.fresh.contains(&root) || escaped.contains(&root) {
                continue;
            }
            for ty in writes {
                elements
                    .entry(root)
                    .and_modify(|element| {
                        if element.as_ref() != Some(ty) {
                            *element = None;
                        }
                    })
                    .or_insert_with(|| is_resolvable_type(ty).then(|| ty.clone()));
            }
        }
        for id in &self.element_reads {
            if let Some(element) = elements.get_mut(&self.root(*id))
                && !matches!(
                    element,
                    Some(HirType::Number | HirType::String | HirType::Boolean | HirType::BigInt)
                )
            {
                *element = None;
            }
        }
        self.fresh
            .iter()
            .chain(self.aliases.keys())
            .filter_map(|id| {
                let ty = elements.get(&self.root(*id))?.as_ref()?;
                Some((*id, HirType::Array(Box::new(ty.clone()))))
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
        _ => {}
    });
    ty
}

struct Collect<'a, 'e> {
    constraints: &'a mut Constraints,
    env: FunctionEnv<'e>,
}

impl Collect<'_, '_> {
    fn write(&mut self, id: LocalId, value: &Expr) {
        self.constraints
            .writes
            .entry(id)
            .or_default()
            .push(element_type(value, &mut self.env));
    }
}

impl Visitor for Collect<'_, '_> {
    type Error = Infallible;

    fn statement(&mut self, statement: &mut Stmt) -> Result<(), Infallible> {
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
            && unresolved_array(ty)
        {
            match init {
                Expr::Array(values, _) if values.is_empty() => {
                    self.constraints.fresh.insert(*id);
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
                self.write(*array_id, value);
                return self.expression(value);
            }
            Expr::IndexSet {
                object,
                index,
                value,
            } if matches!(**object, Expr::LocalGet(_)) => {
                let Expr::LocalGet(id) = **object else {
                    unreachable!()
                };
                self.write(id, value);
                self.expression(index)?;
                return self.expression(value);
            }
            Expr::PutValueSet {
                target,
                key,
                value,
                receiver,
                ..
            } if matches!((&**target, &**receiver), (Expr::LocalGet(a), Expr::LocalGet(b)) if a == b)
                && matches!(
                    expression(key, &mut self.env),
                    HirType::Number | HirType::Int32
                ) =>
            {
                let Expr::LocalGet(id) = **target else {
                    unreachable!()
                };
                self.write(id, value);
                self.expression(key)?;
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
                        self.write(id, arg);
                        self.expression(arg)?;
                    }
                    return Ok(());
                }
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
            } if property == "length" && matches!(**object, Expr::LocalGet(_)) => {
                return Ok(());
            }
            Expr::IndexGet { object, index } if matches!(**object, Expr::LocalGet(_)) => {
                let Expr::LocalGet(id) = **object else {
                    unreachable!()
                };
                self.constraints.element_reads.insert(id);
                return self.expression(index);
            }
            Expr::Closure { captures, .. } => {
                let mut references = Vec::new();
                references.extend(captures.iter().copied());
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
    mut base: perry_hir::HirTypeEnv,
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
    collect(init, inference.function(&base, classes, &mut locals));
    for function in &mut *functions {
        collect(
            &mut function.body,
            inference.function_with_params(&base, classes, &mut locals, &function.params),
        );
    }
    for class_index in 0..classes.len() {
        base.set_current_class(Some(classes[class_index].name.clone()));
        for index in 0..shapes::class_functions(&classes[class_index]).count() {
            let mut body = std::mem::take(
                &mut shapes::class_functions_mut(&mut classes[class_index])
                    .nth(index)
                    .unwrap()
                    .body,
            );
            let function = shapes::class_functions(&classes[class_index])
                .nth(index)
                .unwrap();
            collect(
                &mut body,
                inference.function_with_params(&base, classes, &mut locals, &function.params),
            );
            shapes::class_functions_mut(&mut classes[class_index])
                .nth(index)
                .unwrap()
                .body = body;
        }
    }
    let mut escaped = Vec::new();
    for init in globals
        .iter()
        .filter_map(|global| global.init.as_ref())
        .chain(
            classes
                .iter()
                .flat_map(|class| class.fields.iter().filter_map(|field| field.init.as_ref())),
        )
        .chain(classes.iter().flat_map(|class| {
            class
                .static_fields
                .iter()
                .filter_map(|field| field.init.as_ref())
        }))
        .chain(
            functions
                .iter()
                .chain(classes.iter().flat_map(shapes::class_functions))
                .flat_map(|function| {
                    function
                        .params
                        .iter()
                        .filter_map(|param| param.default.as_ref())
                }),
        )
    {
        collect_local_refs_expr(init, &mut escaped, &mut HashSet::new());
    }
    constraints.escaped.extend(escaped);
    let bindings = constraints.bindings();
    let changed = !bindings.is_empty();
    let mut apply = |id: LocalId, ty: &mut HirType| {
        if let Some(inferred) = bindings.get(&id) {
            *ty = inferred.clone();
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
