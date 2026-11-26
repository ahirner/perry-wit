use super::{Diagnostic, Expr, Result, static_property};
use crate::source::LiteralClasses;

/// Object fields retained across Perry's duplicate-key and literal-class desugarings.
pub(crate) fn object_literal_pairs<'s>(
    expr: &'s Expr,
    literals: &LiteralClasses<'s>,
) -> Option<impl Iterator<Item = (&'s str, &'s Expr)> + use<'s>> {
    let (fields, defined, names, args) = match expr {
        Expr::Object(fields) => (&fields[..], &[][..], &[][..], &[][..]),
        Expr::ObjectSpread { parts }
            if parts.iter().all(|part| static_property(part).is_some()) =>
        {
            (&[][..], &parts[..], &[][..], &[][..])
        }
        Expr::New {
            class_name, args, ..
        } => {
            let fields = *literals.get(class_name.as_str())?;
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
    literals: &LiteralClasses<'s>,
    key: &str,
) -> Option<&'s Expr> {
    let mut fields = object_literal_pairs(expr, literals)?;
    let (name, value) = fields.next()?;
    (name == key && fields.next().is_none()).then_some(value)
}

pub(crate) fn read_file_text<'s>(
    option: Option<&'s Expr>,
    literals: &LiteralClasses<'s>,
) -> Result<bool> {
    let Some(option) = option else {
        return Ok(false);
    };
    let encoding = literal_option(option, literals, "encoding").unwrap_or(option);
    if matches!(encoding, Expr::String(name) if matches!(name.as_str(), "utf8" | "utf-8" | "UTF-8" | "UTF8"))
    {
        return Ok(true);
    }
    Err(Diagnostic::unsupported(
        "readFile supports no options but the 'utf8' encoding yet",
    ))
}
