use anyhow::Result;
use perry_hir::ir::Expr;
use perry_wit_diagnostics::{bail_unsupported, ensure_internal, unsupported};

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
            let names = literal_fields(class_name).ok_or_else(|| unsupported!("{NOT_LITERAL}"))?;
            ensure_internal!(
                names.len() == args.len(),
                "init object arity differs from its shape"
            );
            Ok(names.iter().copied().zip(args).collect())
        }
        Some(_) => bail_unsupported!("{NOT_LITERAL}"),
    }
}
