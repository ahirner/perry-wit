use super::*;

impl<'s> FunctionBuilder<'s, '_> {
    pub(in crate::lower) fn search_shape(
        &self,
        ctx: &Context,
        source: &Expr,
        search: Value,
    ) -> perry_mir::RegexShape {
        match self.site(source).operand {
            Some(crate::engine::solve::OperandUse::RegexSearch(shape)) => shape,
            _ => perry_mir::RegexShape::of(ctx, search),
        }
    }

    pub(super) fn replace_callback(
        &mut self,
        ctx: &mut Context,
        text: Value,
        (source, search): (&'s Expr, Value),
        callback: crate::lower::callable::Callback<'s>,
        all: bool,
    ) -> Result<Value> {
        let regex = kind(ctx, search)? == Kind::Regex;
        ensure_unsupported!(
            regex || kind(ctx, search)? == Kind::String,
            "string replacement requires a string or literal regex pattern"
        );
        let (global, captures, named) = if regex {
            let shape = self.search_shape(ctx, source, search);
            let global = global_flag(shape)?;
            ensure_unsupported!(
                !all || global,
                "String.prototype.replaceAll called with a non-global RegExp argument"
            );
            let (captures, named) = shape.captures.ok_or_else(|| {
                unsupported!("replacement callbacks require statically known capture arity")
            })?;
            (global, captures as usize, named)
        } else {
            (all, 0, false)
        };
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        let empty = self.string(ctx, "");
        let (number, string) = (zero.get_type(ctx), empty.get_type(ctx));
        let (head, [from, end, output]) = self.block_with_args(ctx, [number, number, string]);
        let (done, [suffix_start, result]) = self.block_with_args(ctx, [number, string]);
        self.emit(BrOp::new(ctx, head, vec![zero, zero, empty]), ctx);
        self.switch_to(head);
        let (found, offset, matched) = if regex {
            let found = self.intrinsic(ctx, Intrinsic::RegexExecFrom, vec![text, search, from]);
            let missing = self.emit_value(IsNullOp::new(ctx, found), ctx);
            self.yield_if(ctx, done, missing, [end, output]);
            let found = self.defined(ctx, found)?;
            let offset = self.intrinsic(ctx, Intrinsic::RegexIndex, vec![found]);
            let matched = self.element_at(ctx, found, zero)?;
            (found, offset, self.defined(ctx, matched)?)
        } else {
            // indexOf clamps out-of-range positions, including the position after an empty match.
            let length = self.string_length(ctx, text);
            let exhausted = self.compare(ctx, CompareKind::Gt, from, length);
            self.yield_if(ctx, done, exhausted, [end, output]);
            let index = self.intrinsic(ctx, Intrinsic::StringIndexOf, vec![text, search, from]);
            let missing = self.compare(ctx, CompareKind::Lt, index, zero);
            self.yield_if(ctx, done, missing, [end, output]);
            (index, index, search)
        };
        let length = self.string_length(ctx, matched);
        let match_end = self.binary(ctx, BinaryKind::Add, offset, length);
        let mut arguments = Vec::with_capacity(captures + 3 + usize::from(named));
        arguments.push(matched);
        for index in 1..=captures {
            let index = self.number(ctx, index as f64);
            arguments.push(self.element_at(ctx, found, index)?);
        }
        arguments.extend([offset, text]);
        if named {
            arguments.push(self.intrinsic(ctx, Intrinsic::RegexGroups, vec![found]));
        }
        let replacement = self.array_callback(ctx, callback, &arguments)?;
        let replacement = self.display(ctx, replacement)?;
        let prefix = self.intrinsic(ctx, Intrinsic::StringSlice, vec![text, end, offset]);
        let joined = self.emit_value(ConcatOp::new(ctx, output, prefix), ctx);
        let joined = self.emit_value(ConcatOp::new(ctx, joined, replacement), ctx);
        if global {
            let advance = self.intrinsic(ctx, Intrinsic::MathMax, vec![length, one]);
            let next = self.binary(ctx, BinaryKind::Add, offset, advance);
            self.emit(BrOp::new(ctx, head, vec![next, match_end, joined]), ctx);
        } else {
            self.emit(BrOp::new(ctx, done, vec![match_end, joined]), ctx);
        }
        self.switch_to(done);
        let infinity = self.number(ctx, f64::INFINITY);
        let suffix = self.intrinsic(
            ctx,
            Intrinsic::StringSlice,
            vec![text, suffix_start, infinity],
        );
        Ok(self.emit_value(ConcatOp::new(ctx, result, suffix), ctx))
    }
}

pub(super) fn global_flag(shape: perry_mir::RegexShape) -> Result<bool> {
    shape
        .global
        .ok_or_else(|| Diagnostic::unsupported(perry_mir::RegexShape::UNKNOWN_FLAGS))
}
