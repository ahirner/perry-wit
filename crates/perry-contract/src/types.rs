//! Borrowed WIT traversal with constructors for each contract representation.
use wit_parser::{Enum, Handle, Int, Resolve, Type, TypeDefKind, TypeId};

pub use perry_abi::layout::numeric::Integer;

macro_rules! wit_integers {
    ($($ty:ident => $name:literal, $domain:ident);* $(;)?) => {
        pub const INTEGERS: &[(&str, Type, Integer)] = &[$(($name, Type::$ty, Integer::$domain)),*];

        fn primitive<T>(ty: Type) -> Result<Contract<'static, T>, TypeId> {
            Ok(match ty {
                $(Type::$ty => Contract::Scalar(Scalar::Integer(Integer::$domain)),)*
                Type::Bool => Contract::Scalar(Scalar::Bool),
                Type::F32 => Contract::Scalar(Scalar::F32),
                Type::F64 => Contract::Scalar(Scalar::F64),
                Type::String => Contract::String,
                Type::Char => Contract::Char,
                Type::ErrorContext => Contract::ErrorContext,
                Type::Id(id) => return Err(id),
            })
        }
    };
}
wit_integers! {
    U8 => "u8", U8; S8 => "s8", I8;
    U16 => "u16", U16; S16 => "s16", I16;
    U32 => "u32", U32; S32 => "s32", I32;
    U64 => "u64", U64; S64 => "s64", I64;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scalar {
    Bool,
    Integer(Integer),
    F32,
    F64,
}

impl std::fmt::Display for Scalar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bool => f.write_str("bool"),
            Self::F32 => f.write_str("f32"),
            Self::F64 => f.write_str("f64"),
            Self::Integer(domain) => write!(
                f,
                "{}{}",
                if domain.signed() { "s" } else { "u" },
                domain.bits()
            ),
        }
    }
}

pub enum Contract<'a, T> {
    Scalar(Scalar),
    String,
    Char,
    ErrorContext,
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
    let id = match primitive(ty) {
        Ok(contract) => return contract,
        Err(id) => id,
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
