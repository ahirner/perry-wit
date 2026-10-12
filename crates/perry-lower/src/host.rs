//! Host capabilities: timers, `fetch` and the web stream/HTTP classes, and `node:fs/promises`.
use super::*;

impl<'s> FunctionBuilder<'s> {
    /// `setTimeout(delay?, value?)` from `node:timers/promises`.
    pub(super) fn timer(&mut self, ctx: &mut Context, args: &'s [Expr]) -> Result<Value> {
        ensure!(
            args.len() <= 3,
            "setTimeout takes a delay, a value and options"
        );
        let delay = match args.first() {
            Some(delay) => {
                let value = self.expression(ctx, delay)?;
                let value = self.arithmetic(ctx, value);
                ensure!(
                    kind(ctx, value)? == Kind::Number,
                    "setTimeout delay must be a number"
                );
                value
            }
            None => self.number(ctx, 1.0),
        };
        let value = match args.get(1) {
            Some(value) => {
                let value = self.expression(ctx, value)?;
                self.defined(ctx, value)?
            }
            None => {
                let number = FP64Type::get(ctx).into();
                self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number))
            }
        };
        // `{ signal }`: the only option (`ref` has no meaning in a component).
        let signal = match args.get(2) {
            None => None,
            Some(options) => {
                let signal = match options {
                    Expr::New {
                        class_name, args, ..
                    } if self
                        .shapes
                        .literal_fields(class_name)
                        .is_some_and(|names| names == ["signal"]) =>
                    {
                        &args[0]
                    }
                    Expr::Object(fields) if fields.len() == 1 && fields[0].0 == "signal" => {
                        &fields[0].1
                    }
                    _ => bail!("setTimeout options must be a literal {{ signal }}"),
                };
                Some(self.typed_operand(ctx, signal, Kind::Abort, "an AbortSignal")?)
            }
        };
        Ok(self.emit_value(ctx, |ctx| TimerOp::new(ctx, delay, value, signal)))
    }

    /// `new Headers(init?)`, `new Response(body?, init?)`, `new Request(url, init?)`.
    pub(super) fn web_new(
        &mut self,
        ctx: &mut Context,
        class: &str,
        args: &'s [Expr],
    ) -> Result<Value> {
        if class == "Headers" {
            ensure!(args.len() <= 1, "new Headers takes one init");
            return self.headers_init(ctx, args.first());
        }
        let (first, init) = (args.first(), args.get(1));
        ensure!(args.len() <= 2, "{class} takes two arguments");
        let fields = self.init_fields(init)?;
        let field = |name: &str| fields.iter().find(|(n, _)| *n == name).map(|(_, e)| *e);
        for (name, _) in &fields {
            let known: &[&str] = if class == "Response" {
                &["status", "statusText", "headers"]
            } else {
                &["method", "headers", "body"]
            };
            ensure!(
                known.contains(name),
                "{class} init field {name} is not supported yet"
            );
        }
        let headers = self.headers_init(ctx, field("headers"))?;
        let body = if class == "Response" {
            first
        } else {
            field("body")
        };
        let (body, text) = self.body_init(ctx, body)?;
        let text = self.boolean(ctx, text);
        if class == "Response" {
            let status = self.number_or(ctx, field("status"), 200.0)?;
            let status_text = match field("statusText") {
                Some(expr) => self.typed_operand(ctx, expr, Kind::String, "a status text")?,
                None => self.string(ctx, ""),
            };
            let operands = vec![body, text, status, status_text, headers];
            return Ok(self.intrinsic(ctx, Intrinsic::ResponseNew, operands));
        }
        let url = first.context("new Request needs a URL")?;
        let url = self.typed_operand(ctx, url, Kind::String, "a URL string")?;
        let method = match field("method") {
            Some(expr) => self.typed_operand(ctx, expr, Kind::String, "a method string")?,
            None => self.string(ctx, "GET"),
        };
        let operands = vec![url, method, body, text, headers];
        Ok(self.intrinsic(ctx, Intrinsic::RequestNew, operands))
    }

    /// The fields of an init object literal, by name.
    pub(super) fn init_fields<'e>(
        &self,
        init: Option<&'e Expr>,
    ) -> Result<Vec<(&'s str, &'e Expr)>> {
        const NOT_LITERAL: &str = "init objects must be object literals yet";
        match init {
            None | Some(Expr::Undefined) => Ok(Vec::new()),
            Some(Expr::New {
                class_name, args, ..
            }) => {
                let names = self
                    .shapes
                    .literal_fields(class_name)
                    .context(NOT_LITERAL)?;
                ensure!(
                    names.len() == args.len(),
                    "init object arity differs from its shape"
                );
                Ok(names.into_iter().zip(args.iter()).collect())
            }
            Some(_) => bail!(NOT_LITERAL),
        }
    }

    pub(super) fn fetch(&mut self, ctx: &mut Context, args: &'s [Expr]) -> Result<Value> {
        ensure!(
            (1..=2).contains(&args.len()),
            "fetch takes a URL and optional init"
        );
        let url = self.url_string(ctx, &args[0])?;
        let fields = self.init_fields(args.get(1))?;
        for (name, _) in &fields {
            ensure!(
                matches!(*name, "method" | "headers" | "body"),
                "fetch init field {name} is not supported yet"
            );
        }
        let field = |name: &str| fields.iter().find(|(n, _)| *n == name).map(|(_, e)| *e);
        let method = match field("method") {
            Some(method) => self.typed_operand(ctx, method, Kind::String, "a method string")?,
            None => self.string(ctx, "GET"),
        };
        let headers = self.headers_init(ctx, field("headers"))?;
        let (body, text) = self.body_init(ctx, field("body"))?;
        let text = self.boolean(ctx, text);
        Ok(self.intrinsic(
            ctx,
            Intrinsic::Fetch,
            vec![url, method, headers, body, text],
        ))
    }

    /// A header list from an init: absent, an object literal of string values, or `Headers`.
    fn headers_init(&mut self, ctx: &mut Context, init: Option<&'s Expr>) -> Result<Value> {
        if let Some(expr) = init
            && !matches!(expr, Expr::Undefined | Expr::New { .. })
        {
            let value = self.expression(ctx, expr)?;
            ensure!(
                kind(ctx, value)? == Kind::Headers,
                "headers init must be an object literal or Headers"
            );
            return Ok(self.intrinsic(ctx, Intrinsic::HeadersCopy, vec![value]));
        }
        let fields = self.init_fields(init)?;
        self.header_list(ctx, fields)
    }

    /// A new header list of string-valued `(name, value)` pairs, in order.
    fn header_list<'e: 's>(
        &mut self,
        ctx: &mut Context,
        fields: impl IntoIterator<Item = (&'e str, &'e Expr)>,
    ) -> Result<Value> {
        let headers = self.intrinsic(ctx, Intrinsic::HeadersNew, vec![]);
        for (name, expr) in fields {
            let name = self.string(ctx, name);
            let value = self.typed_operand(ctx, expr, Kind::String, "a header value")?;
            self.intrinsic_void(ctx, Intrinsic::HeadersAppend, vec![headers, name, value]);
        }
        Ok(headers)
    }

    /// A body init as `Uint8Array | undefined` and whether it was a string.
    fn body_init(&mut self, ctx: &mut Context, body: Option<&'s Expr>) -> Result<(Value, bool)> {
        let bytes: TypeHandle = BytesType::get(ctx).into();
        let Some(body) = body.filter(|b| !matches!(b, Expr::Undefined | Expr::Null)) else {
            return Ok((
                self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, bytes)),
                false,
            ));
        };
        let value = self.expression(ctx, body)?;
        let value = self.defined(ctx, value)?;
        let (value, text) = match kind(ctx, value)? {
            Kind::Bytes => (value, false),
            Kind::String => {
                let encoded = self.intrinsic(ctx, Intrinsic::TextEncode, vec![value]);
                (encoded, true)
            }
            _ => bail!("bodies are strings and Uint8Arrays yet"),
        };
        Ok((self.optional(ctx, value), text))
    }

    /// A property (`args` is `None`) or method of `Headers`, `Response` or `Request`.
    pub(super) fn web_member(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        name: &str,
        args: Option<&'s [Expr]>,
    ) -> Result<Value> {
        let receiver_kind = kind(ctx, receiver)?;
        let strings = |builder: &mut Self, ctx: &mut Context, count: usize| -> Result<Vec<Value>> {
            let args = args.unwrap_or(&[]);
            ensure!(args.len() == count, "{name} takes {count} arguments");
            args.iter()
                .map(|arg| builder.typed_operand(ctx, arg, Kind::String, "a string"))
                .collect()
        };
        let (intrinsic, operands) = match (receiver_kind, name, args.is_some()) {
            (Kind::Headers, "append" | "set", true) => {
                let mut operands = vec![receiver];
                operands.extend(strings(self, ctx, 2)?);
                let intrinsic = if name == "append" {
                    Intrinsic::HeadersAppend
                } else {
                    Intrinsic::HeadersSet
                };
                self.call_intrinsic(ctx, intrinsic, operands);
                // Both return `undefined`; statements discard it.
                let number = FP64Type::get(ctx).into();
                return Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number)));
            }
            (Kind::Headers, "delete", true) => {
                let mut operands = vec![receiver];
                operands.extend(strings(self, ctx, 1)?);
                self.intrinsic_void(ctx, Intrinsic::HeadersDelete, operands);
                let number = FP64Type::get(ctx).into();
                return Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number)));
            }
            (Kind::Headers, "get" | "has", true) => {
                let mut operands = vec![receiver];
                operands.extend(strings(self, ctx, 1)?);
                let intrinsic = if name == "get" {
                    Intrinsic::HeadersGet
                } else {
                    Intrinsic::HeadersHas
                };
                (intrinsic, operands)
            }
            (Kind::Response, "status", false) => (Intrinsic::ResponseStatus, vec![receiver]),
            (Kind::Response, "statusText", false) => {
                (Intrinsic::ResponseStatusText, vec![receiver])
            }
            (Kind::Response, "ok", false) => (Intrinsic::ResponseOk, vec![receiver]),
            (Kind::Response, "headers", false) => (Intrinsic::ResponseHeaders, vec![receiver]),
            (Kind::Response, "body", false) => (Intrinsic::ResponseBody, vec![receiver]),
            (Kind::Request, "body", false) => (Intrinsic::RequestBody, vec![receiver]),
            (Kind::Response, "bodyUsed", false) => (Intrinsic::ResponseBodyUsed, vec![receiver]),
            (Kind::Response, "bytes", true) => (Intrinsic::ResponseBytes, vec![receiver]),
            (Kind::Response, "text", true) => (Intrinsic::ResponseText, vec![receiver]),
            (Kind::Response, "json", true) => (Intrinsic::ResponseJson, vec![receiver]),
            (Kind::Request, "method", false) => (Intrinsic::RequestMethod, vec![receiver]),
            (Kind::Request, "url", false) => (Intrinsic::RequestUrl, vec![receiver]),
            (Kind::Request, "headers", false) => (Intrinsic::RequestHeaders, vec![receiver]),
            (Kind::Request, "bodyUsed", false) => (Intrinsic::RequestBodyUsed, vec![receiver]),
            (Kind::Request, "bytes", true) => (Intrinsic::RequestBytes, vec![receiver]),
            (Kind::Request, "text", true) => (Intrinsic::RequestText, vec![receiver]),
            (Kind::Request, "json", true) => (Intrinsic::RequestJson, vec![receiver]),
            (Kind::Writable, "locked", false) => (Intrinsic::WritableLocked, vec![receiver]),
            (Kind::Writable, "getWriter", true) => (Intrinsic::WritableGetWriter, vec![receiver]),
            (Kind::Writer, "write", true) => {
                let [chunk] = args.unwrap_or(&[]) else {
                    bail!("write takes one chunk");
                };
                let (chunk, _) = self.body_init(ctx, Some(chunk))?;
                let chunk = self.defined(ctx, chunk)?;
                return Ok(self.intrinsic(ctx, Intrinsic::WriterWrite, vec![receiver, chunk]));
            }
            (Kind::Writer, "close", true) => (Intrinsic::WriterClose, vec![receiver]),
            (Kind::Readable, "locked", false) => (Intrinsic::ReadableLocked, vec![receiver]),
            (Kind::Readable, "getReader", true) => {
                let args = args.unwrap_or(&[]);
                if args.is_empty() {
                    (Intrinsic::ReadableGetReader, vec![receiver])
                } else {
                    ensure!(
                        args.len() == 1,
                        "getReader takes an optional {{mode: 'byob'}}"
                    );
                    let fields = self.init_fields(args.first())?;
                    ensure!(
                        fields.len() == 1
                            && fields[0].0 == "mode"
                            && matches!(fields[0].1, Expr::String(mode) if mode == "byob"),
                        "getReader supports only {{mode: 'byob'}}"
                    );
                    return Ok(self.intrinsic(
                        ctx,
                        Intrinsic::ReadableGetByobReader,
                        vec![receiver],
                    ));
                }
            }
            (Kind::Readable, "cancel", true) => (Intrinsic::ReadableCancel, vec![receiver]),
            (Kind::Reader, "read", true) => {
                ensure!(
                    args.is_some_and(<[Expr]>::is_empty),
                    "default reader.read takes no arguments"
                );
                (Intrinsic::ReaderRead, vec![receiver])
            }
            (Kind::ByobReader, "read", true) => {
                let args = args.unwrap_or(&[]);
                ensure!(
                    (1..=2).contains(&args.len()),
                    "BYOB read requires a Uint8Array and optional {{min}}"
                );
                let view = self.typed_operand(ctx, &args[0], Kind::Bytes, "a Uint8Array")?;
                let fields = self.init_fields(args.get(1))?;
                ensure!(
                    fields.is_empty() || (fields.len() == 1 && fields[0].0 == "min"),
                    "BYOB read supports only {{min}}"
                );
                let minimum = self.number_or(ctx, fields.first().map(|(_, value)| *value), 1.0)?;
                return Ok(self.intrinsic(
                    ctx,
                    Intrinsic::ReaderReadByob,
                    vec![receiver, view, minimum],
                ));
            }
            (Kind::Reader | Kind::ByobReader, "cancel", true) => {
                (Intrinsic::ReaderCancel, vec![receiver])
            }
            (Kind::Reader | Kind::ByobReader, "releaseLock", true) => {
                self.intrinsic_void(ctx, Intrinsic::ReaderReleaseLock, vec![receiver]);
                let number = FP64Type::get(ctx).into();
                return Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number)));
            }
            (Kind::ReadResult, "done", false) => (Intrinsic::ReadDone, vec![receiver]),
            (Kind::ReadResult, "value", false) => (Intrinsic::ReadValue, vec![receiver]),
            (Kind::Writer, "releaseLock", true) => {
                self.intrinsic_void(ctx, Intrinsic::WriterReleaseLock, vec![receiver]);
                let number = FP64Type::get(ctx).into();
                return Ok(self.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number)));
            }
            (_, name, _) => bail!("{name} is not supported on this type yet"),
        };
        if args.is_some_and(|args| !args.is_empty()) && !matches!(receiver_kind, Kind::Headers) {
            bail!("{name} takes no arguments");
        }
        Ok(self.intrinsic(ctx, intrinsic, operands))
    }

    /// `fetch(url, { method, headers, body })` as Perry HIR desugars a literal init.
    pub(super) fn fetch_with_options(
        &mut self,
        ctx: &mut Context,
        url: &'s Expr,
        method: &'s Expr,
        body: &'s Expr,
        headers: &'s [(String, Expr)],
        headers_dynamic: Option<&'s Expr>,
    ) -> Result<Value> {
        let url = self.url_string(ctx, url)?;
        let method = self.typed_operand(ctx, method, Kind::String, "a method string")?;
        let list = match headers_dynamic {
            Some(dynamic) => self.headers_init(ctx, Some(dynamic))?,
            None => self.header_list(
                ctx,
                headers.iter().map(|(name, value)| (name.as_str(), value)),
            )?,
        };
        let (body, text) = self.body_init(ctx, Some(body))?;
        let text = self.boolean(ctx, text);
        Ok(self.intrinsic(ctx, Intrinsic::Fetch, vec![url, method, list, body, text]))
    }

    /// `Writable.toWeb(process.stdout)` or of `process.stderr`.
    pub(super) fn writable_to_web(&mut self, ctx: &mut Context, args: &'s [Expr]) -> Result<Value> {
        let intrinsic = match args {
            [Expr::ProcessStdout] => Intrinsic::WritableStdout,
            [Expr::ProcessStderr] => Intrinsic::WritableStderr,
            _ => bail!("Writable.toWeb supports process.stdout and process.stderr"),
        };
        Ok(self.intrinsic(ctx, intrinsic, vec![]))
    }

    /// A member of a web class as Perry HIR presents it: a native method call, which is a
    /// property read for the getters when it has no arguments.
    pub(super) fn web_method(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        method: &str,
        args: &'s [Expr],
    ) -> Result<Value> {
        let receiver = self.expression(ctx, object)?;
        let receiver = self.defined(ctx, receiver)?;
        let call = !args.is_empty()
            || !matches!(
                method,
                "status"
                    | "statusText"
                    | "ok"
                    | "headers"
                    | "bodyUsed"
                    | "url"
                    | "method"
                    | "locked"
                    | "body"
            );
        ensure!(
            matches!(
                kind(ctx, receiver)?,
                Kind::Headers
                    | Kind::Response
                    | Kind::Request
                    | Kind::Writable
                    | Kind::Writer
                    | Kind::Readable
                    | Kind::Reader
                    | Kind::ByobReader
                    | Kind::ReadResult
            ),
            "{method} is not supported on this type yet"
        );
        self.web_member(ctx, receiver, method, call.then_some(args))
    }

    /// A `node:fs/promises` function: an operation whose promise settles from host events.
    pub(super) fn fs(
        &mut self,
        ctx: &mut Context,
        builtin: Builtin,
        args: &'s [Expr],
    ) -> Result<Value> {
        let path = args.first().context("a path is required")?;
        let path = self.typed_operand(ctx, path, Kind::String, "a path string")?;
        let (intrinsic, operands) = match builtin {
            Builtin::ReadFile => {
                let text = match args.get(1) {
                    None => false,
                    Some(Expr::String(encoding)) if is_utf8(encoding) => true,
                    // `{ encoding: 'utf8' }`: an object literal of one field.
                    Some(Expr::New {
                        class_name, args, ..
                    }) if matches!(args.as_slice(), [Expr::String(encoding)] if is_utf8(encoding))
                        && self
                            .shapes
                            .literal_fields(class_name)
                            .is_some_and(|names| names == ["encoding"]) =>
                    {
                        true
                    }
                    Some(_) => bail!("readFile supports no options but the 'utf8' encoding yet"),
                };
                let intrinsic = if text {
                    Intrinsic::FsReadText
                } else {
                    Intrinsic::FsReadBytes
                };
                (intrinsic, vec![path])
            }
            Builtin::WriteFile => {
                ensure!(args.len() == 2, "writeFile options are not supported yet");
                let data = self.expression(ctx, &args[1])?;
                let data = self.defined(ctx, data)?;
                let intrinsic = match kind(ctx, data)? {
                    Kind::String => Intrinsic::FsWriteText,
                    Kind::Bytes => Intrinsic::FsWriteBytes,
                    _ => bail!("writeFile writes strings and Uint8Arrays"),
                };
                (intrinsic, vec![path, data])
            }
            other => {
                ensure!(
                    args.len() == 1,
                    "{other:?} options are not supported yet (recursive, withFileTypes)"
                );
                let intrinsic = match other {
                    Builtin::Stat => Intrinsic::FsStat,
                    Builtin::Readdir => Intrinsic::FsReaddir,
                    Builtin::Mkdir => Intrinsic::FsMkdir,
                    Builtin::Unlink => Intrinsic::FsUnlink,
                    _ => Intrinsic::FsRmdir,
                };
                (intrinsic, vec![path])
            }
        };
        Ok(self.intrinsic(ctx, intrinsic, operands))
    }
}

fn is_utf8(encoding: &str) -> bool {
    matches!(encoding, "utf8" | "utf-8" | "UTF-8" | "UTF8")
}
