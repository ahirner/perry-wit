//! Component contracts shared by the compiler and SDK.

pub mod export_names;

pub(crate) fn result_type(
    resolve: &wit_parser::Resolve,
    mut ty: wit_parser::Type,
) -> Option<&wit_parser::Result_> {
    while let wit_parser::Type::Id(id) = ty {
        match &resolve.types[id].kind {
            wit_parser::TypeDefKind::Type(inner) => ty = *inner,
            wit_parser::TypeDefKind::Result(result) => return Some(result),
            _ => break,
        }
    }
    None
}
