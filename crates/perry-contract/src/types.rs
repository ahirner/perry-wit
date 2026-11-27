//! Borrowed WIT traversal with constructors for each contract representation.
use wit_parser::{Enum, Handle, Int, Resolve, Type, TypeDefKind, TypeId};

pub enum Contract<'a, T> {
    Scalar(Type),
    Alias(T),
    Resource { id: TypeId, owned: bool },
    Bytes,
    ByteStream,
    List(T),
    Option(T),
    Result { ok: Option<T>, err: Option<T> },
    Tuple(Vec<T>),
    Record(Vec<(String, T)>),
    Enum(&'a Enum),
    Variant(Int, Vec<(&'a str, Option<T>)>),
    Unsupported(&'static str),
}

pub enum Input {
    Reference(Type),
    Definition(TypeId),
}

pub fn convert<S, T>(
    resolve: &Resolve,
    input: Input,
    state: &mut S,
    reference: &impl Fn(&mut S, TypeId) -> Option<T>,
    construct: &impl Fn(&mut S, Contract<'_, T>) -> T,
) -> T {
    let ty = match input {
        Input::Reference(Type::Id(id)) if let Some(named) = reference(state, id) => return named,
        Input::Reference(ty) => ty,
        Input::Definition(id) => Type::Id(id),
    };
    let contract = project(resolve, ty, |ty| {
        convert(resolve, Input::Reference(*ty), state, reference, construct)
    });
    construct(state, contract)
}

/// Classify one WIT node, mapping its immediate children in declaration order.
pub fn project<T>(
    resolve: &Resolve,
    ty: Type,
    mut child: impl FnMut(&Type) -> T,
) -> Contract<'_, T> {
    let Type::Id(id) = ty else {
        return Contract::Scalar(ty);
    };
    match &resolve.types[id].kind {
        TypeDefKind::Type(ty) => Contract::Alias(child(ty)),
        TypeDefKind::Resource => Contract::Resource { id, owned: true },
        TypeDefKind::Handle(handle) => Contract::Resource {
            id: match handle {
                Handle::Own(id) | Handle::Borrow(id) => *id,
            },
            owned: matches!(handle, Handle::Own(_)),
        },
        TypeDefKind::List(Type::U8) => Contract::Bytes,
        TypeDefKind::List(ty) => Contract::List(child(ty)),
        TypeDefKind::Option(ty) => Contract::Option(child(ty)),
        TypeDefKind::Result(result) => Contract::Result {
            ok: result.ok.as_ref().map(&mut child),
            err: result.err.as_ref().map(child),
        },
        TypeDefKind::Tuple(tuple) => Contract::Tuple(tuple.types.iter().map(child).collect()),
        TypeDefKind::Record(record) => Contract::Record(
            record
                .fields
                .iter()
                .map(|field| (crate::names::to_camel_case(&field.name), child(&field.ty)))
                .collect(),
        ),
        TypeDefKind::Variant(variant) => Contract::Variant(
            variant.tag(),
            variant
                .cases
                .iter()
                .map(|case| (case.name.as_str(), case.ty.as_ref().map(&mut child)))
                .collect(),
        ),
        TypeDefKind::Enum(e) => Contract::Enum(e),
        TypeDefKind::Stream(Some(Type::U8)) => Contract::ByteStream,
        other => Contract::Unsupported(other.as_str()),
    }
}
