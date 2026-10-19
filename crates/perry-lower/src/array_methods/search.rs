use super::*;

pub(crate) enum Search<'s> {
    Index(&'s Expr, Option<&'s Expr>, Direction),
    Includes(&'s Expr, Option<&'s Expr>),
    Find(&'s Expr, FindResult, Direction),
    Quantifier(&'s Expr, Quantifier),
}

enum Predicate<'s> {
    Equal(Value, bool),
    Callback(&'s Expr),
}

enum Outcome {
    Index,
    Item,
    Boolean(bool),
}

impl<'s> FunctionBuilder<'s> {
    pub(crate) fn array_search(
        &mut self,
        ctx: &mut Context,
        array: Value,
        search: Search<'s>,
    ) -> Result<Value> {
        let array = self.defined(ctx, array)?;
        let element = self.array_element(ctx, array)?;
        let optional = crate::coerce::admitting(ctx, element, perry_mir::Absence::Undefined);
        let (predicate, direction, holes, outcome, from) = match search {
            Search::Index(value, from, _) | Search::Includes(value, from) => {
                let includes = matches!(search, Search::Includes(..));
                let direction = match search {
                    Search::Index(_, _, direction) => direction,
                    _ => Direction::Forward,
                };
                let needle = self.expression_with(ctx, value, Some(element))?;
                let needle = self.fit(ctx, needle, element)?;
                let from = from
                    .map(|from| self.number_or(ctx, Some(from), 0.0))
                    .transpose()?;
                (
                    Predicate::Equal(needle, includes),
                    direction,
                    Holes::Dense,
                    if includes {
                        Outcome::Boolean(true)
                    } else {
                        Outcome::Index
                    },
                    from,
                )
            }
            Search::Find(callback, result, direction) => (
                Predicate::Callback(callback),
                direction,
                Holes::Visit(optional),
                match result {
                    FindResult::Index => Outcome::Index,
                    FindResult::Value => Outcome::Item,
                },
                None,
            ),
            Search::Quantifier(callback, quantifier) => (
                Predicate::Callback(callback),
                Direction::Forward,
                Holes::Skip,
                Outcome::Boolean(matches!(quantifier, Quantifier::Some)),
                None,
            ),
        };
        let zero = self.number(ctx, 0.0);
        let len = self.array_length(ctx, array);
        let (start, limit) = if let Some(from) = from {
            let number = FP64Type::get(ctx).into();
            let from = self.integer_or_infinity(ctx, from)?;
            let negative = self.compare(ctx, CompareKind::Lt, from, zero);
            let from = self.choose(
                ctx,
                number,
                negative,
                |b, ctx| Ok(b.binary(ctx, BinaryKind::Add, len, from)),
                |_, _| Ok(from),
            )?;
            match direction {
                Direction::Forward => (
                    self.intrinsic(ctx, Intrinsic::MathMax, vec![from, zero]),
                    len,
                ),
                Direction::Reverse => {
                    let one = self.number(ctx, 1.0);
                    let count = self.binary(ctx, BinaryKind::Add, from, one);
                    let count = self.intrinsic(ctx, Intrinsic::MathMin, vec![count, len]);
                    (
                        zero,
                        self.intrinsic(ctx, Intrinsic::MathMax, vec![count, zero]),
                    )
                }
            }
        } else {
            (zero, len)
        };
        let miss = match outcome {
            Outcome::Index => self.number(ctx, -1.0),
            Outcome::Item => self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, optional)),
            Outcome::Boolean(hit) => self.boolean(ctx, !hit),
        };
        let result_type = miss.get_type(ctx);
        let result = self.entry_slot(ctx, result_type);
        self.store(ctx, result, miss);
        self.iterate(
            ctx,
            Input::Slice(
                array,
                Span {
                    start,
                    limit,
                    direction,
                },
                holes,
            ),
            |b, ctx, item, current, next, done| {
                let matched = match predicate {
                    Predicate::Equal(needle, same_value_zero) => {
                        if same_value_zero {
                            b.same_value_zero(ctx, item, needle, element)?
                        } else {
                            b.compare(ctx, CompareKind::Eq, item, needle)
                        }
                    }
                    Predicate::Callback(callback) => {
                        let value = b.array_callback(ctx, callback, &[item, current, array])?;
                        b.truthy(ctx, value)?
                    }
                };
                let hit = b.new_block(ctx, "search_hit");
                if matches!(outcome, Outcome::Boolean(false)) {
                    b.branch(ctx, matched, next, hit);
                } else {
                    b.branch(ctx, matched, hit, next);
                }
                b.switch_to(hit);
                let value = match outcome {
                    Outcome::Index => current,
                    Outcome::Item => b.fit(ctx, item, result_type)?,
                    Outcome::Boolean(value) => b.boolean(ctx, value),
                };
                b.store(ctx, result, value);
                b.jump(ctx, done);
                Ok(())
            },
        )?;
        self.load(ctx, result)
    }
}
