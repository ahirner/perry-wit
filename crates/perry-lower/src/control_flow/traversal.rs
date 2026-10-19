use super::*;
use crate::array_methods::Direction;
use crate::builder::insert_before_terminator;
use crate::collections::{
    CollectionViewKind, collection_view_record, map_entry_types, set_entry_type,
};

#[derive(Clone, Copy)]
pub(crate) enum Holes {
    Dense,
    Skip,
    Visit(TypeHandle),
}
pub(crate) struct Span {
    pub(crate) start: Value,
    pub(crate) limit: Value,
    pub(crate) direction: Direction,
}
pub(crate) enum Input {
    Array(Value, Holes),
    Slice(Value, Span, Holes),
    Collection(Value),
    UndefinedCount(Value),
}
pub(crate) enum ArrayOutput {
    Item(Value),
    Items(Value),
}

impl<'s> FunctionBuilder<'s> {
    pub(crate) fn iterate<T>(
        &mut self,
        ctx: &mut Context,
        input: Input,
        body: impl FnOnce(
            &mut Self,
            &mut Context,
            Value,
            Value,
            Ptr<BasicBlock>,
            Ptr<BasicBlock>,
        ) -> Result<T>,
    ) -> Result<T> {
        let (start, bound) = match &input {
            Input::Slice(_, span, _) => (span.start, LoopBound::Fixed(span.limit)),
            Input::Array(array, _) => (
                self.number(ctx, 0.0),
                LoopBound::Fixed(self.array_length(ctx, *array)),
            ),
            Input::Collection(value) => (self.number(ctx, 0.0), LoopBound::Collection(*value)),
            Input::UndefinedCount(limit) => (self.number(ctx, 0.0), LoopBound::Fixed(*limit)),
        };
        let index = self.entry_slot(ctx, start.get_type(ctx));
        self.store(ctx, index, start);
        let walk = match input {
            Input::Collection(collection) => Some(CollectionTraversal {
                index,
                collection,
                cursor: LiveCursor::open(self, ctx, collection, index)?,
            }),
            _ => None,
        };
        self.repeat(
            ctx,
            "iterate",
            LoopCondition::Indices(index, bound),
            None,
            walk,
            |b, ctx, next, done| {
                let mut current = b.load(ctx, index)?;
                let item = match input {
                    Input::Array(value, holes) | Input::Slice(value, _, holes) => {
                        if let Input::Slice(
                            _,
                            Span {
                                limit,
                                direction: Direction::Reverse,
                                ..
                            },
                            _,
                        ) = input
                        {
                            let one = b.number(ctx, 1.0);
                            let last = b.binary(ctx, BinaryKind::Sub, limit, one);
                            current = b.binary(ctx, BinaryKind::Sub, last, current);
                        }
                        match holes {
                            Holes::Dense => b.element_at(ctx, value, current)?,
                            Holes::Skip | Holes::Visit(_) => {
                                let available = b.array_length(ctx, value);
                                let present = b.compare(ctx, CompareKind::Lt, current, available);
                                if let Holes::Visit(ty) = holes {
                                    b.choose(
                                        ctx,
                                        ty,
                                        present,
                                        |b, ctx| {
                                            let item = b.element_at(ctx, value, current)?;
                                            b.fit(ctx, item, ty)
                                        },
                                        |b, ctx| {
                                            Ok(b.emit_value(ctx, |ctx| {
                                                UndefinedOp::with_type(ctx, ty)
                                            }))
                                        },
                                    )?
                                } else {
                                    let read = b.new_block(ctx, "array_read");
                                    b.branch(ctx, present, read, next);
                                    b.switch_to(read);
                                    b.element_at(ctx, value, current)?
                                }
                            }
                        }
                    }
                    Input::Collection(value) => {
                        b.collection_view_element_at(ctx, value, current)?
                    }
                    Input::UndefinedCount(_) => {
                        let ty = void_type(ctx);
                        b.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty))
                    }
                };
                body(b, ctx, item, current, next, done)
            },
        )
    }

    pub(crate) fn collect_items(
        &mut self,
        ctx: &mut Context,
        input: Input,
        item: impl FnOnce(&mut Self, &mut Context, Value, Value, Ptr<BasicBlock>) -> Result<ArrayOutput>,
    ) -> Result<Value> {
        let prepare = self.block;
        self.iterate(ctx, input, |b, ctx, value, index, next, _| {
            let output = item(b, ctx, value, index, next)?;
            let element = match output {
                ArrayOutput::Item(value) => value.get_type(ctx),
                ArrayOutput::Items(array) => b.array_element(ctx, array)?,
            };
            let allocation = ArrayNewOp::new(ctx, element, vec![]);
            insert_before_terminator(ctx, prepare, [allocation.get_operation()])?;
            let result = allocation.get_result(ctx);
            match output {
                ArrayOutput::Item(value) => {
                    b.emit(ctx, |ctx| ArrayPushOp::new(ctx, result, value));
                }
                ArrayOutput::Items(array) => {
                    b.emit(ctx, |ctx| perry_mir::ArrayExtendOp::new(ctx, result, array));
                }
            }
            Ok(result)
        })
    }
}

pub(crate) enum Projection {
    Item(TypeHandle),
    Field((&'static str, TypeHandle)),
    Pair((&'static str, TypeHandle), (&'static str, TypeHandle)),
}

impl Projection {
    pub(crate) fn collection(ctx: &Context, ty: TypeHandle) -> Result<Self> {
        let ty = present_type(ctx, ty);
        if let Ok(array) = TypedHandle::<ArrayType>::from_handle(ty, ctx) {
            return Ok(Self::Item(array.deref(ctx).element()));
        }
        let (view, box_type) = if let Some((view, record)) = collection_view_record(ctx, ty) {
            (
                view,
                record
                    .deref(ctx)
                    .field("_0")
                    .context("collection view missing box")?
                    .1,
            )
        } else if set_entry_type(ctx, ty).is_some() {
            (CollectionViewKind::SetValues, ty)
        } else {
            ensure!(
                map_entry_types(ctx, ty).is_some(),
                "expected an array or collection iterable"
            );
            (CollectionViewKind::MapEntries, ty)
        };
        let record = map_entry_types(ctx, box_type)
            .map(|(_, _, record)| record)
            .or_else(|| set_entry_type(ctx, box_type).map(|(_, record)| record))
            .context("collection view requires a Map or Set box")?;
        let field = |name| {
            record
                .deref(ctx)
                .field(name)
                .map(|(_, ty)| (name, ty))
                .context("collection entry missing field")
        };
        Ok(match view {
            CollectionViewKind::MapKeys | CollectionViewKind::SetValues => {
                Self::Field(field("_0")?)
            }
            CollectionViewKind::MapValues => Self::Field(field("_1")?),
            CollectionViewKind::MapEntries => Self::Pair(field("_0")?, field("_1")?),
            CollectionViewKind::SetEntries => Self::Pair(field("_0")?, field("_0")?),
        })
    }

    pub(crate) fn pair_types(&self, ctx: &Context) -> Option<(TypeHandle, TypeHandle)> {
        let ty = match *self {
            Self::Pair((_, first), (_, second)) => return Some((first, second)),
            Self::Item(ty) | Self::Field((_, ty)) => ty,
        };
        if let Ok(array) = TypedHandle::<ArrayType>::from_handle(ty, ctx) {
            let element = array.deref(ctx).element();
            return Some((element, element));
        }
        let record = TypedHandle::<RecordType>::from_handle(ty, ctx).ok()?;
        let record = record.deref(ctx);
        let fields = record.fields().ok()?;
        (record.is_tuple()
            && (fields.len() == 2
                || (record.kind() == perry_mir::RecordKind::MapEntry && fields.len() == 4)))
            .then_some((record.field("_0")?.1, record.field("_1")?.1))
    }

    pub(crate) fn output_type(&self, ctx: &mut Context, shapes: &Shapes<'_>) -> Result<TypeHandle> {
        Ok(match *self {
            Self::Item(ty) | Self::Field((_, ty)) => ty,
            Self::Pair((_, first), (_, second)) => shapes.pair_type(ctx, first, second)?.into(),
        })
    }

    pub(crate) fn read(
        self,
        b: &mut FunctionBuilder<'_>,
        ctx: &mut Context,
        entry: Value,
    ) -> Result<Value> {
        let field = |b: &mut FunctionBuilder<'_>, ctx: &mut Context, name: &str| {
            let op = FieldGetOp::new(ctx, entry, name.to_owned())?;
            Ok::<_, anyhow::Error>(b.append_value(ctx, &op))
        };
        match self {
            Self::Item(_) => Ok(entry),
            Self::Field((name, _)) => field(b, ctx, name),
            Self::Pair((first, _), (second, _)) => {
                let key = field(b, ctx, first)?;
                let value = if first == second {
                    key
                } else {
                    field(b, ctx, second)?
                };
                b.pair(ctx, key, value)
            }
        }
    }
}
