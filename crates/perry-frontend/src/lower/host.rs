//! Host capabilities: timers, `fetch` and the web stream/HTTP classes, and `node:fs/promises`.
use super::*;

impl<'s> FunctionBuilder<'s, '_> {
    /// `setTimeout(delay?, value?)` from `node:timers/promises`.
    pub(super) fn timer(&mut self, ctx: &mut Context, args: &'s [Expr]) -> Result<Value> {
        ensure_diag!(
            args.len() <= 3,
            codes::TYPE_MISMATCH,
            "setTimeout takes a delay, a value and options"
        );
        let delay = match args.first() {
            Some(delay) => {
                let value = self.expression(ctx, delay)?;
                let value = self.arithmetic(ctx, value);
                ensure_diag!(
                    kind(ctx, value)? == Kind::Number,
                    codes::TYPE_MISMATCH,
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
                let signal = hir_compat::literal_option(
                    options,
                    &self.shapes.specialization.literal_classes,
                    "signal",
                )
                .ok_or_else(|| unsupported!("setTimeout options must be a literal {{ signal }}"))?;
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
            ensure_diag!(
                args.len() <= 1,
                codes::TYPE_MISMATCH,
                "new Headers takes one init"
            );
            return self.headers_init(ctx, args.first());
        }
        let (first, init) = (args.first(), args.get(1));
        ensure_diag!(
            args.len() <= 2,
            codes::TYPE_MISMATCH,
            "{class} takes two arguments"
        );
        let fields = self.init_fields(init)?;
        let field = |name: &str| fields.iter().find(|(n, _)| *n == name).map(|(_, e)| *e);
        for (name, _) in &fields {
            let known: &[&str] = if class == "Response" {
                &["status", "statusText", "headers"]
            } else {
                &["method", "headers", "body"]
            };
            ensure_unsupported!(
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
        let url = first
            .ok_or_else(|| Diagnostic::new(codes::TYPE_MISMATCH, "new Request needs a URL"))?;
        let url = self.typed_operand(ctx, url, Kind::String, "a URL string")?;
        let method = match field("method") {
            Some(expr) => self.typed_operand(ctx, expr, Kind::String, "a method string")?,
            None => self.string(ctx, "GET"),
        };
        let operands = vec![url, method, body, text, headers];
        Ok(self.intrinsic(ctx, Intrinsic::RequestNew, operands))
    }

    pub(super) fn object_literal_pairs(
        &self,
        expr: &'s Expr,
    ) -> Option<impl Iterator<Item = (&'s str, &'s Expr)> + use<'s>> {
        crate::hir_compat::object_literal_pairs(expr, &self.shapes.specialization.literal_classes)
    }

    /// The fields of an init object literal, by name.
    pub(super) fn init_fields(&self, init: Option<&'s Expr>) -> Result<Vec<(&'s str, &'s Expr)>> {
        match init {
            None | Some(Expr::Undefined) => Ok(Vec::new()),
            Some(expr @ Expr::New { .. }) if let Some(fields) = self.object_literal_pairs(expr) => {
                Ok(fields.collect())
            }
            Some(_) => bail_unsupported!("init objects must be object literals yet"),
        }
    }

    pub(super) fn fetch(&mut self, ctx: &mut Context, args: &'s [Expr]) -> Result<Value> {
        ensure_diag!(
            (1..=2).contains(&args.len()),
            codes::TYPE_MISMATCH,
            "fetch takes a URL and optional init"
        );
        let url = self.url_string(ctx, &args[0])?;
        let fields = self.init_fields(args.get(1))?;
        for (name, _) in &fields {
            ensure_unsupported!(
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
            ensure_unsupported!(
                kind(ctx, value)? == Kind::Headers,
                "headers init must be an object literal or Headers"
            );
            return Ok(self.intrinsic(ctx, Intrinsic::HeadersCopy, vec![value]));
        }
        let fields = self.init_fields(init)?;
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
            _ => bail_unsupported!("bodies are strings and Uint8Arrays yet"),
        };
        Ok((self.optional(ctx, value), text))
    }

    /// `Writable.toWeb(process.stdout)` or of `process.stderr`.
    pub(super) fn writable_to_web(&mut self, ctx: &mut Context, args: &'s [Expr]) -> Result<Value> {
        let intrinsic = match args {
            [Expr::ProcessStdout] => Intrinsic::WritableStdout,
            [Expr::ProcessStderr] => Intrinsic::WritableStderr,
            _ => bail_unsupported!("Writable.toWeb supports process.stdout and process.stderr"),
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
        crate::methods::Method::lookup(crate::methods::Receiver::of(ctx, receiver)?, method)
            .filter(|m| !m.is_property() || args.is_empty())
            .ok_or_else(|| unsupported!("{method} is not supported on this receiver"))?
            .bind(method, args)?
            .lower(self, ctx, receiver)?
            .ok_or_else(|| Diagnostic::new(codes::TYPE_MISMATCH, "web members produce a value"))
    }

    fn fs_path(&mut self, ctx: &mut Context, args: &'s [Expr]) -> Result<Value> {
        let path = args
            .first()
            .ok_or_else(|| Diagnostic::new(codes::TYPE_MISMATCH, "a path is required"))?;
        self.typed_operand(ctx, path, Kind::String, "a path string")
    }

    pub(super) fn read_file(
        &mut self,
        ctx: &mut Context,
        args: &'s [Expr],
        text: bool,
    ) -> Result<Value> {
        let path = self.fs_path(ctx, args)?;
        let intrinsic = if text {
            Intrinsic::FsReadText
        } else {
            Intrinsic::FsReadBytes
        };
        Ok(self.intrinsic(ctx, intrinsic, vec![path]))
    }

    pub(super) fn fs(
        &mut self,
        ctx: &mut Context,
        builtin: Builtin,
        args: &'s [Expr],
    ) -> Result<Value> {
        let path = self.fs_path(ctx, args)?;
        let (intrinsic, operands) = match builtin {
            Builtin::WriteFile => {
                ensure_unsupported!(args.len() == 2, "writeFile options are not supported yet");
                let data = self.expression(ctx, &args[1])?;
                let data = self.defined(ctx, data)?;
                let intrinsic = match kind(ctx, data)? {
                    Kind::String => Intrinsic::FsWriteText,
                    Kind::Bytes => Intrinsic::FsWriteBytes,
                    _ => bail_unsupported!("writeFile writes strings and Uint8Arrays"),
                };
                (intrinsic, vec![path, data])
            }
            other => {
                ensure_unsupported!(
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
