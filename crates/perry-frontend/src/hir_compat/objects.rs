use anyhow::{Context as _, Result, bail, ensure};
use perry_hir::ir::Expr;

pub fn init_fields<'h, 'e>(
    init: Option<&'e Expr>,
    literal_fields: impl FnOnce(&str) -> Option<&'h [&'h str]>,
) -> Result<Vec<(&'h str, &'e Expr)>> {
    const NOT_LITERAL: &str = "init objects must be object literals yet";
    match init {
        None | Some(Expr::Undefined) => Ok(Vec::new()),
        Some(Expr::New {
            class_name, args, ..
        }) => {
            let names = literal_fields(class_name).context(NOT_LITERAL)?;
            ensure!(
                names.len() == args.len(),
                "init object arity differs from its shape"
            );
            Ok(names.iter().copied().zip(args).collect())
        }
        Some(_) => bail!(NOT_LITERAL),
    }
}
