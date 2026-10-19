//! Built-in globals and methods of primitive values: `Math`, `JSON`, errors, strings,
//! bytes, text coding and `console`.
use super::*;
use crate::methods::{Call, Receiver};

/// Accepted deviation: JSON revivers, replacers and indentation are diagnosed at compile time.
pub(super) const JSON_CALLBACKS: &str = "JSON revivers, replacers and indentation are not supported (accepted profile): parse into a typed shape and transform it in code";

impl<'s> FunctionBuilder<'s> {
    /// `object.method(args)` on built-in values.
    pub(super) fn method(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        method: &str,
        args: &'s [Expr],
    ) -> Result<Option<Value>> {
        let method = hir_compat::method(method);
        if let Expr::ClassRef(class_name) = object {
            return self.class_static_method_call(ctx, class_name, method.name(), args);
        }
        if method == hir_compat::Method::Iteration && matches!(object, Expr::UrlSearchParamsNew(_))
        {
            bail!(
                "synchronous for...of requires a dense typed array or string; URLSearchParams iteration is unsupported"
            );
        }
        let receiver = self.expression(ctx, object)?;
        let receiver = self.defined(ctx, receiver)?;
        if method == hir_compat::Method::Iteration {
            ensure!(
                matches!(kind(ctx, receiver)?, Kind::Array | Kind::String)
                    || crate::collections::is_collection_or_view_record(
                        ctx,
                        receiver.get_type(ctx)
                    ),
                "synchronous for...of requires a dense typed array or string; this receiver has no supported iterator"
            );
            return Ok(Some(receiver));
        }
        if let Some((class_name, type_args)) =
            self.shapes.class_of_type(ctx, receiver.get_type(ctx))
            && let Some((owner_class, func)) = self.shapes.class_method(class_name, method.name())
        {
            return self.class_method_call(ctx, receiver, owner_class, &type_args, func, args);
        }
        if method == hir_compat::Method::Push {
            ensure!(
                kind(ctx, receiver)? == Kind::Array,
                "push requires a dense array receiver"
            );
        }
        let method = method.name();
        if let Some(call) = Call::parse(Receiver::of(ctx, receiver)?, method, args)? {
            return call.lower(self, ctx, receiver);
        }
        bail!(
            "method {method} is not supported on {} yet",
            kind(ctx, receiver)?.label()
        )
    }

    pub(super) fn replace_string(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        search: &'s Expr,
        replacement: &'s Expr,
        all: bool,
    ) -> Result<Option<Value>> {
        let method = if all { "replaceAll" } else { "replace" };
        Ok({
            let search_val = self.expression(ctx, search)?;
            let search_val = self.defined(ctx, search_val)?;
            let repl_val = self.string_operand(ctx, replacement)?;
            match kind(ctx, search_val)? {
                Kind::String => {
                    let intrinsic = if all {
                        Intrinsic::StringReplaceAll
                    } else {
                        Intrinsic::StringReplace
                    };
                    self.call_intrinsic(ctx, intrinsic, vec![receiver, search_val, repl_val])
                }
                Kind::Regex => {
                    if all && !Self::regex_global(ctx, search_val)? {
                        bail!(
                            "String.prototype.replaceAll called with a non-global RegExp argument"
                        );
                    }
                    self.call_intrinsic(
                        ctx,
                        Intrinsic::RegexReplace,
                        vec![receiver, search_val, repl_val],
                    )
                }
                _ => bail!("string.{method} requires a string or literal regex search pattern"),
            }
        })
    }

    pub(super) fn regex_test(&mut self, ctx: &mut Context, regex: Value, text: Value) -> Value {
        let index = self.intrinsic(ctx, Intrinsic::RegexSearch, vec![text, regex]);
        let zero = self.number(ctx, 0.0);
        self.emit_value(ctx, |ctx| {
            MirCompare::new(ctx, CompareKind::Ge, index, zero)
        })
    }

    /// Whether `regex` has the `g` flag. Global and non-global regexes behave differently for
    /// every caller, so a regex whose flags are not statically one or the other is rejected.
    pub(super) fn regex_global(ctx: &Context, regex: Value) -> Result<bool> {
        Self::static_regex_global(ctx, regex, &mut Vec::new()).context(
            "regex flags must be statically known; use a regex literal or a local assigned only literals with the same g flag",
        )
    }

    /// The `g` flag shared by every literal `regex` can hold, or `None`. A local qualifies only
    /// while its slot is just loaded and stored to; `seen` holds the slots being resolved, so a
    /// self-assignment is rejected instead of recursing forever.
    fn static_regex_global(ctx: &Context, regex: Value, seen: &mut Vec<Value>) -> Option<bool> {
        let op = regex.defining_op()?;
        if let Some(const_op) = Operation::get_op::<perry_mir::RegexConstOp>(op, ctx) {
            return Some(const_op.flags(ctx).contains('g'));
        }
        Operation::get_op::<perry_mir::LoadOp>(op, ctx)?;
        let slot = op.deref(ctx).get_operand(0);
        if seen.contains(&slot) {
            return None;
        }
        seen.push(slot);
        let mut global = None;
        for user in slot.uses(ctx).iter().map(|u| u.user_op()) {
            if Operation::get_op::<perry_mir::LoadOp>(user, ctx).is_some() {
                continue;
            }
            Operation::get_op::<perry_mir::StoreOp>(user, ctx)?;
            let user = user.deref(ctx);
            if user.get_operand(0) != slot {
                return None;
            }
            let stored = Self::static_regex_global(ctx, user.get_operand(1), seen)?;
            if global.is_some_and(|global| global != stored) {
                return None;
            }
            global = Some(stored);
        }
        global
    }

    /// `JSON.stringify(value)`.
    pub(super) fn stringify(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<Value> {
        let mut value = self.expression(ctx, expr)?;
        if let Some(src) = self.integer64_source(ctx, value) {
            value = src;
        } else if let Expr::LocalGet(id) = expr
            && let Some(int_ty) = self.locals.get(id).and_then(|local| local.integer)
            && int_ty.deref(ctx).width() == 64
        {
            let int_handle: TypeHandle = int_ty.into();
            value = self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, value, int_handle));
        }
        Ok(self.emit_value(ctx, |ctx| JsonStringifyOp::new(ctx, value)))
    }

    /// Construct an Error with a supported scalar or reference cause.
    pub(super) fn error_with_cause(
        &mut self,
        ctx: &mut Context,
        kind: ErrorKind,
        message: &'s Expr,
        cause: &'s Expr,
    ) -> Result<Value> {
        let message = self.string_operand(ctx, message)?;
        let error = self.emit_value(ctx, |ctx| ErrorNewOp::new(ctx, kind, message));
        let cause = self.expression(ctx, cause)?;
        let cause = self.error_cause(ctx, cause)?;
        self.intrinsic_void(ctx, Intrinsic::ExceptionSetCause, vec![error, cause]);
        Ok(error)
    }

    /// `new Error(message, options)` and its subclasses, by HIR error-kind code.
    pub(super) fn error_with_options(
        &mut self,
        ctx: &mut Context,
        error_kind: u32,
        message: &'s Expr,
        options: &'s Expr,
    ) -> Result<Value> {
        let error_class = match error_kind {
            0 => ErrorKind::Any,
            1 => ErrorKind::TypeError,
            2 => ErrorKind::RangeError,
            3 => ErrorKind::ReferenceError,
            4 => ErrorKind::SyntaxError,
            _ => bail!("unsupported Error constructor kind {error_kind}"),
        };
        let message = self.string_operand(ctx, message)?;
        let options = self.expression(ctx, options)?;
        let cause = match kind(ctx, options)? {
            Kind::Record => FieldGetOp::new(ctx, options, "cause".into())
                .ok()
                .map(|op| {
                    self.append(ctx, &op);
                    op.get_result(ctx)
                }),
            Kind::Json => {
                let field = self.string(ctx, "cause");
                Some(self.intrinsic(ctx, Intrinsic::JsonMember, vec![options, field]))
            }
            _ => None,
        };
        let error = self.emit_value(ctx, |ctx| ErrorNewOp::new(ctx, error_class, message));
        if let Some(cause) = cause {
            let cause = self.error_cause(ctx, cause)?;
            self.intrinsic_void(ctx, Intrinsic::ExceptionSetCause, vec![error, cause]);
        }
        Ok(error)
    }

    /// `value instanceof Error` or a supported Error subclass or user-defined class.
    pub(super) fn instance_of(
        &mut self,
        ctx: &mut Context,
        value: &'s Expr,
        ty: &str,
    ) -> Result<Value> {
        let value = self.expression(ctx, value)?;
        if self.shapes.classes.contains_key(ty) {
            let is_inst = self
                .shapes
                .class_of_type(ctx, value.get_type(ctx))
                .is_some_and(|(c, _)| c == ty);
            return Ok(self.boolean(ctx, is_inst));
        }
        let (error, code) = match ty {
            "Error" => (ErrorKind::Any, 0),
            "TypeError" => (ErrorKind::TypeError, 1),
            "RangeError" => (ErrorKind::RangeError, 2),
            "ReferenceError" => (ErrorKind::ReferenceError, 6),
            "SyntaxError" => (ErrorKind::SyntaxError, 7),
            other => bail!("instanceof {other} is not supported yet"),
        };
        Ok(match kind(ctx, value)? {
            Kind::Json => {
                let code = self.i32(ctx, code);
                self.intrinsic(ctx, Intrinsic::JsonExceptionIs, vec![value, code])
            }
            Kind::Exception => self.emit_value(ctx, |ctx| ExceptionIsOp::new(ctx, error, value)),
            _ => self.boolean(ctx, false),
        })
    }

    /// `BigInt(value)` of a number or bigint.
    pub(super) fn bigint_coerce(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<Value> {
        let value = self.expression(ctx, expr)?;
        let value = self.defined(ctx, value)?;
        Ok(match kind(ctx, value)? {
            Kind::BigInt => value,
            Kind::Number => self.intrinsic(ctx, Intrinsic::BigIntFromNumber, vec![value]),
            _ => bail!(
                "BigInt conversion requires a number or bigint in the 64-bit transport profile"
            ),
        })
    }

    /// `Number(value)`.
    pub(super) fn number_coerce(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<Value> {
        let value = self.expression(ctx, expr)?;
        let value = self.arithmetic(ctx, value);
        ensure!(
            kind(ctx, value)? != Kind::NumberString,
            "narrow number | string before numeric conversion"
        );
        Ok(match kind(ctx, value)? {
            Kind::BigInt => self.intrinsic(ctx, Intrinsic::BigIntToNumber, vec![value]),
            Kind::String => self.intrinsic(ctx, Intrinsic::StringToNumber, vec![value]),
            Kind::Number => value,
            _ => bail!("Number conversion requires a number, bigint or string"),
        })
    }

    /// A one-argument `Math` function such as `Math.floor`, lowered as `intrinsic`.
    pub(super) fn math_unary(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        value: &'s Expr,
    ) -> Result<Value> {
        let value = self.number_operand(ctx, value)?;
        Ok(self.intrinsic(ctx, intrinsic, vec![value]))
    }

    /// `Math.min(…)` or `Math.max(…)`: `intrinsic` folded over `values` from `empty`.
    pub(super) fn math_extremum(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        empty: f64,
        values: &'s [Expr],
    ) -> Result<Value> {
        let mut result = self.number(ctx, empty);
        for value in values {
            let value = self.number_operand(ctx, value)?;
            result = self.intrinsic(ctx, intrinsic, vec![result, value]);
        }
        Ok(result)
    }

    /// `Math.min(...array)` or `Math.max(...array)`.
    pub(super) fn math_extremum_spread(
        &mut self,
        ctx: &mut Context,
        intrinsic: Intrinsic,
        empty: f64,
        array: &'s Expr,
    ) -> Result<Value> {
        let arr = self.expression(ctx, array)?;
        let arr = self.defined(ctx, arr)?;
        ensure!(
            kind(ctx, arr)? == Kind::Array,
            "Math.max/min spread requires an array"
        );
        let init = self.number(ctx, empty);
        let acc_slot = self.entry_slot(ctx, FP64Type::get(ctx).into());
        self.store(ctx, acc_slot, init);
        let len = self.array_length(ctx, arr);
        let zero = self.number(ctx, 0.0);
        self.index_loop(ctx, "math_spread", zero, len, |b, ctx, idx, _, _| {
            let item = b.element_at(ctx, arr, idx)?;
            let val = b.arithmetic(ctx, item);
            let cur = b.load(ctx, acc_slot)?;
            let next = b.intrinsic(ctx, intrinsic, vec![cur, val]);
            b.store(ctx, acc_slot, next);
            Ok(())
        })?;
        self.load(ctx, acc_slot)
    }

    /// `new Uint8Array(source)` or `Uint8Array.from(source)`.
    pub(super) fn bytes_from(
        &mut self,
        ctx: &mut Context,
        creation: BytesCreation,
        source: &'s Expr,
    ) -> Result<Value> {
        let number = FP64Type::get(ctx).into();
        let numbers = ArrayType::get(ctx, number).into();
        let expected = matches!(source, Expr::Array(_)).then_some(numbers);
        let value = self.expression_with(ctx, source, expected)?;
        let value = self.defined(ctx, value)?;
        let value = self.arithmetic(ctx, value);
        Ok(match kind(ctx, value)? {
            Kind::Number if matches!(creation, BytesCreation::Construct) => {
                self.intrinsic(ctx, Intrinsic::BytesNew, vec![value])
            }
            Kind::Array => {
                ensure!(
                    value.get_type(ctx) == numbers,
                    "Uint8Array sources must be number arrays"
                );
                self.intrinsic(ctx, Intrinsic::BytesFromArray, vec![value])
            }
            Kind::ByteBuffer => self.intrinsic(ctx, Intrinsic::BytesFromBuffer, vec![value]),
            Kind::Bytes => self.intrinsic(ctx, Intrinsic::BytesCopy, vec![value]),
            _ => bail!("new Uint8Array takes a length, a number[] or a Uint8Array"),
        })
    }

    /// `new TextDecoder(label?, { fatal?, ignoreBOM? })`, UTF-8 only.
    pub(super) fn text_decoder(
        &mut self,
        ctx: &mut Context,
        label: &'s Expr,
        fatal: &'s Expr,
        ignore_bom: &'s Expr,
    ) -> Result<Value> {
        let label = if matches!(label, Expr::Undefined) {
            None
        } else {
            Some(self.string_operand(ctx, label)?)
        };
        let fatal = match fatal {
            Expr::Undefined => self.boolean(ctx, false),
            fatal => self.condition(ctx, fatal)?,
        };
        let ignore_bom = match ignore_bom {
            Expr::Undefined => self.boolean(ctx, false),
            ignore_bom => self.condition(ctx, ignore_bom)?,
        };
        if let Some(label) = label {
            self.intrinsic_void(ctx, Intrinsic::DecoderValidateLabel, vec![label]);
        }
        Ok(self.intrinsic(ctx, Intrinsic::DecoderNew, vec![fatal, ignore_bom]))
    }

    /// `console.log(a, b, …)`: the arguments' strings joined by spaces.
    pub(super) fn console_line(&mut self, ctx: &mut Context, args: &'s [Expr]) -> Result<Value> {
        let mut line: Option<Value> = None;
        for arg in args {
            let value = self.expression(ctx, arg)?;
            let text = self.display_as(ctx, value, crate::coerce::DisplayMode::Console)?;
            line = Some(match line {
                None => text,
                Some(line) => {
                    let space = self.string(ctx, " ");
                    let spaced = self.emit_value(ctx, |ctx| ConcatOp::new(ctx, line, space));
                    self.emit_value(ctx, |ctx| ConcatOp::new(ctx, spaced, text))
                }
            });
        }
        Ok(match line {
            Some(line) => line,
            None => self.string(ctx, ""),
        })
    }
}

/// How a `Uint8Array` is created from a source: only the constructor also accepts a length.
pub(super) enum BytesCreation {
    Construct,
    From,
}
