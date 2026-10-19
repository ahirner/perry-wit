use super::*;

impl<'s> FunctionBuilder<'s> {
    pub(crate) fn regex_global(ctx: &Context, regex: Value) -> Result<bool> {
        static_regex(ctx, regex, |op| op.flags(ctx).contains('g')).context(
            "regex flags must be statically known; use a regex literal or a local assigned only literals with the same g flag",
        )
    }

    pub(crate) fn regex_result(ctx: &Context, value: Value) -> bool {
        origin(ctx, value, &mut Vec::new(), &|op| {
            if Operation::get_op::<NullOp>(op, ctx).is_some()
                || Operation::get_op::<UndefinedOp>(op, ctx).is_some()
            {
                return Some(());
            }
            let call = Operation::get_op::<perry_mir::IntrinsicOp>(op, ctx)?;
            match call.intrinsic(ctx) {
                Intrinsic::RegexExec | Intrinsic::RegexExecFrom => Some(()),
                Intrinsic::RegexMatch => Self::regex_global(ctx, op.deref(ctx).get_operand(1))
                    .ok()
                    .filter(|global| !global)
                    .map(|_| ()),
                _ => None,
            }
        })
        .is_ok_and(|res| res.is_some())
    }

    pub(super) fn replace_callback(
        &mut self,
        ctx: &mut Context,
        text: Value,
        search: Value,
        callback: &'s Expr,
        all: bool,
    ) -> Result<Value> {
        let regex = kind(ctx, search)? == Kind::Regex;
        ensure!(
            regex || kind(ctx, search)? == Kind::String,
            "string replacement requires a string or literal regex pattern"
        );
        let (global, captures, named) = if regex {
            let global = Self::regex_global(ctx, search)?;
            ensure!(
                !all || global,
                "String.prototype.replaceAll called with a non-global RegExp argument"
            );
            let (captures, named) = static_regex(ctx, search, |op| op.captures(ctx))
                .flatten()
                .context("replacement callbacks require statically known capture arity")?;
            (global, captures, named)
        } else {
            (all, 0, false)
        };
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        let empty = self.string(ctx, "");
        let cursor = self.entry_slot(ctx, zero.get_type(ctx));
        let end = self.entry_slot(ctx, zero.get_type(ctx));
        let output = self.entry_slot(ctx, empty.get_type(ctx));
        self.store(ctx, cursor, zero);
        self.store(ctx, end, zero);
        self.store(ctx, output, empty);
        let head = self.new_block(ctx, "replacement_match");
        let hit = self.new_block(ctx, "replacement_callback");
        let done = self.new_block(ctx, "replacement_done");
        self.jump(ctx, head);
        self.switch_to(head);
        let from = self.load(ctx, cursor)?;
        let found = if regex {
            self.intrinsic(ctx, Intrinsic::RegexExecFrom, vec![text, search, from])
        } else {
            self.intrinsic(ctx, Intrinsic::StringIndexOf, vec![text, search, from])
        };
        let matched = if regex {
            let missing = self.emit_value(ctx, |ctx| IsNullOp::new(ctx, found));
            self.branch(ctx, missing, done, hit);
            None
        } else {
            // indexOf clamps out-of-range positions; an empty needle must stop after
            // the final boundary instead of matching the end forever.
            let length = self.string_length(ctx, text);
            let exhausted = self.compare(ctx, CompareKind::Gt, from, length);
            let check = self.new_block(ctx, "replacement_found");
            self.branch(ctx, exhausted, done, check);
            self.switch_to(check);
            let missing = self.compare(ctx, CompareKind::Lt, found, zero);
            self.branch(ctx, missing, done, hit);
            Some(search)
        };
        self.switch_to(hit);
        let found = if regex {
            self.defined(ctx, found)?
        } else {
            found
        };
        let offset = if regex {
            self.intrinsic(ctx, Intrinsic::RegexIndex, vec![found])
        } else {
            found
        };
        let matched = if let Some(matched) = matched {
            matched
        } else {
            let op = ArrayGetOp::new(ctx, found, zero)?;
            let value = self.append_value(ctx, &op);
            self.defined(ctx, value)?
        };
        let length = self.string_length(ctx, matched);
        let match_end = self.binary(ctx, BinaryKind::Add, offset, length);
        let mut arguments = Vec::with_capacity(captures + 3 + usize::from(named));
        arguments.push(matched);
        for index in 1..=captures {
            let index = self.number(ctx, index as f64);
            let op = ArrayGetOp::new(ctx, found, index)?;
            arguments.push(self.append_value(ctx, &op));
        }
        arguments.extend([offset, text]);
        if named {
            arguments.push(self.intrinsic(ctx, Intrinsic::RegexGroups, vec![found]));
        }
        let replacement = self.array_callback(ctx, callback, &arguments)?;
        let replacement = self.display(ctx, replacement)?;
        let previous_end = self.load(ctx, end)?;
        let prefix = self.intrinsic(
            ctx,
            Intrinsic::StringSlice,
            vec![text, previous_end, offset],
        );
        let previous = self.load(ctx, output)?;
        let joined = self.emit_value(ctx, |ctx| ConcatOp::new(ctx, previous, prefix));
        let joined = self.emit_value(ctx, |ctx| ConcatOp::new(ctx, joined, replacement));
        self.store(ctx, output, joined);
        self.store(ctx, end, match_end);
        if global {
            let next = if regex {
                self.intrinsic(ctx, Intrinsic::RegexNext, vec![found])
            } else {
                let advance = self.intrinsic(ctx, Intrinsic::MathMax, vec![length, one]);
                self.binary(ctx, BinaryKind::Add, offset, advance)
            };
            self.store(ctx, cursor, next);
            self.jump(ctx, head);
        } else {
            self.jump(ctx, done);
        }
        self.switch_to(done);
        let previous_end = self.load(ctx, end)?;
        let infinity = self.number(ctx, f64::INFINITY);
        let suffix = self.intrinsic(
            ctx,
            Intrinsic::StringSlice,
            vec![text, previous_end, infinity],
        );
        let output = self.load(ctx, output)?;
        Ok(self.emit_value(ctx, |ctx| ConcatOp::new(ctx, output, suffix)))
    }

    fn string_length(&mut self, ctx: &mut Context, string: Value) -> Value {
        let length = self.emit_value(ctx, |ctx| StringLenOp::new(ctx, string));
        self.emit_value(ctx, |ctx| {
            NumericCastOp::new(ctx, length, FP64Type::get(ctx).into())
        })
    }
}

fn static_regex<T: PartialEq>(
    ctx: &Context,
    regex: Value,
    property: impl Fn(perry_mir::RegexConstOp) -> T,
) -> Option<T> {
    origin(ctx, regex, &mut Vec::new(), &|op| {
        Operation::get_op::<perry_mir::RegexConstOp>(op, ctx).map(&property)
    })
    .ok()
    .flatten()
}

fn origin<T: PartialEq>(
    ctx: &Context,
    value: Value,
    seen: &mut Vec<Value>,
    leaf: &impl Fn(Ptr<Operation>) -> Option<T>,
) -> Result<Option<T>, ()> {
    let op = value.defining_op().ok_or(())?;
    if Operation::get_op::<UnwrapOp>(op, ctx).is_some()
        || Operation::get_op::<WrapOp>(op, ctx).is_some()
        || Operation::get_op::<perry_mir::OptionalCastOp>(op, ctx).is_some()
    {
        return origin(ctx, op.deref(ctx).get_operand(0), seen, leaf);
    }
    if Operation::get_op::<LoadOp>(op, ctx).is_none() {
        return leaf(op).map(Some).ok_or(());
    }
    let slot = op.deref(ctx).get_operand(0);
    if seen.contains(&slot) {
        return Ok(None);
    }
    seen.push(slot);
    let mut merged = None;
    let status = slot
        .uses(ctx)
        .iter()
        .map(|u| u.user_op())
        .try_for_each(|user| {
            if Operation::get_op::<LoadOp>(user, ctx).is_some() {
                return Ok(());
            }
            Operation::get_op::<StoreOp>(user, ctx).ok_or(())?;
            let user = user.deref(ctx);
            if user.get_operand(0) != slot {
                return Err(());
            }
            if let Some(stored) = origin(ctx, user.get_operand(1), seen, leaf)? {
                if merged.as_ref().is_some_and(|prev| *prev != stored) {
                    return Err(());
                }
                merged = Some(stored);
            }
            Ok(())
        });
    seen.pop();
    status.map(|()| merged)
}
