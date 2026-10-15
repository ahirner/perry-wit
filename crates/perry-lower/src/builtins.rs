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
        if method == streams::ITERATION && matches!(object, Expr::UrlSearchParamsNew(_)) {
            bail!(
                "synchronous for...of requires a dense typed array or string; URLSearchParams iteration is unsupported"
            );
        }
        let receiver = self.expression(ctx, object)?;
        let receiver = self.defined(ctx, receiver)?;
        if method == streams::ITERATION {
            ensure!(
                matches!(kind(ctx, receiver)?, Kind::Array | Kind::String),
                "synchronous for...of requires a dense typed array or string; this receiver has no supported iterator"
            );
            return Ok(Some(receiver));
        }
        if method == array_methods::PUSH {
            ensure!(
                kind(ctx, receiver)? == Kind::Array,
                "push requires a dense array receiver"
            );
        }
        let arg = |index: usize| args.get(index);
        let method = if method == decoder::DECODE {
            "decode"
        } else if method == array_methods::PUSH {
            "push"
        } else if method == array_methods::CONCAT {
            "concat"
        } else {
            method
        };
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
            (Kind::String, "split") => {
                let [separator] = args else {
                    bail!("string.split requires one string separator");
                };
                let separator = self.string_operand(ctx, separator)?;
                self.call_intrinsic(ctx, Intrinsic::StringSplit, vec![receiver, separator])
            }
            (Kind::Regex, "test") => {
                let [text] = args else {
                    bail!("literal regex.test requires one string");
                };
                let text = self.string_operand(ctx, text)?;
                Some(self.regex_test(ctx, receiver, text))
            }
            (Kind::String, "search") => {
                let [regex] = args else {
                    bail!("string.search requires one literal regex");
                };
                let regex = self.typed_operand(ctx, regex, Kind::Regex, "a literal regex")?;
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
                let intrinsic = if method == "codePointAt" {
                    Intrinsic::StringCodePointAt
                } else {
                    ensure!(method != "at", "String.prototype.at is not supported yet");
                    Intrinsic::StringCharAt
                };
                self.call_intrinsic(ctx, intrinsic, vec![receiver, position])
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
                "toLowerCase" | "toUpperCase" | "trim" | "toLocaleLowerCase" | "toLocaleUpperCase",
            ) => {
                let intrinsic = match method {
                    "trim" => Intrinsic::StringTrim,
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

    /// `value instanceof Error` or a supported Error subclass.
    pub(super) fn instance_of(
        &mut self,
        ctx: &mut Context,
        value: &'s Expr,
        ty: &str,
    ) -> Result<Value> {
        let value = self.expression(ctx, value)?;
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
