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
