//! Module-bound filesystem calls; their native results are promises, including failures.
use super::*;
use perry_hir::ir::ImportSpecifier;

enum Binding {
    Function(String),
    Namespace,
}
pub(super) struct Bindings(HashMap<String, Binding>);
impl Bindings {
    pub(super) fn new(hir: &HirModule) -> Self {
        Self(
            hir.imports
                .iter()
                .filter(|import| {
                    matches!(import.source.as_str(), "fs/promises" | "node:fs/promises")
                })
                .flat_map(|import| &import.specifiers)
                .map(|specifier| match specifier {
                    ImportSpecifier::Named { imported, local } => {
                        (local.clone(), Binding::Function(imported.clone()))
                    }
                    ImportSpecifier::Default { local } | ImportSpecifier::Namespace { local } => {
                        (local.clone(), Binding::Namespace)
                    }
                })
                .collect(),
        )
    }
    pub(super) fn operation<'a>(&'a self, callee: &'a Expr) -> Option<&'a str> {
        match callee {
            Expr::ExternFuncRef { name, .. } => match self.0.get(name)? {
                Binding::Function(name) => Some(name),
                _ => None,
            },
            Expr::PropertyGet {
                object, property, ..
            } => match object.as_ref() {
                Expr::ExternFuncRef { name, .. }
                    if matches!(self.0.get(name), Some(Binding::Namespace)) =>
                {
                    Some(property)
                }
                _ => None,
            },
            _ => None,
        }
    }
}
impl FunctionBuilder<'_> {
    pub(super) fn filesystem_call(
        &mut self,
        ctx: &mut Context,
        operation: &str,
        args: &[Expr],
    ) -> Result<Value> {
        if matches!(operation, "readFile" | "writeFile" | "readdir") {
            return self.filesystem_transfer(ctx, operation, args);
        }
        let intrinsic = match operation {
            "stat" => Intrinsic::FsStat,
            "mkdir" => Intrinsic::FsMkdir,
            "unlink" => Intrinsic::FsUnlink,
            "rmdir" => Intrinsic::FsRmdir,
            _ => bail!("filesystem operation {operation} is not supported yet"),
        };
        ensure!(
            !args.is_empty() && args.len() <= 2,
            "filesystem {operation} takes a path and optional literal options"
        );
        let path = self.expression(ctx, &args[0])?;
        let names = if operation == "stat" {
            &["bigint", "throwIfNoEntry"][..]
        } else {
            &["recursive"][..]
        };
        let options = self.literal_options(ctx, args.get(1), names)?;
        let unsupported = self.boolean_option(ctx, options[0], false)?;
        let yes = self.boolean(ctx, true);
        let mut valid = self.compare(ctx, CompareKind::Ne, unsupported, yes);
        if operation == "stat" {
            let required = self.boolean_option(ctx, options[1], true)?;
            let result = self.entry_slot(ctx, valid.get_type(ctx));
            self.store(ctx, result, valid);
            let check = self.new_block(ctx, "stat_options");
            let join = self.new_block(ctx, "stat_options_done");
            self.branch(ctx, valid, check, join);
            self.switch_to(check);
            self.store(ctx, result, required);
            self.jump(ctx, join);
            self.switch_to(join);
            valid = self.load(ctx, result)?;
        }
        let string = StringType::get(ctx).into();
        let path = self.fit(ctx, path, string)?;
        Ok(self.intrinsic(ctx, intrinsic, vec![path, valid]).unwrap())
    }
}

impl FunctionBuilder<'_> {
    fn filesystem_transfer(
        &mut self,
        ctx: &mut Context,
        operation: &str,
        args: &[Expr],
    ) -> Result<Value> {
        let writing = operation == "writeFile";
        let position = if writing { 2 } else { 1 };
        ensure!(
            args.len() >= position && args.len() <= position + 1,
            "filesystem {operation} takes a path, data for writes, and optional options"
        );
        let path = self.expression(ctx, &args[0])?;
        let data = if writing {
            Some(self.expression(ctx, &args[1])?)
        } else {
            None
        };
        let fields: Vec<(String, &Expr)> = match args.get(position) {
            None | Some(Expr::Null | Expr::Undefined) => vec![],
            Some(Expr::Object(fields)) => fields
                .iter()
                .map(|(name, value)| (name.clone(), value))
                .collect(),
            Some(Expr::New {
                class_name, args, ..
            }) => self
                .shapes
                .literal_fields(class_name)
                .context("filesystem options must have plain data fields")?
                .into_iter()
                .zip(args)
                .collect(),
            Some(value) => vec![("encoding".into(), value)],
        };
        let mut encoding = None;
        let mut flag = None;
        let mut recursive = None;
        let mut file_types = None;
        let mut valid = true;
        let mut text_read = Some(false);
        for (name, expression) in fields {
            match name.as_str() {
                "encoding" => {
                    if operation == "readFile" {
                        text_read = match expression {
                            Expr::String(value) => Some(matches!(
                                value.to_ascii_lowercase().as_str(),
                                "utf8" | "utf-8"
                            )),
                            Expr::Null | Expr::Undefined => Some(false),
                            _ => None,
                        };
                    }
                    encoding = if matches!(expression, Expr::Null) {
                        None
                    } else {
                        self.optional_argument(ctx, Some(expression))?
                    };
                }
                "flag" if operation != "readdir" => {
                    flag = self.optional_argument(ctx, Some(expression))?
                }
                "recursive" if operation == "readdir" => {
                    recursive = self.optional_argument(ctx, Some(expression))?
                }
                "withFileTypes" if operation == "readdir" => {
                    file_types = self.optional_argument(ctx, Some(expression))?
                }
                "signal"
                    if operation != "readdir"
                        && matches!(expression, Expr::Null | Expr::Undefined) => {}
                _ => {
                    self.effect(ctx, expression)?;
                    valid = false;
                }
            }
        }
        let string = StringType::get(ctx).into();
        let encoding = self.fit_optional(ctx, encoding, string)?;
        let flag = self.fit_optional(ctx, flag, string)?;
        let recursive = self.boolean_option(ctx, recursive, false)?;
        let file_types = self.boolean_option(ctx, file_types, false)?;
        let mut allowed = self.boolean(ctx, valid);
        for unsupported in [recursive, file_types] {
            let join = self.new_block(ctx, "filesystem_options_done");
            let accept = self.new_block(ctx, "filesystem_options_accept");
            let result = self.entry_slot(ctx, allowed.get_type(ctx));
            let no = self.boolean(ctx, false);
            self.store(ctx, result, no);
            self.branch(ctx, unsupported, join, accept);
            self.switch_to(accept);
            self.store(ctx, result, allowed);
            self.jump(ctx, join);
            self.switch_to(join);
            allowed = self.load(ctx, result)?;
        }
        let (intrinsic, mode) = match (operation, data.map(|value| kind(ctx, value)).transpose()?) {
            ("readFile", _) if text_read == Some(true) => (Intrinsic::FsReadText, 1.0),
            ("readFile", _) if text_read.is_none() => (Intrinsic::FsReadValue, 5.0),
            ("readFile", _) => (Intrinsic::FsReadBytes, 0.0),
            ("readdir", _) => (Intrinsic::FsReadDir, 4.0),
            ("writeFile", Some(Kind::String)) => (Intrinsic::FsWriteText, 2.0),
            ("writeFile", Some(Kind::TextOrBytes)) => (Intrinsic::FsWriteValue, 0.0),
            ("writeFile", Some(Kind::Bytes)) => (Intrinsic::FsWriteBytes, 3.0),
            _ => bail!("writeFile data must be a string or Uint8Array"),
        };
        let mode = if intrinsic == Intrinsic::FsWriteValue {
            let text = self
                .intrinsic(ctx, Intrinsic::TextBytesIsText, vec![data.unwrap()])
                .unwrap();
            let number = FP64Type::get(ctx).into();
            let slot = self.entry_slot(ctx, number);
            let then = self.new_block(ctx, "write_text_options");
            let otherwise = self.new_block(ctx, "write_bytes_options");
            let done = self.new_block(ctx, "write_options_done");
            self.branch(ctx, text, then, otherwise);
            self.switch_to(then);
            let mode = self.number(ctx, 2.0);
            self.store(ctx, slot, mode);
            self.jump(ctx, done);
            self.switch_to(otherwise);
            let mode = self.number(ctx, 3.0);
            self.store(ctx, slot, mode);
            self.jump(ctx, done);
            self.switch_to(done);
            self.load(ctx, slot)?
        } else {
            self.number(ctx, mode)
        };
        let valid = self
            .intrinsic(
                ctx,
                Intrinsic::FsOptions,
                vec![encoding, flag, mode, allowed],
            )
            .unwrap();
        let path = self.fit(ctx, path, string)?;
        let mut operands = vec![path];
        if intrinsic == Intrinsic::FsReadValue {
            operands.push(encoding);
        }
        if let Some(data) = data {
            operands.push(data);
        }
        operands.push(valid);
        Ok(self.intrinsic(ctx, intrinsic, operands).unwrap())
    }
}
