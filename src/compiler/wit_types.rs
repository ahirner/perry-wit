//! WIT types as HIR types under their SDK names, for programs that import the SDK's generated
//! declarations (which the parser does not read). The mapping mirrors the SDK's TypeScript:
//! records are object types, variants unions of `{ tag, val }` objects, options
//! `T | null | undefined`, tuples tuples and `list<u8>` a `Uint8Array`.
use crate::sdk::{to_camel_case, to_pascal_case, type_name};
use perry_lower::hir::{ObjectType, PropertyInfo, Type as HirType};
use std::collections::HashMap;
use wit_parser::{Resolve, Type, TypeDefKind};

/// Every named WIT type of `resolve` that has a HIR equivalent, by SDK name prefixed with
/// `wit:` (references between them use these names too), so that only imported names reach
/// them and a WIT `headers` does not shadow the `Headers` class.
pub(crate) fn foreign(resolve: &Resolve) -> HashMap<String, HirType> {
    let mut types: HashMap<String, HirType> = resolve
        .types
        .iter()
        .filter(|(_, definition)| definition.name.is_some())
        // A `use` makes an alias with the declaration's SDK name: keep the declaration.
        .filter(|(id, definition)| {
            !matches!(definition.kind, TypeDefKind::Type(Type::Id(inner))
                if resolve.types[inner].name.is_some()
                    && type_name(resolve, inner) == type_name(resolve, *id))
        })
        .filter_map(|(id, definition)| {
            Some((
                foreign_name(resolve, id),
                if matches!(definition.kind, TypeDefKind::Resource) {
                    super::resources::source_type(resolve, id)
                } else {
                    kind(resolve, &definition.kind)?
                },
            ))
        })
        .collect();
    for (id, interface) in &resolve.interfaces {
        let Some(module) = resolve.id_of(id) else {
            continue;
        };
        for (name, id) in &interface.types {
            let target = foreign_name(resolve, *id);
            if types.contains_key(&target) {
                types.insert(
                    format!("wit:{module}#{}", to_pascal_case(name)),
                    HirType::Named(target),
                );
            }
        }
    }
    let inputs = types
        .keys()
        .map(|name| {
            (
                format!("{name}Input"),
                HirType::Generic {
                    base: "WitInput".into(),
                    type_args: vec![HirType::Named(name.clone())],
                },
            )
        })
        .collect::<Vec<_>>();
    types.extend(inputs);
    types
}

fn foreign_name(resolve: &Resolve, id: wit_parser::TypeId) -> String {
    format!("wit:{}", type_name(resolve, id))
}

fn object(fields: impl IntoIterator<Item = (String, HirType)>) -> HirType {
    HirType::Object(ObjectType {
        properties: fields
            .into_iter()
            .map(|(name, ty)| {
                let info = PropertyInfo {
                    ty,
                    optional: false,
                    readonly: false,
                };
                (name, info)
            })
            .collect(),
        ..ObjectType::default()
    })
}

pub(super) fn ty(resolve: &Resolve, ty: &Type) -> Option<HirType> {
    Some(match ty {
        Type::Bool => HirType::Boolean,
        Type::U8
        | Type::U16
        | Type::U32
        | Type::S8
        | Type::S16
        | Type::S32
        | Type::F32
        | Type::F64 => HirType::Number,
        Type::Char | Type::String => HirType::String,
        Type::U64 | Type::S64 => HirType::BigInt,
        Type::ErrorContext => return None,
        Type::Id(id) => {
            let definition = &resolve.types[*id];
            if definition.name.is_some() {
                HirType::Named(foreign_name(resolve, *id))
            } else {
                kind(resolve, &definition.kind)?
            }
        }
    })
}

fn kind(resolve: &Resolve, kind: &TypeDefKind) -> Option<HirType> {
    Some(match kind {
        TypeDefKind::Type(inner) => ty(resolve, inner)?,
        TypeDefKind::Handle(wit_parser::Handle::Own(id) | wit_parser::Handle::Borrow(id)) => {
            super::resources::source_type(resolve, *id)
        }
        TypeDefKind::Record(record) => object(
            record
                .fields
                .iter()
                .map(|field| Some((to_camel_case(&field.name), ty(resolve, &field.ty)?)))
                .collect::<Option<Vec<_>>>()?,
        ),
        TypeDefKind::Variant(variant) => HirType::Union(
            variant
                .cases
                .iter()
                .map(|case| {
                    let mut fields =
                        vec![("tag".to_string(), HirType::StringLiteral(case.name.clone()))];
                    if let Some(payload) = &case.ty {
                        fields.push(("val".to_string(), ty(resolve, payload)?));
                    }
                    Some(object(fields))
                })
                .collect::<Option<_>>()?,
        ),
        TypeDefKind::Enum(enumeration) => HirType::Union(
            enumeration
                .cases
                .iter()
                .map(|case| HirType::StringLiteral(case.name.clone()))
                .collect(),
        ),
        TypeDefKind::Result(result) => HirType::Union(
            [
                ("value", result.ok.as_ref()),
                ("error", result.err.as_ref()),
            ]
            .into_iter()
            .map(|(name, payload)| {
                let mut fields = vec![("ok".into(), HirType::Boolean)];
                if let Some(payload) = payload {
                    fields.push((name.into(), ty(resolve, payload)?));
                }
                Some(object(fields))
            })
            .collect::<Option<_>>()?,
        ),
        TypeDefKind::Option(inner) => {
            HirType::Union(vec![ty(resolve, inner)?, HirType::Null, HirType::Void])
        }
        TypeDefKind::List(Type::U8) => HirType::Named("Uint8Array".into()),
        TypeDefKind::List(element) => HirType::Array(Box::new(ty(resolve, element)?)),
        TypeDefKind::Tuple(tuple) => HirType::Tuple(
            tuple
                .types
                .iter()
                .map(|element| ty(resolve, element))
                .collect::<Option<_>>()?,
        ),
        TypeDefKind::Stream(Some(Type::U8)) => HirType::Generic {
            base: "ReadableStream".into(),
            type_args: vec![HirType::Named("Uint8Array".into())],
        },
        _ => return None,
    })
}
