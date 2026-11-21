//! Propagate the source tag needed by a writable string/byte alias through MIR storage.
use super::super::*;
use crate::{ArrayType, RecordType, SlotType, effects::Mutation};
use std::collections::HashSet;

#[derive(Clone, Copy)]
struct End {
    value: Value,
    slot: Option<usize>,
}

impl From<Value> for End {
    fn from(value: Value) -> Self {
        Self { value, slot: None }
    }
}

impl End {
    fn field(value: Value, slot: usize) -> Self {
        Self {
            value,
            slot: Some(slot),
        }
    }
}

#[derive(Clone, Copy)]
struct Edge {
    take: Option<usize>,
    destination: End,
    backward: bool,
}

struct Leaf {
    path: Vec<usize>,
    tagged: bool,
}

#[derive(Default)]
pub struct SourceSlots {
    layouts: HashMap<TypeHandle, Vec<Leaf>>,
    needed: HashSet<(Value, usize)>,
    returns: HashMap<Identifier, Vec<Value>>,
}

fn shape(ctx: &Context, mut ty: TypeHandle) -> TypeHandle {
    loop {
        let t = ty.deref(ctx);
        if let Some(slot) = t.downcast_ref::<SlotType>() {
            ty = slot.value();
        } else if let Some(promise) = t.downcast_ref::<PromiseType>() {
            ty = promise.value();
        } else if let Some((inner, _)) = crate::optional_parts(ctx, ty) {
            ty = inner;
        } else {
            return ty;
        }
    }
}

impl SourceSlots {
    pub fn paths(&self, ctx: &Context, value: Value) -> impl Iterator<Item = &[usize]> {
        self.layouts
            .get(&value.get_type(ctx))
            .into_iter()
            .flatten()
            .enumerate()
            .filter(move |(index, leaf)| !leaf.tagged && self.needed.contains(&(value, *index)))
            .map(|(_, leaf)| leaf.path.as_slice())
    }

    pub fn returned(&self, name: &Identifier) -> impl Iterator<Item = Value> + '_ {
        self.returns.get(name).into_iter().flatten().copied()
    }

    pub fn solve(ctx: &Context, module: Ptr<Operation>, views: &[Value]) -> Result<Self> {
        fn leaves(
            ctx: &Context,
            ty: TypeHandle,
            path: &mut Vec<usize>,
            visiting: &mut HashSet<TypeHandle>,
            out: &mut Vec<Leaf>,
        ) -> Result<()> {
            let ty = shape(ctx, ty);
            if !visiting.insert(ty) {
                return Ok(());
            }
            let tagged = crate::is_byte_input(ctx, ty);
            if tagged
                || ty.deref(ctx).is::<crate::StringType>()
                || ty.deref(ctx).is::<crate::BytesType>()
            {
                out.push(Leaf {
                    path: path.clone(),
                    tagged,
                });
            } else if let Some(array) = ty.deref(ctx).downcast_ref::<ArrayType>() {
                path.push(0);
                leaves(ctx, array.element(), path, visiting, out)?;
                path.pop();
            } else if let Some(record) = ty.deref(ctx).downcast_ref::<RecordType>() {
                for (i, (_, field)) in record.fields()?.iter().enumerate() {
                    path.push(i);
                    leaves(ctx, *field, path, visiting, out)?;
                    path.pop();
                }
            }
            visiting.remove(&ty);
            Ok(())
        }

        let functions = crate::aggregates::functions(module, ctx);
        let mut out = Self::default();
        let mut params = HashMap::new();
        let mut edges: HashMap<Value, Vec<Edge>> = HashMap::new();
        let mut connect = |a: End, b: End| {
            edges.entry(a.value).or_default().push(Edge {
                take: a.slot,
                destination: b,
                backward: false,
            });
            edges.entry(b.value).or_default().push(Edge {
                take: b.slot,
                destination: a,
                backward: true,
            });
        };
        let mut calls = Vec::new();
        let mut globals = HashMap::new();
        for function in functions {
            let name = function.get_symbol_name(ctx);
            params.insert(
                name.clone(),
                function
                    .get_entry_block(ctx)
                    .deref(ctx)
                    .arguments()
                    .collect::<Vec<_>>(),
            );
            for op in crate::aggregates::operations(ctx, &function) {
                let ir = op.deref(ctx);
                let arg = |i| ir.get_operand(i);
                let result = || ir.get_result(0);
                let kind = OperationKind::classify(ctx, op);
                if let Some(i) = kind.aliased_operand() {
                    connect(arg(i).into(), result().into());
                } else if let Some(global) = Operation::get_op::<crate::GlobalOp>(op, ctx) {
                    if let Some(other) = globals.insert(global.name(ctx), result()) {
                        connect(other.into(), result().into());
                        connect(result().into(), other.into());
                    }
                } else if crate::is_any::<(crate::LoadOp, crate::GlobalLoadOp)>(ctx, op) {
                    connect(arg(0).into(), result().into());
                } else {
                    match kind {
                        OperationKind::Allocate { array } => {
                            for (i, v) in ir.operands().enumerate() {
                                connect(v.into(), End::field(result(), if array { 0 } else { i }));
                            }
                        }
                        OperationKind::Read { slot } => {
                            connect(End::field(arg(0), slot), result().into())
                        }
                        OperationKind::Mutate(Mutation::Local) => {
                            connect(arg(1).into(), arg(0).into())
                        }
                        OperationKind::Mutate(Mutation::Extend) => {
                            connect(End::field(arg(1), 0), End::field(arg(0), 0))
                        }
                        OperationKind::Mutate(mutation)
                            if let Some(i) = mutation.stored_operand()
                                && let Some(slot) = kind.storage_slot() =>
                        {
                            connect(arg(i).into(), End::field(arg(0), slot));
                        }
                        OperationKind::Concat { depth: 1 } => {
                            for v in ir.operands() {
                                connect(End::field(v, 0), End::field(result(), 0));
                            }
                        }
                        OperationKind::Return => out
                            .returns
                            .entry(name.clone())
                            .or_default()
                            .extend(ir.operands()),
                        OperationKind::Call { .. } => {
                            if let Some(callee) = crate::calls::callee(ctx, op) {
                                calls.push((op, callee));
                            }
                        }
                        _ => {}
                    }
                }
                branch_arguments(ctx, op, |from, to| connect(from.into(), to.into()));
            }
        }
        for (op, callee) in calls {
            let ir = op.deref(ctx);
            if let Some(args) = params.get(&callee) {
                for (from, &to) in ir.operands().zip(args) {
                    connect(from.into(), to.into());
                }
            }
            if let Some(result) = ir.results().next() {
                for ret in out.returned(&callee) {
                    connect(ret.into(), result.into());
                }
            }
        }
        for &value in edges.keys().chain(views) {
            let ty = value.get_type(ctx);
            if let std::collections::hash_map::Entry::Vacant(entry) = out.layouts.entry(ty) {
                leaves(
                    ctx,
                    ty,
                    &mut Vec::new(),
                    &mut HashSet::new(),
                    entry.insert(Vec::new()),
                )?;
            }
        }
        let fields = |value: Value| out.layouts[&value.get_type(ctx)].as_slice();
        let mut queue = Vec::new();
        for &view in views {
            queue.extend(
                fields(view)
                    .iter()
                    .enumerate()
                    .filter(|(_, leaf)| leaf.tagged)
                    .map(|(i, _)| (view, i)),
            );
        }
        while let Some((value, i)) = queue.pop() {
            if !out.needed.insert((value, i)) {
                continue;
            }
            for edge in edges.get(&value).into_iter().flatten() {
                let Some(tail) = strip_field(&fields(value)[i].path, edge.take) else {
                    continue;
                };
                // Aggregate references share their slots. Scalar stores copy the value.
                if edge.backward && tail.is_empty() {
                    continue;
                }
                let destination = edge.destination;
                if let Some(other) = fields(destination.value)
                    .iter()
                    .position(|leaf| strip_field(&leaf.path, destination.slot) == Some(tail))
                {
                    queue.push((destination.value, other));
                }
            }
        }
        Ok(out)
    }
}

fn strip_field(path: &[usize], field: Option<usize>) -> Option<&[usize]> {
    match field {
        Some(field) => path.strip_prefix(&[field]),
        None => Some(path),
    }
}
