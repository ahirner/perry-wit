//! WIT types as HIR types under their SDK names, for programs that import the SDK's generated
//! declarations (which the parser does not read). The mapping mirrors the SDK's TypeScript:
//! records are object types, variants unions of `{ tag, val }` objects, options
//! `T | null`, tuples tuples and `list<u8>` a `Uint8Array`.
use crate::sdk::{
    collect_world_types, declaration_id, qualified_type_name, to_camel_case, to_pascal_case,
    type_name, world_type_name,
};
use perry_lower::hir::{Foreign, ObjectType, PropertyInfo, Signedness, Type as HirType};
use std::collections::HashMap;
use wit_parser::{Resolve, Type, TypeDefKind, WorldId};

/// Every named WIT type of `resolve` that has a HIR equivalent, registered under its
/// qualified SDK name, world-scoped SDK name (when non-colliding), and `module#Name`
/// interface binding.
pub(crate) fn foreign(resolve: &Resolve, world: Option<WorldId>) -> HashMap<String, Foreign> {
    let world_types = world.map(|w| collect_world_types(resolve, &resolve.worlds[w]));
    let mut types = HashMap::new();
    for (id, definition) in &resolve.types {
        if definition.name.is_none() || declaration_id(resolve, id) != id {
            continue;
        }
        let entry = if matches!(definition.kind, TypeDefKind::Resource) {
            Foreign::Resource(super::resources::canonical(resolve, id).index() as u32)
        } else {
            let Some(ty) = kind(resolve, &definition.kind) else {
                continue;
            };
            Foreign::Type(ty)
        };
        let qualified = qualified_type_name(resolve, id);
        let input_entry =
            Foreign::Input(Box::new(Foreign::Type(HirType::Named(qualified.clone()))));
        types.insert(qualified.clone(), entry);
        types.insert(format!("{qualified}Input"), input_entry);
        for alias in [
            Some(type_name(resolve, id)),
            world_types
                .as_ref()
                .map(|wt| world_type_name(resolve, wt, id)),
        ]
        .into_iter()
        .flatten()
        {
            if alias != qualified {
                types
                    .entry(alias.clone())
                    .or_insert_with(|| Foreign::Type(HirType::Named(qualified.clone())));
                types.entry(format!("{alias}Input")).or_insert_with(|| {
                    Foreign::Input(Box::new(Foreign::Type(HirType::Named(qualified.clone()))))
                });
            }
        }
    }
    for (id, interface) in &resolve.interfaces {
        let Some(module) = resolve.id_of(id) else {
            continue;
        };
        for (name, id) in &interface.types {
            let decl = declaration_id(resolve, *id);
            let target = qualified_type_name(resolve, decl);
            if types.contains_key(&target) {
                let local = to_pascal_case(name);
                types.insert(
                    format!("{module}#{local}"),
                    Foreign::Type(HirType::Named(target.clone())),
                );
                types.insert(
                    format!("{module}#{local}Input"),
                    Foreign::Input(Box::new(Foreign::Type(HirType::Named(target)))),
                );
            }
        }
    }
    for (wit_key, width, signedness) in [
        ("u8", 8, Signedness::Unsigned),
        ("s8", 8, Signedness::Signed),
        ("u16", 16, Signedness::Unsigned),
        ("s16", 16, Signedness::Signed),
        ("u32", 32, Signedness::Unsigned),
        ("s32", 32, Signedness::Signed),
        ("u64", 64, Signedness::Unsigned),
        ("s64", 64, Signedness::Signed),
    ] {
        types
            .entry(wit_key.to_string())
            .or_insert(Foreign::Integer { width, signedness });
    }
    types
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
        Type::U8 => HirType::Named("perry:types/u8".into()),
        Type::S8 => HirType::Named("perry:types/s8".into()),
        Type::U16 => HirType::Named("perry:types/u16".into()),
        Type::S16 => HirType::Named("perry:types/s16".into()),
        Type::U32 => HirType::Named("perry:types/u32".into()),
        Type::S32 => HirType::Named("perry:types/s32".into()),
        Type::F32 | Type::F64 => HirType::Number,
        Type::Char | Type::String => HirType::String,
        Type::U64 => HirType::Named("perry:types/u64".into()),
        Type::S64 => HirType::Named("perry:types/s64".into()),
        Type::ErrorContext => return None,
        Type::Id(id) => {
            let definition = &resolve.types[*id];
            if definition.name.is_some() {
                HirType::Named(qualified_type_name(resolve, *id))
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
        TypeDefKind::Option(inner) => HirType::Union(vec![ty(resolve, inner)?, HirType::Null]),
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
