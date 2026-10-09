//! Component contracts shared by the compiler and SDK.

pub mod export_names;

pub(crate) fn unalias(resolve: &wit_parser::Resolve, mut ty: wit_parser::Type) -> wit_parser::Type {
    while let wit_parser::Type::Id(id) = ty {
        match resolve.types[id].kind {
            wit_parser::TypeDefKind::Type(inner) => ty = inner,
            _ => break,
        }
    }
    ty
}

/// Distinct string and byte-list payloads select their case without a tagged TS object.
pub(crate) fn text_bytes_order(
    resolve: &wit_parser::Resolve,
    variant: &wit_parser::Variant,
) -> Option<perry_codegen::boundary::TextBytesOrder> {
    use perry_codegen::boundary::TextBytesOrder;
    use wit_parser::{Type, TypeDefKind};
    let [first, second] = variant.cases.as_slice() else {
        return None;
    };
    let bytes = |ty| match ty {
        Type::Id(id) => matches!(
            resolve.types[id].kind,
            TypeDefKind::List(element) if unalias(resolve, element) == Type::U8
        ),
        _ => false,
    };
    match (unalias(resolve, first.ty?), unalias(resolve, second.ty?)) {
        (Type::String, other) if bytes(other) => Some(TextBytesOrder::TextFirst),
        (other, Type::String) if bytes(other) => Some(TextBytesOrder::BytesFirst),
        _ => None,
    }
}

pub(crate) fn result_type(
    resolve: &wit_parser::Resolve,
    ty: wit_parser::Type,
) -> Option<&wit_parser::Result_> {
    if let wit_parser::Type::Id(id) = unalias(resolve, ty)
        && let wit_parser::TypeDefKind::Result(result) = &resolve.types[id].kind
    {
        return Some(result);
    }
    None
}
