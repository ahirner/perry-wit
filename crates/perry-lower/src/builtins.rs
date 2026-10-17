//! Built-in globals and methods of primitive values: `Math`, `JSON`, errors, strings,
//! bytes, text coding and `console`.
use super::*;

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
        let arg = |index: usize| args.get(index);
        let method = method.name();
        if let Some(value) = self.collection_method(ctx, object, receiver, method, args)? {
            return Ok(value);
        }
        let value = match (kind(ctx, receiver)?, method) {
            (Kind::Array, "push") => Some(self.push_values(ctx, receiver, args)?),
            (Kind::String, "concat") => {
                let mut segments = Vec::with_capacity(args.len());
                for argument in args {
                    let value = self.expression(ctx, argument)?;
                    segments.push(self.display(ctx, value)?);
                }
                let mut result = receiver;
                for segment in segments {
                    result = self.emit_value(ctx, |ctx| ConcatOp::new(ctx, result, segment));
                }
                Some(result)
            }
            (Kind::Array, "concat") => Some(self.concat_values(ctx, receiver, args)?),
            (Kind::Array, "map" | "filter") => {
                let [callback] = args else {
                    bail!("array.{method} requires one static callback")
                };
                Some(self.array_transform(ctx, receiver, callback, method == "filter")?)
            }
            (Kind::Array, "find" | "findIndex") => {
                let [callback] = args else {
                    bail!("array.{method} requires one static callback")
                };
                Some(self.array_find(ctx, receiver, callback, method == "findIndex")?)
            }
            (Kind::Array, "some" | "every") => {
                let [callback] = args else {
                    bail!("array.{method} requires one static callback")
                };
                Some(self.array_quantifier(ctx, receiver, callback, method == "every")?)
            }
            (Kind::Array, "flatMap") => {
                let [callback] = args else {
                    bail!("array.flatMap requires one static callback")
                };
                Some(self.array_flat_map(ctx, receiver, callback)?)
            }
            (Kind::Array, "sort" | "toSorted") => {
                ensure!(
                    args.len() <= 1,
                    "array.{method} accepts at most one comparator callback"
                );
                Some(self.array_sort(ctx, receiver, arg(0), method == "toSorted")?)
            }
            (Kind::Array, "reduce" | "reduceRight") => {
                let (Some(callback), true) = (arg(0), args.len() <= 2) else {
                    bail!("array.{method} requires a callback and optional initial value")
                };
                Some(self.array_reduce(ctx, receiver, callback, arg(1), method == "reduceRight")?)
            }
            (Kind::Array, "forEach") => {
                let [callback] = args else {
                    bail!("array.forEach requires one static callback")
                };
                Some(self.array_for_each(ctx, receiver, callback)?)
            }
            (Kind::Array, "join") => {
                ensure!(args.len() <= 1, "array.join accepts at most one separator");
                Some(self.array_join(ctx, receiver, arg(0))?)
            }
            (Kind::Array, "slice") => {
                ensure!(args.len() <= 2, "array.slice accepts at most two bounds");
                Some(self.array_slice(ctx, receiver, arg(0), arg(1))?)
            }
            (Kind::Array, "reverse" | "toReversed") => {
                ensure!(args.is_empty(), "array.{method} takes no arguments");
                Some(self.array_reverse(ctx, receiver, method == "toReversed")?)
            }
            (Kind::Array, "flat") => {
                ensure!(
                    args.len() <= 1,
                    "array.flat accepts at most one depth argument"
                );
                Some(self.array_flat(ctx, receiver, arg(0))?)
            }
            (Kind::Array, "findLast" | "findLastIndex") => {
                let [callback] = args else {
                    bail!("array.{method} requires one static callback")
                };
                Some(self.array_find_last(ctx, receiver, callback, method == "findLastIndex")?)
            }
            (Kind::Array, "at") => {
                ensure!(args.len() <= 1, "array.at accepts at most one index");
                Some(self.array_at(ctx, receiver, arg(0))?)
            }
            (Kind::Array, "pop") => {
                ensure!(args.is_empty(), "array.pop takes no arguments");
                Some(self.array_pop(ctx, receiver)?)
            }
            (Kind::Array, "with") => {
                let [index, value] = args else {
                    bail!("array.with requires index and value arguments");
                };
                Some(self.array_with(ctx, receiver, index, value)?)
            }
            (Kind::String, "normalize") => {
                ensure!(
                    args.len() <= 1,
                    "string.normalize accepts at most one form argument"
                );
                let form = match args.first() {
                    None | Some(Expr::Undefined) => self.string(ctx, "NFC"),
                    Some(arg) => self.string_operand(ctx, arg)?,
                };
                self.call_intrinsic(ctx, Intrinsic::StringNormalize, vec![receiver, form])
            }
            (Kind::String, "localeCompare") => {
                let [other] = args else {
                    bail!("string.localeCompare requires one string");
                };
                let other = self.string_operand(ctx, other)?;
                let num_ty = FP64Type::get(ctx).into();
                let lt = self.compare(ctx, CompareKind::Lt, receiver, other);
                Some(self.choose(
                    ctx,
                    num_ty,
                    lt,
                    |b, ctx| Ok(b.number(ctx, -1.0)),
                    |b, ctx| {
                        let gt = b.compare(ctx, CompareKind::Gt, receiver, other);
                        b.choose(
                            ctx,
                            num_ty,
                            gt,
                            |b, ctx| Ok(b.number(ctx, 1.0)),
                            |b, ctx| Ok(b.number(ctx, 0.0)),
                        )
                    },
                )?)
            }
            (Kind::Array, "includes" | "indexOf") => {
                let (value, from_index) = match args {
                    [value] => (value, None),
                    [value, from_index] => (value, Some(from_index)),
                    _ => bail!("array.{method} requires a value and optional starting index"),
                };
                Some(self.array_search(ctx, receiver, value, from_index, method == "includes")?)
            }
            (Kind::Array, "lastIndexOf") => {
                let (value, from_index) = match args {
                    [value] => (value, None),
                    [value, from_index] => (value, Some(from_index)),
                    _ => bail!("array.lastIndexOf requires a value and optional starting index"),
                };
                Some(self.array_last_index_of(ctx, receiver, value, from_index)?)
            }
            (Kind::String, "split") => {
                let [separator] = args else {
                    bail!("string.split requires one string or regex separator");
                };
                let sep_val = self.expression(ctx, separator)?;
                let sep_val = self.defined(ctx, sep_val)?;
                match kind(ctx, sep_val)? {
                    Kind::String => {
                        self.call_intrinsic(ctx, Intrinsic::StringSplit, vec![receiver, sep_val])
                    }
                    Kind::Regex => {
                        self.call_intrinsic(ctx, Intrinsic::RegexSplit, vec![receiver, sep_val])
                    }
                    _ => bail!("string.split requires a string or literal regex separator"),
                }
            }
            (Kind::String, "replace" | "replaceAll") => {
                let [search, replacement] = args else {
                    bail!("string.{method} requires a search pattern and replacement string");
                };
                let search_val = self.expression(ctx, search)?;
                let search_val = self.defined(ctx, search_val)?;
                let repl_val = self.string_operand(ctx, replacement)?;
                match kind(ctx, search_val)? {
                    Kind::String => {
                        let intrinsic = if method == "replaceAll" {
                            Intrinsic::StringReplaceAll
                        } else {
                            Intrinsic::StringReplace
                        };
                        self.call_intrinsic(ctx, intrinsic, vec![receiver, search_val, repl_val])
                    }
                    Kind::Regex => {
                        if method == "replaceAll" && !Self::regex_has_global(ctx, search_val) {
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
            }
            (Kind::Regex, "test") => {
                ensure!(
                    !Self::regex_has_global(ctx, receiver),
                    "Regex search supports only the u and s flags"
                );
                let [text] = args else {
                    bail!("literal regex.test requires one string");
                };
                let text = self.string_operand(ctx, text)?;
                Some(self.regex_test(ctx, receiver, text))
            }
            (Kind::Regex, "exec") => {
                let [text] = args else {
                    bail!("regex.exec requires one string");
                };
                let text = self.string_operand(ctx, text)?;
                self.call_intrinsic(ctx, Intrinsic::RegexExec, vec![text, receiver])
            }
            (Kind::String, "match") => {
                let [regex] = args else {
                    bail!("string.match requires one literal regex");
                };
                let regex = self.typed_operand(ctx, regex, Kind::Regex, "a literal regex")?;
                self.call_intrinsic(ctx, Intrinsic::RegexMatch, vec![receiver, regex])
            }
            (Kind::String, "search") => {
                let [regex] = args else {
                    bail!("string.search requires one literal regex");
                };
                let regex = self.typed_operand(ctx, regex, Kind::Regex, "a literal regex")?;
                ensure!(
                    !Self::regex_has_global(ctx, regex),
                    "Regex search supports only the u and s flags"
                );
                self.call_intrinsic(ctx, Intrinsic::RegexSearch, vec![receiver, regex])
            }
            (Kind::Decoder, "decode") => {
                ensure!(
                    args.len() <= 2,
                    "TextDecoder.decode takes bytes and stream options"
                );
                Some(self.decode(ctx, receiver, arg(0), arg(1))?)
            }
            (Kind::Url, "toString" | "toJSON") => Some(self.url_field(ctx, receiver, "href")?),
            (Kind::SearchParams, "append") => {
                let [name, value] = args else {
                    bail!("searchParams.append needs a name and value");
                };
                let name = self.string_operand(ctx, name)?;
                let value = self.string_operand(ctx, value)?;
                self.intrinsic_void(ctx, Intrinsic::UrlAppend, vec![receiver, name, value]);
                let ty = void_type(ctx);
                Some(self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty)))
            }
            (
                Kind::Headers
                | Kind::Response
                | Kind::Request
                | Kind::Writable
                | Kind::Writer
                | Kind::Readable
                | Kind::Reader
                | Kind::ByobReader,
                _,
            ) => {
                return self.web_member(ctx, receiver, method, Some(args)).map(Some);
            }
            (Kind::Instant | Kind::PlainDate | Kind::PlainDateTime, _) => {
                return self.temporal_method(ctx, receiver, method, args);
            }
            (Kind::Abort, "abort") => {
                ensure!(args.is_empty(), "abort reasons are not supported yet");
                self.intrinsic_void(ctx, Intrinsic::AbortAbort, vec![receiver]);
                return Ok(None);
            }
            (Kind::Stats, "isFile" | "isDirectory") => {
                let intrinsic = if method == "isFile" {
                    Intrinsic::StatsIsFile
                } else {
                    Intrinsic::StatsIsDirectory
                };
                self.call_intrinsic(ctx, intrinsic, vec![receiver])
            }
            (Kind::String, "charCodeAt") => {
                bail!("charCodeAt uses UTF-16 code units; use codePointAt with scalar indices")
            }
            (Kind::String, "charAt" | "codePointAt" | "at") => {
                let position = self.number_or(ctx, arg(0), 0.0)?;
                let intrinsic = match method {
                    "codePointAt" => Intrinsic::StringCodePointAt,
                    "at" => Intrinsic::StringAt,
                    _ => Intrinsic::StringCharAt,
                };
                self.call_intrinsic(ctx, intrinsic, vec![receiver, position])
            }
            (Kind::String, "padStart" | "padEnd") => {
                let target_len = self.number_or(ctx, arg(0), 0.0)?;
                let pad_str = match arg(1) {
                    None | Some(Expr::Undefined) => self.string(ctx, " "),
                    Some(expr) => {
                        let val = self.expression(ctx, expr)?;
                        self.display(ctx, val)?
                    }
                };
                let intrinsic = if method == "padStart" {
                    Intrinsic::StringPadStart
                } else {
                    Intrinsic::StringPadEnd
                };
                self.call_intrinsic(ctx, intrinsic, vec![receiver, target_len, pad_str])
            }
            (Kind::String, "slice" | "substring") => {
                let start = self.number_or(ctx, arg(0), 0.0)?;
                let end = self.number_or(ctx, arg(1), f64::INFINITY)?;
                let intrinsic = if method == "slice" {
                    Intrinsic::StringSlice
                } else {
                    Intrinsic::StringSubstring
                };
                self.call_intrinsic(ctx, intrinsic, vec![receiver, start, end])
            }
            (Kind::String, "indexOf" | "lastIndexOf" | "includes" | "startsWith" | "endsWith") => {
                let needle = arg(0).context("a search string is required")?;
                let needle = self.typed_operand(ctx, needle, Kind::String, "a search string")?;
                let default = if matches!(method, "lastIndexOf" | "endsWith") {
                    f64::INFINITY
                } else {
                    0.0
                };
                let position = self.number_or(ctx, arg(1), default)?;
                let args = vec![receiver, needle, position];
                match method {
                    "indexOf" => self.call_intrinsic(ctx, Intrinsic::StringIndexOf, args),
                    "lastIndexOf" => self.call_intrinsic(ctx, Intrinsic::StringLastIndexOf, args),
                    "startsWith" => self.call_intrinsic(ctx, Intrinsic::StringStartsWith, args),
                    "endsWith" => self.call_intrinsic(ctx, Intrinsic::StringEndsWith, args),
                    _ => {
                        let found = self.intrinsic(ctx, Intrinsic::StringIndexOf, args);
                        let missing = self.number(ctx, -1.0);
                        Some(self.compare(ctx, CompareKind::Ne, found, missing))
                    }
                }
            }
            (
                Kind::String,
                "toLowerCase" | "toUpperCase" | "trim" | "trimStart" | "trimLeft" | "trimEnd"
                | "trimRight" | "toLocaleLowerCase" | "toLocaleUpperCase",
            ) => {
                let intrinsic = match method {
                    "trim" => Intrinsic::StringTrim,
                    "trimStart" | "trimLeft" => Intrinsic::StringTrimStart,
                    "trimEnd" | "trimRight" => Intrinsic::StringTrimEnd,
                    "toLowerCase" | "toLocaleLowerCase" => Intrinsic::StringLower,
                    _ => Intrinsic::StringUpper,
                };
                self.call_intrinsic(ctx, intrinsic, vec![receiver])
            }
            (Kind::String, "repeat") => {
                let count = self.number_or(ctx, arg(0), 0.0)?;
                self.call_intrinsic(ctx, Intrinsic::StringRepeat, vec![receiver, count])
            }
            (Kind::String, "toString") => Some(receiver),
            (Kind::Number, "toString") => {
                ensure!(
                    args.len() <= 1,
                    "Number.prototype.toString accepts at most one radix"
                );
                let num = self.arithmetic(ctx, receiver);
                match arg(0) {
                    None | Some(Expr::Undefined) => {
                        self.call_intrinsic(ctx, Intrinsic::NumberToString, vec![num])
                    }
                    Some(radix_expr) => {
                        let radix = self.number_operand(ctx, radix_expr)?;
                        self.call_intrinsic(ctx, Intrinsic::NumberToStringRadix, vec![num, radix])
                    }
                }
            }
            (Kind::Bytes, "subarray" | "slice") => {
                let start = self.number_or(ctx, arg(0), 0.0)?;
                let end = self.number_or(ctx, arg(1), f64::INFINITY)?;
                let intrinsic = if method == "subarray" {
                    Intrinsic::BytesSubarray
                } else {
                    Intrinsic::BytesSlice
                };
                self.call_intrinsic(ctx, intrinsic, vec![receiver, start, end])
            }
            (Kind::Bytes, "set") => {
                let source = arg(0).context("set needs a source")?;
                let source = self.expression(ctx, source)?;
                let source = self.defined(ctx, source)?;
                let source = match kind(ctx, source)? {
                    Kind::Bytes => source,
                    Kind::Array => self.intrinsic(ctx, Intrinsic::BytesFromArray, vec![source]),
                    _ => bail!("Uint8Array.set needs a Uint8Array or number[] source"),
                };
                let offset = self.number_or(ctx, arg(1), 0.0)?;
                self.intrinsic_void(ctx, Intrinsic::BytesSetFrom, vec![receiver, source, offset]);
                None
            }
            (Kind::Bytes, "fill") => {
                let value = self.number_or(ctx, arg(0), 0.0)?;
                let start = self.number_or(ctx, arg(1), 0.0)?;
                let end = self.number_or(ctx, arg(2), f64::INFINITY)?;
                self.intrinsic_void(ctx, Intrinsic::BytesFill, vec![receiver, value, start, end]);
                Some(receiver)
            }
            (Kind::Bytes, "$$cryptoFillRandom") => {
                self.call_intrinsic(ctx, Intrinsic::BytesFillRandom, vec![receiver])
            }
            (Kind::Date, "getTime" | "valueOf") => {
                self.call_intrinsic(ctx, Intrinsic::DateTime, vec![receiver])
            }
            (Kind::Date, "toISOString" | "toJSON") => {
                let intrinsic = if method == "toJSON" {
                    Intrinsic::DateJson
                } else {
                    Intrinsic::DateIso
                };
                self.call_intrinsic(ctx, intrinsic, vec![receiver])
            }
            (receiver_kind, method) => {
                bail!(
                    "method {method} is not supported on {} yet",
                    receiver_kind.label()
                )
            }
        };
        Ok(value)
    }

    pub(super) fn regex_test(&mut self, ctx: &mut Context, regex: Value, text: Value) -> Value {
        let index = self.intrinsic(ctx, Intrinsic::RegexSearch, vec![text, regex]);
        let zero = self.number(ctx, 0.0);
        self.emit_value(ctx, |ctx| {
            MirCompare::new(ctx, CompareKind::Ge, index, zero)
        })
    }

    pub(super) fn regex_has_global(ctx: &Context, regex: Value) -> bool {
        let Some(op) = regex.defining_op() else {
            return false;
        };
        if let Some(const_op) = Operation::get_op::<perry_mir::RegexConstOp>(op, ctx) {
            return const_op.flags(ctx).contains('g');
        }
        if Operation::get_op::<perry_mir::LoadOp>(op, ctx).is_some() {
            let slot = op.deref(ctx).get_operand(0);
            return slot.uses(ctx).iter().any(|u| {
                let user = u.user_op();
                Operation::get_op::<perry_mir::StoreOp>(user, ctx).is_some()
                    && user.deref(ctx).get_operand(0) == slot
                    && Self::regex_has_global(ctx, user.deref(ctx).get_operand(1))
            });
        }
        false
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
            let text = self.display(ctx, value)?;
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
