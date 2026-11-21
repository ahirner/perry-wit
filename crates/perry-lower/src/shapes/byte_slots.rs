//! Element and field storage shared by a narrow aggregate and a mutable outbound view.
use super::*;
use crate::{BytesType, Expr, FuncId, LocalId, Stmt, hir_compat, optional_parts, present_type};
use perry_frontend::hir::tdz_check::{for_each_stmt_shallow, for_each_stmts_expr_shallow};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Binding {
    Local(LocalId),
    Result(FuncId),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Step {
    Field(String),
    Element,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Place(Binding, Vec<Step>);

impl Place {
    fn member(&self, step: Step) -> Self {
        let mut p = self.clone();
        p.1.push(step);
        p
    }

    fn write(expr: &Expr) -> Option<(&Expr, Step, &Expr)> {
        match expr {
            Expr::PropertySet {
                object,
                property,
                value,
                ..
            } => Some((object, Step::Field(property.clone()), value)),
            Expr::IndexSet { object, value, .. } => Some((object, Step::Element, value)),
            Expr::PutValueSet {
                target, key, value, ..
            } => Some((
                target,
                match &**key {
                    Expr::String(field) => Step::Field(field.clone()),
                    _ => Step::Element,
                },
                value,
            )),
            _ => None,
        }
    }

    fn expression(expr: &Expr) -> Option<Self> {
        if let Some(value) = hir_compat::non_null_assertion(expr) {
            return Self::expression(value);
        }
        match expr {
            Expr::LocalGet(id) => Some(Self(Binding::Local(*id), vec![])),
            Expr::Call { callee, .. } if let Expr::FuncRef(id) = **callee => {
                Some(Self(Binding::Result(id), vec![]))
            }
            Expr::PropertyGet {
                object, property, ..
            } => Some(Self::expression(object)?.member(Step::Field(property.clone()))),
            Expr::IndexGet { object, .. } => Some(Self::expression(object)?.member(Step::Element)),
            Expr::Await(value) => Self::expression(value),
            Expr::ArrayPop(id) | Expr::ArrayShift(id) => {
                Some(Self(Binding::Local(*id), vec![Step::Element]))
            }
            Expr::Sequence(values) => Self::expression(values.last()?),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Flow {
    edges: Vec<(Place, Place)>,
    slots: HashMap<Binding, HashSet<Vec<Step>>>,
    tagged: HashSet<Place>,
    types: HashMap<Binding, TypeHandle>,
}

impl Flow {
    fn bind(&mut self, ctx: &Context, binding: Binding, ty: TypeHandle) {
        self.types.insert(binding, ty);
        self.fields(ctx, Place(binding, vec![]), ty, &mut HashSet::new());
    }

    fn fields(
        &mut self,
        ctx: &Context,
        place: Place,
        ty: TypeHandle,
        seen: &mut HashSet<TypeHandle>,
    ) {
        let ty = present_type(ctx, ty);
        if !seen.insert(ty) {
            return;
        }
        let deref = ty.deref(ctx);
        if let Some(array) = deref.downcast_ref::<ArrayType>() {
            self.fields(ctx, place.member(Step::Element), array.element(), seen);
        } else if let Some(rec) = deref.downcast_ref::<RecordType>() {
            if let Ok(fields) = rec.fields() {
                for (name, value) in fields {
                    self.fields(ctx, place.member(Step::Field(name.clone())), *value, seen);
                }
            }
        } else {
            let tagged = perry_mir::is_byte_input(ctx, ty);
            if tagged || deref.is::<BytesType>() || deref.is::<StringType>() {
                if tagged {
                    self.tagged.insert(place.clone());
                }
                self.slots.entry(place.0).or_default().insert(place.1);
            }
        }
        seen.remove(&ty);
    }

    fn shared_slots(mut self) -> HashMap<Binding, Vec<Vec<Step>>> {
        // A scalar assignment copies a value. A shared aggregate exposes writes through either view.
        let mut neighbors: HashMap<Place, Vec<Place>> = HashMap::new();
        for (target, source) in self.edges {
            let (Some(target_slots), Some(source_slots)) =
                (self.slots.get(&target.0), self.slots.get(&source.0))
            else {
                continue;
            };
            for path in target_slots {
                let Some(suffix) = path.strip_prefix(&target.1[..]) else {
                    continue;
                };
                let mut from = source.clone();
                from.1.extend_from_slice(suffix);
                if source_slots.contains(&from.1) {
                    let to = Place(target.0, path.clone());
                    if !suffix.is_empty() {
                        neighbors.entry(to.clone()).or_default().push(from.clone());
                    }
                    neighbors.entry(from).or_default().push(to);
                }
            }
        }
        let mut work: Vec<_> = self.tagged.iter().cloned().collect();
        let mut marked: HashMap<Binding, Vec<Vec<Step>>> = HashMap::new();
        while let Some(place) = work.pop() {
            for reached in neighbors.remove(&place).unwrap_or_default() {
                if self.tagged.insert(reached.clone()) {
                    marked.entry(reached.0).or_default().push(reached.1.clone());
                    work.push(reached);
                }
            }
        }
        marked
    }

    fn assignment(&mut self, shapes: &Shapes<'_>, target: Place, source: &Expr) {
        if let Some(p) = Place::expression(source) {
            self.edges.push((target, p));
        } else if let Some(fields) =
            hir_compat::object_literal_pairs(source, |name| shapes.literal_fields(name))
        {
            for (name, field) in fields {
                self.assignment(shapes, target.member(Step::Field(name.to_owned())), field);
            }
        } else if let Expr::New {
            class_name, args, ..
        } = source
            && let Some((_, class)) = shapes.classes.named.get(class_name.as_str())
            && let Some(ctor) = &class.constructor
        {
            for (param, arg) in ctor.params.iter().zip(args) {
                self.assignment(shapes, Place(Binding::Local(param.id), vec![]), arg);
            }
            for_each_stmts_expr_shallow(&ctor.body, &mut |expr| {
                if let Some((object, step, value)) = Place::write(expr)
                    && matches!(object, Expr::This | Expr::ThisProvenClass(_))
                    && let Some(from) = Place::expression(value)
                {
                    self.edges.push((target.member(step), from));
                }
            });
        } else {
            match source {
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
                    self.assignment(shapes, target.clone(), then_expr);
                    self.assignment(shapes, target, else_expr);
                }
                Expr::Array(elements, _) => {
                    for element in elements {
                        self.assignment(shapes, target.member(Step::Element), element);
                    }
                }
                _ => {}
            }
        }
    }
}

impl Shapes<'_> {
    pub(crate) fn plan_byte_slots(&mut self, ctx: &Context, module: &HirModule) -> Result<()> {
        if !self.metadata.type_references.iter().any(|name| {
            name == "WitInput"
                || matches!(
                    self.decls
                        .imported_types
                        .get(name.as_str())
                        .copied()
                        .or_else(|| self.decls.foreign.get(name)),
                    Some(crate::hir::Foreign::Input(_))
                )
        }) {
            return Ok(());
        }
        let functions: HashMap<_, _> = module
            .functions
            .iter()
            .chain(
                module
                    .classes
                    .iter()
                    .flat_map(perry_frontend::shapes::class_functions),
            )
            .map(|f| (f.id, f))
            .collect();
        let mut flow = Flow::default();
        for (&id, function) in &functions {
            let result = match &function.return_type {
                HirType::Promise(inner) => inner.as_ref(),
                ty => ty,
            };
            if let Ok(ty) = self.value_type(ctx, result) {
                flow.bind(ctx, Binding::Result(id), ty);
            }
            for p in &function.params {
                if let Ok(ty) = self.value_type(ctx, &p.ty) {
                    flow.bind(ctx, Binding::Local(p.id), ty);
                }
            }
        }
        for (result, body) in std::iter::once((None, &module.init))
            .chain(functions.iter().map(|(&id, f)| (Some(id), &f.body)))
        {
            for_each_stmt_shallow(body, &mut |stmt| match stmt {
                Stmt::Let { id, ty, init, .. } => {
                    let owner = Binding::Local(*id);
                    if let Ok(ty) = self.value_type(ctx, ty) {
                        flow.bind(ctx, owner, ty);
                    }
                    if let Some(expr) = init {
                        flow.assignment(self, Place(owner, vec![]), expr);
                    }
                }
                Stmt::Return(Some(expr)) if let Some(id) = result => {
                    flow.assignment(self, Place(Binding::Result(id), vec![]), expr)
                }
                _ => {}
            });
            for_each_stmts_expr_shallow(body, &mut |expr| match expr {
                Expr::LocalSet(id, value) => {
                    flow.assignment(self, Place(Binding::Local(*id), vec![]), value)
                }
                Expr::Call { callee, args, .. }
                    if let Expr::FuncRef(id) = **callee
                        && let Some(f) = functions.get(&id) =>
                {
                    for (param, arg) in f.params.iter().zip(args) {
                        flow.assignment(self, Place(Binding::Local(param.id), vec![]), arg);
                    }
                }
                Expr::ArrayPush {
                    array_id, value, ..
                }
                | Expr::ArrayUnshift { array_id, value } => flow.assignment(
                    self,
                    Place(Binding::Local(*array_id), vec![Step::Element]),
                    value,
                ),
                _ => {
                    if let Some((object, step, value)) = Place::write(expr)
                        && let Some(base) = Place::expression(object)
                    {
                        flow.assignment(self, base.member(step), value);
                    }
                }
            });
        }
        let types = std::mem::take(&mut flow.types);
        let marked = flow.shared_slots();
        for (binding, paths) in marked {
            let paths: Vec<_> = paths.iter().map(Vec::as_slice).collect();
            let ty = self.storage_type(ctx, types[&binding], &paths)?;
            self.byte_slots.insert(binding, ty);
        }
        Ok(())
    }

    fn storage_type(&self, ctx: &Context, ty: TypeHandle, paths: &[&[Step]]) -> Result<TypeHandle> {
        if paths.is_empty() {
            return Ok(ty);
        }
        if let Some((inner, absence)) = optional_parts(ctx, ty) {
            return Ok(
                OptionalType::get_with(ctx, self.storage_type(ctx, inner, paths)?, absence).into(),
            );
        }
        if paths.contains(&&[][..]) {
            return Ok(perry_mir::byte_input(ctx));
        }
        let suffixes = |step: Step| {
            paths
                .iter()
                .filter_map(|path| match path.split_first() {
                    Some((head, tail)) if head == &step => Some(tail),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let value = ty.deref(ctx);
        if let Some(array) = value.downcast_ref::<ArrayType>() {
            return Ok(ArrayType::get(
                ctx,
                self.storage_type(ctx, array.element(), &suffixes(Step::Element))?,
            )
            .into());
        }
        let record = TypedHandle::<RecordType>::from_handle(ty, ctx).to_diag(ctx)?;
        let fields = record
            .deref(ctx)
            .fields()
            .to_diag(ctx)?
            .iter()
            .map(|(name, field)| {
                Ok((
                    name.clone(),
                    self.storage_type(ctx, *field, &suffixes(Step::Field(name.clone())))?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let kind = record.deref(ctx).kind();
        if let Some(class) = self.class_of_type(ctx, ty) {
            let projected = fresh_record(ctx, kind, Some(fields))?.into();
            self.class_records.borrow_mut().insert(projected, class);
            return Ok(projected);
        }
        Ok(self.mir_record_kind(ctx, kind, fields)?.into())
    }
}
