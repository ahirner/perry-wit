//! Borrowed WIT traversal with constructors for each contract representation.
use wit_parser::{Handle, Resolve, Type, TypeDefKind, TypeId};

type Children<'a, T> = &'a mut dyn Iterator<Item = T>;

pub enum Contract<'a, T> {
    Scalar(Type),
    Resource { id: TypeId, owned: bool },
    Bytes,
    ByteStream,
    List(T),
    Option(T),
    Result { ok: Option<T>, err: Option<T> },
    Tuple(Children<'a, T>),
    Record(Children<'a, (String, T)>),
    Enum(&'a mut dyn Iterator<Item = &'a str>),
    Variant(Children<'a, (&'a str, Option<T>)>),
    Unsupported(&'static str),
}

pub enum Input {
    Reference(Type),
    Definition(TypeId),
}

pub fn convert<T>(
    resolve: &Resolve,
    input: Input,
    reference: &impl Fn(TypeId) -> Option<T>,
    construct: &impl Fn(Contract<'_, T>) -> T,
) -> T {
    let id = match input {
        Input::Reference(Type::Id(id)) => {
            if let Some(named) = reference(id) {
                return named;
            }
            id
        }
        Input::Reference(ty) => return construct(Contract::Scalar(ty)),
        Input::Definition(id) => id,
    };
    let child = |ty: &Type| convert(resolve, Input::Reference(*ty), reference, construct);
    construct(match &resolve.types[id].kind {
        TypeDefKind::Type(ty) => return child(ty),
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
            ok: result.ok.as_ref().map(child),
            err: result.err.as_ref().map(child),
        },
        TypeDefKind::Tuple(tuple) => {
            return construct(Contract::Tuple(&mut tuple.types.iter().map(child)));
        }
        TypeDefKind::Record(record) => {
            return construct(Contract::Record(&mut record.fields.iter().map(|field| {
                (crate::names::to_camel_case(&field.name), child(&field.ty))
            })));
        }
        TypeDefKind::Variant(variant) => {
            return construct(Contract::Variant(
                &mut variant
                    .cases
                    .iter()
                    .map(|case| (case.name.as_str(), case.ty.as_ref().map(child))),
            ));
        }
        TypeDefKind::Enum(enumeration) => {
            return construct(Contract::Enum(
                &mut enumeration.cases.iter().map(|case| case.name.as_str()),
            ));
        }
        TypeDefKind::Stream(Some(Type::U8)) => Contract::ByteStream,
        other => Contract::Unsupported(other.as_str()),
    })
}
