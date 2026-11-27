use super::*;
use crate::methods::ReplacementArg;

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

    pub(in crate::lower) fn replace_string(
        &mut self,
        ctx: &mut Context,
        text: Value,
        pattern: &'s Expr,
        replacement: &'s Expr,
        all: bool,
    ) -> Result<Value> {
        let search = self.expression(ctx, pattern)?;
        let search = self.defined(ctx, search)?;
        let callback = self.optional_callback(ctx, replacement)?;
        let (intrinsic, regex) = match kind(ctx, search)? {
            Kind::Regex => (
                Intrinsic::RegexReplace,
                (all || callback.is_some()).then(|| self.search_shape(ctx, pattern, search)),
            ),
            Kind::String if all => (Intrinsic::StringReplaceAll, None),
            Kind::String => (Intrinsic::StringReplace, None),
            _ => bail_unsupported!("string replacement requires a string or regex search pattern"),
        };
        let global = regex
            .map_or(Some(all), |shape| shape.global)
            .ok_or_else(|| Diagnostic::unsupported(perry_mir::RegexShape::UNKNOWN_FLAGS))?;
        ensure_unsupported!(
            !all || global,
            "String.prototype.replaceAll called with a non-global RegExp argument"
        );
        let Some(callback) = callback else {
            let replacement = self.string_operand(ctx, replacement)?;
            return Ok(self.intrinsic(ctx, intrinsic, vec![text, search, replacement]));
        };
        self.replace_matches(ctx, text, search, regex, callback, global)
    }

    fn replace_matches(
        &mut self,
        ctx: &mut Context,
        text: Value,
        search: Value,
        regex: Option<perry_mir::RegexShape>,
        callback: crate::lower::callable::Callback<'s>,
        global: bool,
    ) -> Result<Value> {
        let zero = self.number(ctx, 0.0);
        let one = self.number(ctx, 1.0);
        let empty = self.string(ctx, "");
        let (number, string) = (zero.get_type(ctx), empty.get_type(ctx));
        let (head, [from, end, output]) = self.block_with_args(ctx, [number, number, string]);
        let (done, [suffix_start, result]) = self.block_with_args(ctx, [number, string]);
        self.emit(BrOp::new(ctx, head, vec![zero, zero, empty]), ctx);
        self.switch_to(head);
        let (captures, offset, matched) = if let Some(regex) = regex {
            let shape = regex.captures.ok_or_else(|| {
                unsupported!("replacement callbacks require statically known capture arity")
            })?;
            let found = self.intrinsic(ctx, Intrinsic::RegexExecFrom, vec![text, search, from]);
            let missing = self.emit_value(IsNullOp::new(ctx, found), ctx);
            self.yield_if(ctx, done, missing, [end, output]);
            let found = self.defined(ctx, found)?;
            let offset = self.intrinsic(ctx, Intrinsic::RegexIndex, vec![found]);
            let matched = self.element_at(ctx, found, zero)?;
            (Some((found, shape)), offset, self.defined(ctx, matched)?)
        } else {
            // indexOf clamps out-of-range positions, including the position after an empty match.
            let length = self.string_length(ctx, text);
            let exhausted = self.compare(ctx, CompareKind::Gt, from, length);
            self.yield_if(ctx, done, exhausted, [end, output]);
            let index = self.intrinsic(ctx, Intrinsic::StringIndexOf, vec![text, search, from]);
            let missing = self.compare(ctx, CompareKind::Lt, index, zero);
            self.yield_if(ctx, done, missing, [end, output]);
            (None, index, search)
        };
        let length = self.string_length(ctx, matched);
        let match_end = self.binary(ctx, BinaryKind::Add, offset, length);
        let arguments = ReplacementArg::layout(matched, offset, text, captures)
            .map(|arg| match arg {
                ReplacementArg::Value(value) => Ok(value),
                ReplacementArg::Capture(found, index) => {
                    let index = self.number(ctx, index.into());
                    self.element_at(ctx, found, index)
                }
                ReplacementArg::Groups(found) => {
                    Ok(self.intrinsic(ctx, Intrinsic::RegexGroups, vec![found]))
                }
            })
            .collect::<Result<Vec<_>>>()?;
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
