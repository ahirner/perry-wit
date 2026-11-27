//! Built-in globals and methods of primitive values: `Math`, `JSON`, errors, strings,
//! bytes, text coding and `console`.
use super::*;
use crate::lower::methods::{Call, Receiver};
mod regex;

impl<'s> FunctionBuilder<'s, '_> {
    /// `object.method(args)` on built-in values.
    pub(super) fn method(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        property: Option<&str>,
        args: &'s [Expr],
    ) -> Result<Option<Value>> {
        let method = property.map(hir_compat::method);
        if let Some(method) = method
            && let Expr::ClassRef(class_name) = object
        {
            bail_diag!(
                codes::TYPE_MISMATCH,
                "static method {class_name}.{} not found",
                method.name()
            );
        }
        let receiver = self.expression(ctx, object)?;
        let receiver = self.defined(ctx, receiver)?;
        let resource = perry_mir::typed::<perry_mir::ResourceType>(ctx, receiver.get_type(ctx));
        if let Some(member) = resource.and_then(|ty| {
            self.shapes
                .specialization
                .receiver_members
                .get(object, ty.deref(ctx).key())
        }) {
            return self.resource_call(ctx, member, Some(receiver), args);
        }
        let method = method.ok_or_else(|| {
            unsupported!("computed method or element calls are not supported yet")
        })?;
        if method == hir_compat::Method::Push {
            ensure_unsupported!(
                kind(ctx, receiver)? == Kind::Array,
                "push requires a dense array receiver"
            );
        }
        let method = method.name();
        if let Some(call) = Call::member(Receiver::of(ctx, receiver)?, method, Some(args))? {
            return call.lower(self, ctx, receiver);
        }
        bail_unsupported!(
            "method {method} is not supported on {} yet",
            kind(ctx, receiver)?.label()
        )
    }

    pub(super) fn builtin_call(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        method: crate::methods::Method,
        name: &str,
        args: &'s [Expr],
    ) -> Result<Option<Value>> {
        let value = self.expression(ctx, object)?;
        let receiver = self.defined(ctx, value)?;
        method.bind(name, args)?.lower(self, ctx, receiver)
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
            if let Some(callback) = self.optional_callback(ctx, replacement)? {
                return self
                    .replace_callback(ctx, receiver, (search, search_val), callback, all)
                    .map(Some);
            }
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
                        bail_unsupported!(
                            "String.prototype.replaceAll called with a non-global RegExp argument"
                        );
                    }
                    self.call_intrinsic(
                        ctx,
                        Intrinsic::RegexReplace,
                        vec![receiver, search_val, repl_val],
                    )
                }
                _ => {
                    bail_unsupported!(
                        "string.{method} requires a string or literal regex search pattern"
                    )
                }
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
        let error_class = match hir_compat::error_class(error_kind)? {
            hir_compat::ErrorClass::Any => ErrorKind::Any,
            hir_compat::ErrorClass::Type => ErrorKind::TypeError,
            hir_compat::ErrorClass::Range => ErrorKind::RangeError,
            hir_compat::ErrorClass::Reference => ErrorKind::ReferenceError,
            hir_compat::ErrorClass::Syntax => ErrorKind::SyntaxError,
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
        let classes = &self.shapes.specialization.bodies.classes;
        if classes
            .iter()
            .any(|class| class.name == ty || class.declaration.name == ty)
        {
            let is_inst = self
                .shapes
                .class_of_type(ctx, value.get_type(ctx))
                .and_then(|name| self.shapes.instance_class(name))
                .is_some_and(|class| class.name == ty || class.declaration.name == ty);
            return Ok(self.boolean(ctx, is_inst));
        }
        let (error, code) = match ty {
            "Error" => (ErrorKind::Any, 0),
            "TypeError" => (ErrorKind::TypeError, 1),
            "RangeError" => (ErrorKind::RangeError, 2),
            "ReferenceError" => (ErrorKind::ReferenceError, 6),
            "SyntaxError" => (ErrorKind::SyntaxError, 7),
            other => bail_unsupported!("instanceof {other} is not supported yet"),
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
            _ => bail_unsupported!(
                "BigInt conversion requires a number or bigint in the 64-bit transport profile"
            ),
        })
    }

    /// `Number(value)`.
    pub(super) fn number_coerce(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<Value> {
        let value = self.expression(ctx, expr)?;
        let value = self.arithmetic(ctx, value);
        ensure_unsupported!(
            kind(ctx, value)? != Kind::ValueUnion,
            "narrow the value union before numeric conversion"
        );
        Ok(match kind(ctx, value)? {
            Kind::BigInt => self.intrinsic(ctx, Intrinsic::BigIntToNumber, vec![value]),
            Kind::String => self.intrinsic(ctx, Intrinsic::StringToNumber, vec![value]),
            Kind::Number => value,
            _ => bail_unsupported!("Number conversion requires a number, bigint or string"),
        })
    }

    pub(super) fn data_view(&mut self, ctx: &mut Context, args: &'s [Expr]) -> Result<Value> {
        let Some((buffer, rest)) = args.split_first() else {
            bail_diag!(codes::TYPE_MISMATCH, "DataView requires an ArrayBuffer");
        };
        let buffer = self.typed_operand(ctx, buffer, Kind::ByteBuffer, "an ArrayBuffer")?;
        let offset = self.number_or(ctx, rest.first(), 0.0)?;
        let number = FP64Type::get(ctx).into();
        let optional = perry_mir::OptionalType::get(ctx, number).into();
        let length = match rest.get(1) {
            Some(expr) => {
                let value = self.expression_with(ctx, expr, Some(optional))?;
                self.fit(ctx, value, optional)?
            }
            None => self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, optional)),
        };
        for extra in rest.iter().skip(2) {
            self.effect(ctx, extra)?;
        }
        Ok(self.intrinsic(ctx, Intrinsic::DataViewNew, vec![buffer, offset, length]))
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
        let expected = matches!(source, Expr::Array(_, _)).then_some(numbers);
        let value = self.expression_with(ctx, source, expected)?;
        let value = self.defined(ctx, value)?;
        let value = self.arithmetic(ctx, value);
        Ok(match kind(ctx, value)? {
            Kind::Number if matches!(creation, BytesCreation::Construct) => {
                self.intrinsic(ctx, Intrinsic::BytesNew, vec![value])
            }
            Kind::Array => {
                ensure_diag!(
                    value.get_type(ctx) == numbers,
                    codes::TYPE_MISMATCH,
                    "Uint8Array sources must be number arrays"
                );
                self.intrinsic(ctx, Intrinsic::BytesFromArray, vec![value])
            }
            Kind::ByteBuffer => self.intrinsic(ctx, Intrinsic::BytesFromBuffer, vec![value]),
            Kind::Bytes => self.intrinsic(ctx, Intrinsic::BytesCopy, vec![value]),
            _ => bail_unsupported!("new Uint8Array takes a length, a number[] or a Uint8Array"),
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

    pub(super) fn decode(
        &mut self,
        ctx: &mut Context,
        decoder: Value,
        input: Option<&'s Expr>,
        options: Option<&'s Expr>,
    ) -> Result<Value> {
        let bytes = match input {
            None | Some(Expr::Undefined) => {
                let zero = self.number(ctx, 0.0);
                self.intrinsic(ctx, Intrinsic::BytesNew, vec![zero])
            }
            Some(input) => self.typed_operand(ctx, input, Kind::Bytes, "a Uint8Array")?,
        };
        let mut stream = self.boolean(ctx, false);
        for (name, value) in self.init_fields(options)? {
            ensure_unsupported!(
                name == "stream",
                "unsupported TextDecoder.decode option {name}"
            );
            stream = self.condition(ctx, value)?;
        }
        Ok(self.intrinsic(ctx, Intrinsic::TextDecode, vec![decoder, bytes, stream]))
    }

    /// `console.log(a, b, …)`: the arguments' strings joined by spaces.
    pub(super) fn console_line(&mut self, ctx: &mut Context, args: &'s [Expr]) -> Result<Value> {
        let mut line: Option<Value> = None;
        for arg in args {
            let value = self.expression(ctx, arg)?;
            let text = self.display_as(ctx, value, crate::lower::coerce::DisplayMode::Console)?;
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
