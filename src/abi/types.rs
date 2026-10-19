//! Borrowed WIT traversal with constructors for each contract representation.
use wit_parser::{Handle, Resolve, Type, TypeDefKind, TypeId};

type Children<'a, T> = &'a mut dyn Iterator<Item = T>;

pub(crate) enum Contract<'a, T> {
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

pub(crate) enum Input {
    Reference(Type),
    Definition(TypeId),
}

pub(crate) fn convert<T>(
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
            return construct(Contract::Record(
                &mut record
                    .fields
                    .iter()
                    .map(|field| (crate::sdk::to_camel_case(&field.name), child(&field.ty))),
            ));
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

// Each expansion constructs only the requested representation.
macro_rules! primitive {
    ($ty:expr, $output:ident) => {
        match $ty {
            wit_parser::Type::Bool => primitive!(@$output Bool, Boolean, "boolean"),
            wit_parser::Type::U8 => primitive!(@$output U8, "u8", "number"),
            wit_parser::Type::S8 => primitive!(@$output S8, "s8", "number"),
            wit_parser::Type::U16 => primitive!(@$output U16, "u16", "number"),
            wit_parser::Type::S16 => primitive!(@$output S16, "s16", "number"),
            wit_parser::Type::U32 => primitive!(@$output U32, "u32", "number"),
            wit_parser::Type::S32 => primitive!(@$output S32, "s32", "number"),
            wit_parser::Type::U64 => primitive!(@$output U64, "u64", "bigint"),
            wit_parser::Type::S64 => primitive!(@$output S64, "s64", "bigint"),
            wit_parser::Type::F32 => primitive!(@$output F32, Number, "number"),
            wit_parser::Type::F64 => primitive!(@$output F64, Number, "number"),
            wit_parser::Type::String => primitive!(@$output String, String, "string"),
            wit_parser::Type::Char => primitive!(@$output Unsupported, String, "string"),
            wit_parser::Type::ErrorContext => primitive!(@$output Unsupported, Unsupported, "Error | unknown"),
            wit_parser::Type::Id(_) => primitive!(@$output Unsupported, Unsupported, "unknown"),
        }
    };
    (@abi Unsupported, $hir:tt, $ts:literal) => { None };
    (@abi $abi:ident, $hir:tt, $ts:literal) => { Some(perry_codegen::boundary::AbiType::$abi) };
    (@hir $abi:ident, Unsupported, $ts:literal) => { None };
    (@hir $abi:ident, $name:literal, $ts:literal) => {
        Some(perry_lower::hir::Type::Named(concat!("perry:types/", $name).into()))
    };
    (@hir $abi:ident, $hir:ident, $ts:literal) => { Some(perry_lower::hir::Type::$hir) };
    (@ts $abi:ident, $hir:tt, $ts:literal) => { $ts };
}
pub(crate) use primitive;
