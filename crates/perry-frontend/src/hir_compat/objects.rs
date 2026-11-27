use super::{Diagnostic, Expr, Result, static_property};
use perry_hir::ir::ClassField;

/// Object fields retained across Perry's duplicate-key and literal-class desugarings.
pub(crate) fn object_literal_pairs<'s>(
    expr: &'s Expr,
    literal: Option<&'s [ClassField]>,
) -> Option<impl Iterator<Item = (&'s str, &'s Expr)> + use<'s>> {
    let (fields, defined, names, args) = match expr {
        Expr::Object(fields) => (&fields[..], &[][..], &[][..], &[][..]),
        Expr::ObjectSpread { parts }
            if parts.iter().all(|part| static_property(part).is_some()) =>
        {
            (&[][..], &parts[..], &[][..], &[][..])
        }
        Expr::New { args, .. } => {
            let fields = literal?;
            if fields.len() != args.len() {
                return None;
            }
            (&[][..], &[][..], fields, &args[..])
        }
        _ => return None,
    };
    Some(
        fields
            .iter()
            .map(|(name, value)| (name.as_str(), value))
            .chain(
                defined
                    .iter()
                    .filter_map(|part| Some((static_property(part)?, part.value()))),
            )
            .chain(names.iter().map(|field| field.name.as_str()).zip(args)),
    )
}

pub(crate) fn literal_option<'s>(
    expr: &'s Expr,
    literal: Option<&'s [ClassField]>,
    key: &str,
) -> Option<&'s Expr> {
    let mut fields = object_literal_pairs(expr, literal)?;
    let (name, value) = fields.next()?;
    (name == key && fields.next().is_none()).then_some(value)
}

pub(crate) fn read_file_text<'s>(
    option: Option<&'s Expr>,
    literal: Option<&'s [ClassField]>,
) -> Result<bool> {
    let Some(option) = option else {
        return Ok(false);
    };
    let encoding = literal_option(option, literal, "encoding").unwrap_or(option);
    if matches!(encoding, Expr::String(name) if matches!(name.as_str(), "utf8" | "utf-8" | "UTF-8" | "UTF8"))
    {
        return Ok(true);
    }
    Err(Diagnostic::unsupported(
        "readFile supports no options but the 'utf8' encoding yet",
    ))
}
