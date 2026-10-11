//! Built-in globals and methods of primitive values: `Math`, `JSON`, errors, strings,
//! bytes, text coding and `console`.
use super::*;

/// Accepted deviation: JSON revivers, replacers and indentation are diagnosed at compile time.
pub(super) const JSON_CALLBACKS: &str = "JSON revivers, replacers and indentation are not supported (accepted profile): parse into a typed shape and transform it in code";

impl FunctionBuilder<'_> {
    /// `object.method(args)` on built-in values.
    pub(super) fn method(
        &mut self,
        ctx: &mut Context,
        object: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<Option<Value>> {
        let receiver = self.expression(ctx, object)?;
        let receiver = self.defined(ctx, receiver)?;
        let arg = |index: usize| args.get(index);
        let method = if method == decoder::DECODE {
            "decode"
        } else if method == array_methods::PUSH {
            "push"
        } else {
            method
        };
        let value = match (kind(ctx, receiver)?, method) {
            (Kind::Array, "push") => {
                let element = self.array_element(ctx, receiver)?;
                let values = args
                    .iter()
                    .map(|argument| {
                        let value = self.expression_with(ctx, argument, Some(element))?;
                        self.fit(ctx, value, element)
                    })
                    .collect::<Result<Vec<_>>>()?;
                for value in values {
                    self.emit(ctx, |ctx| ArrayPushOp::new(ctx, receiver, value));
                }
                let length = self.emit_value(ctx, |ctx| ArrayLenOp::new(ctx, receiver));
                Some(self.emit_value(ctx, |ctx| {
                    ConvertOp::new(ctx, ConvertKind::I32ToF64, length)
                }))
            }
            (Kind::Array, "map" | "filter") => {
                let [callback] = args else {
                    bail!("array.{method} requires one static callback")
                };
                Some(self.array_transform(ctx, receiver, callback, method == "filter")?)
            }
            (Kind::Array, "includes" | "indexOf") => {
                ensure!(
                    (1..=2).contains(&args.len()),
                    "array.{method} requires a value and optional starting index"
                );
                Some(self.array_search(
                    ctx,
                    receiver,
                    &args[0],
                    args.get(1),
                    method == "includes",
                )?)
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
                let ty = self.void_type(ctx);
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
            (_, method) => bail!("method {method} is not supported on this type yet"),
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
    pub(super) fn stringify(&mut self, ctx: &mut Context, value: &Expr) -> Result<Value> {
        let value = self.expression(ctx, value)?;
        Ok(self.emit_value(ctx, |ctx| JsonStringifyOp::new(ctx, value)))
    }

    /// `new Error(message, { cause })` of `kind`: causes are strings (accepted profile).
    pub(super) fn error_with_cause(
        &mut self,
        ctx: &mut Context,
        kind: ErrorKind,
        message: &Expr,
        cause: &Expr,
    ) -> Result<Value> {
        let message = self.string_operand(ctx, message)?;
        let error = self.emit_value(ctx, |ctx| ErrorNewOp::new(ctx, kind, message));
        let cause = self.typed_operand(ctx, cause, Kind::String, "a string error cause")?;
        self.intrinsic_void(ctx, Intrinsic::ExceptionSetCause, vec![error, cause]);
        Ok(error)
    }

    /// `new Error(message, options)` and its subclasses, by HIR error-kind code.
    pub(super) fn error_with_options(
        &mut self,
        ctx: &mut Context,
        error_kind: u32,
        message: &Expr,
        options: &Expr,
    ) -> Result<Value> {
        let kind = match error_kind {
            0 => ErrorKind::Any,
            1 => ErrorKind::TypeError,
            2 => ErrorKind::RangeError,
            _ => bail!("only Error, TypeError and RangeError are supported yet"),
        };
        let cause = match options {
            Expr::New {
                class_name, args, ..
            } if self
                .shapes
                .literal_fields(class_name)
                .is_some_and(|names| names == ["cause"]) =>
            {
                &args[0]
            }
            Expr::Object(fields) if fields.len() == 1 && fields[0].0 == "cause" => &fields[0].1,
            _ => bail!("error options must be a literal {{ cause }}"),
        };
        self.error_with_cause(ctx, kind, message, cause)
    }

    /// `value instanceof Error` (or `TypeError`, `RangeError`).
    pub(super) fn instance_of(
        &mut self,
        ctx: &mut Context,
        value: &Expr,
        ty: &str,
    ) -> Result<Value> {
        let value = self.expression(ctx, value)?;
        let error = match ty {
            "Error" => ErrorKind::Any,
            "TypeError" => ErrorKind::TypeError,
            "RangeError" => ErrorKind::RangeError,
            other => bail!("instanceof {other} is not supported yet"),
        };
        Ok(match kind(ctx, value)? {
            Kind::Exception => self.emit_value(ctx, |ctx| ExceptionIsOp::new(ctx, error, value)),
            _ => self.boolean(ctx, false),
        })
    }

    /// `BigInt(value)` of a number or bigint.
    pub(super) fn bigint_coerce(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
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
    pub(super) fn number_coerce(&mut self, ctx: &mut Context, expr: &Expr) -> Result<Value> {
        let value = self.expression(ctx, expr)?;
        ensure!(
            kind(ctx, value)? != Kind::NumberString,
            "narrow number | string before numeric conversion"
        );
        Ok(if kind(ctx, value)? == Kind::BigInt {
            self.intrinsic(ctx, Intrinsic::BigIntToNumber, vec![value])
        } else {
            self.arithmetic(ctx, value)
        })
    }

    /// `Math.floor` and the other one-argument `Math` functions of `expr`.
    pub(super) fn math_unary(
        &mut self,
        ctx: &mut Context,
        expr: &Expr,
        value: &Expr,
    ) -> Result<Value> {
        let intrinsic = match expr {
            Expr::MathFloor(_) => Intrinsic::MathFloor,
            Expr::MathCeil(_) => Intrinsic::MathCeil,
            Expr::MathTrunc(_) => Intrinsic::MathTrunc,
            Expr::MathAbs(_) => Intrinsic::MathAbs,
            Expr::MathRound(_) => Intrinsic::MathRound,
            _ => Intrinsic::MathSqrt,
        };
        let value = self.number_operand(ctx, value)?;
        Ok(self.intrinsic(ctx, intrinsic, vec![value]))
    }

    /// `Math.min(…)` (`min`) or `Math.max(…)`.
    pub(super) fn math_extremum(
        &mut self,
        ctx: &mut Context,
        min: bool,
        values: &[Expr],
    ) -> Result<Value> {
        let (intrinsic, empty) = if min {
            (Intrinsic::MathMin, f64::INFINITY)
        } else {
            (Intrinsic::MathMax, f64::NEG_INFINITY)
        };
        let mut result = self.number(ctx, empty);
        for value in values {
            let value = self.number_operand(ctx, value)?;
            result = self.intrinsic(ctx, intrinsic, vec![result, value]);
        }
        Ok(result)
    }

    /// `new Uint8Array(source)` (`construct`, which also takes a length) or
    /// `Uint8Array.from(source)`.
    pub(super) fn bytes_from(
        &mut self,
        ctx: &mut Context,
        construct: bool,
        source: &Expr,
    ) -> Result<Value> {
        let number = FP64Type::get(ctx).into();
        let numbers = ArrayType::get(ctx, number).into();
        let expected = matches!(source, Expr::Array(_)).then_some(numbers);
        let value = self.expression_with(ctx, source, expected)?;
        let value = self.defined(ctx, value)?;
        let value = self.arithmetic(ctx, value);
        Ok(match kind(ctx, value)? {
            Kind::Number if construct => self.intrinsic(ctx, Intrinsic::BytesNew, vec![value]),
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
        label: &Expr,
        fatal: &Expr,
        ignore_bom: &Expr,
    ) -> Result<Value> {
        ensure!(
            matches!(label, Expr::Undefined)
                || matches!(label, Expr::String(l) if l.eq_ignore_ascii_case("utf-8") || l.eq_ignore_ascii_case("utf8")),
            "TextDecoder supports UTF-8 only"
        );
        let fatal = match fatal {
            Expr::Undefined => self.boolean(ctx, false),
            fatal => self.condition(ctx, fatal)?,
        };
        let ignore_bom = match ignore_bom {
            Expr::Undefined => self.boolean(ctx, false),
            ignore_bom => self.condition(ctx, ignore_bom)?,
        };
        Ok(self.intrinsic(ctx, Intrinsic::DecoderNew, vec![fatal, ignore_bom]))
    }

    /// `console.log(a, b, …)`: the arguments' strings joined by spaces.
    pub(super) fn console_line(&mut self, ctx: &mut Context, args: &[Expr]) -> Result<Value> {
        let mut line: Option<Value> = None;
        for arg in args {
            let value = self.expression(ctx, arg)?;
            let text = self.display(ctx, value)?;
            line = Some(match line {
                None => text,
                Some(line) => {
                    let space = self.string(ctx, " ");
                    let spaced = self.emit_value(ctx, |ctx| StringConcatOp::new(ctx, line, space));
                    self.emit_value(ctx, |ctx| StringConcatOp::new(ctx, spaced, text))
                }
            });
        }
        Ok(match line {
            Some(line) => line,
            None => self.string(ctx, ""),
        })
    }
}
