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
        hir_compat::init_fields(init, |name| self.shapes.literal_fields(name))
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
    pub(super) fn body_init(
        &mut self,
        ctx: &mut Context,
        body: Option<&'s Expr>,
    ) -> Result<(Value, bool)> {
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
        crate::methods::Call::member(crate::methods::Receiver::of(ctx, receiver)?, name, args)?
            .with_context(|| format!("{name} is not supported on this receiver"))?
            .lower(self, ctx, receiver)?
            .context("web members produce a value")
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
        let property = hir_compat::native_member_is_property(
            crate::methods::Receiver::of(ctx, receiver)?,
            method,
            args,
        );
        self.web_member(ctx, receiver, method, (!property).then_some(args))
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
