use super::*;
use crate::collections::Yield;
use crate::lower::array_methods::Direction;
use crate::lower::builder::insert_before_terminator;

#[derive(Clone, Copy)]
pub(crate) enum Holes {
    Dense,
    Skip,
    Visit(TypeHandle),
    Preserve(Ptr<BasicBlock>),
}
pub(crate) struct Span {
    pub(crate) start: Value,
    pub(crate) limit: Value,
    pub(crate) direction: Direction,
}
pub(crate) enum Input {
    Array(Value, Holes),
    LiveArray(Value),
    String(Value),
    Slice(Value, Span, Holes),
    Collection(Value),
    UndefinedCount(Value),
}
pub(crate) enum ArrayOutput {
    Item(Value),
    Items(Value),
}

impl ArrayOutput {
    pub(crate) fn append_to(
        self,
        builder: &mut FunctionBuilder<'_, '_>,
        ctx: &mut Context,
        array: Value,
    ) {
        match self {
            Self::Item(value) => {
                builder.emit(ArrayPushOp::new(ctx, array, value), ctx);
            }
            Self::Items(items) => {
                builder.emit(perry_mir::ArrayExtendOp::new(ctx, array, items), ctx);
            }
        }
    }
}

impl<'s> FunctionBuilder<'s, '_> {
    pub(crate) fn iterate<T>(
        &mut self,
        ctx: &mut Context,
        input: Input,
        body: impl FnOnce(
            &mut Self,
            &mut Context,
            Yield<Value>,
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
            Input::LiveArray(value) => (self.number(ctx, 0.0), LoopBound::Array(*value)),
            Input::String(value) => (
                self.number(ctx, 0.0),
                LoopBound::Fixed(self.string_length(ctx, *value)),
            ),
            Input::Collection(value) => (self.number(ctx, 0.0), LoopBound::Collection(*value)),
            Input::UndefinedCount(limit) => (self.number(ctx, 0.0), LoopBound::Fixed(*limit)),
        };
        let index = self.entry_slot(ctx, start.get_type(ctx));
        self.store(ctx, index, start);
        let cursor = match input {
            Input::Collection(value) => LiveCursor::open(self, ctx, value, index)?,
            _ => None,
        };
        self.repeat(
            ctx,
            "iterate",
            LoopCondition::Indices(index, bound),
            LoopUpdate::default(),
            cursor.map(|cursor| (index, cursor)),
            |b, ctx, next, done| {
                let mut current = b.load(ctx, index)?;
                let item = match input {
                    Input::Array(value, holes) | Input::Slice(value, _, holes) => {
                        if let Input::Slice(_, ref span, _) = input {
                            current = span.direction.index(b, ctx, span.limit, current);
                        }
                        match holes {
                            Holes::Dense => b.array_value(ctx, value, current)?,
                            Holes::Skip | Holes::Visit(_) | Holes::Preserve(_) => {
                                let present = if b.shapes.specialization.sparse_arrays {
                                    b.emit_value(
                                        perry_mir::ArrayPresentOp::new(ctx, value, current),
                                        ctx,
                                    )
                                } else {
                                    let length = b.array_length(ctx, value);
                                    b.compare(ctx, CompareKind::Lt, current, length)
                                };
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
                                            Ok(b.emit_value(UndefinedOp::with_type(ctx, ty), ctx))
                                        },
                                    )?
                                } else {
                                    let read = b.new_block(ctx, "array_read");
                                    let missing = match holes {
                                        Holes::Preserve(hole) => hole,
                                        _ => next,
                                    };
                                    b.branch(ctx, present, read, missing);
                                    b.switch_to(read);
                                    b.element_at(ctx, value, current)?
                                }
                            }
                        }
                    }
                    Input::LiveArray(value) => b.array_value(ctx, value, current)?,
                    Input::String(value) => {
                        b.intrinsic(ctx, Intrinsic::StringIndex, vec![value, current])
                    }
                    Input::Collection(value) => {
                        let entry = b.visit_collection_entry(ctx, value, current, cursor, next)?;
                        return body(b, ctx, entry, current, next, done);
                    }
                    Input::UndefinedCount(_) => {
                        let ty = void_type(ctx);
                        b.emit_value(UndefinedOp::with_type(ctx, ty), ctx)
                    }
                };
                body(b, ctx, Yield::Item(item), current, next, done)
            },
        )
    }

    /// Return the first matching result and stop traversal, or return `miss` on exhaustion.
    pub(in crate::lower) fn find_item(
        &mut self,
        ctx: &mut Context,
        input: Input,
        miss: impl FnOnce(&mut Self, &mut Context) -> Result<Value>,
        matches: impl FnOnce(&mut Self, &mut Context, Value, Value) -> Result<Value>,
        found: impl FnOnce(&mut Self, &mut Context, Value, Value) -> Result<Value>,
    ) -> Result<Value> {
        let result = self.iterate(ctx, input, |b, ctx, item, index, next, _| {
            let item = item.into_value(b, ctx)?;
            let matched = matches(b, ctx, item, index)?;
            let hit = b.new_block(ctx, "search_hit");
            b.branch(ctx, matched, hit, next);
            b.switch_to(hit);
            let value = found(b, ctx, item, index)?;
            let result = b.result_block(ctx, value.get_type(ctx));
            b.emit(BrOp::new(ctx, result, vec![value]), ctx);
            Ok(result)
        })?;
        let fallback = miss(self, ctx)?;
        Ok(self.finish_result(ctx, result, fallback))
    }

    pub(in crate::lower) fn preserve_holes(&mut self, ctx: &mut Context) -> Holes {
        if self.shapes.specialization.sparse_arrays {
            Holes::Preserve(self.new_block(ctx, "missing_element"))
        } else {
            Holes::Skip
        }
    }

    pub(crate) fn collect_items(
        &mut self,
        ctx: &mut Context,
        input: Input,
        item: impl FnOnce(&mut Self, &mut Context, Value, Value, Ptr<BasicBlock>) -> Result<ArrayOutput>,
    ) -> Result<Value> {
        let prepare = self.block;
        let missing = match input {
            Input::Array(_, Holes::Preserve(hole)) | Input::Slice(_, _, Holes::Preserve(hole)) => {
                Some(hole)
            }
            _ => None,
        };
        self.iterate(ctx, input, |b, ctx, value, index, next, _| {
            let value = value.into_value(b, ctx)?;
            let output = item(b, ctx, value, index, next)?;
            let element = match output {
                ArrayOutput::Item(value) => value.get_type(ctx),
                ArrayOutput::Items(array) => b.array_element(ctx, array)?,
            };
            let allocation = ArrayNewOp::new(ctx, element, vec![]);
            insert_before_terminator(ctx, prepare, [allocation.get_operation()])?;
            let result = allocation.get_result(ctx);
            if let Some(hole) = missing {
                let present = b.block;
                b.switch_to(hole);
                let position = b.array_length(ctx, result);
                b.emit(perry_mir::ArrayHoleOp::new(ctx, result, position), ctx);
                b.jump(ctx, next);
                b.switch_to(present);
            }
            output.append_to(b, ctx, result);
            Ok(result)
        })
    }
}
