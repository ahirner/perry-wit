//! WIT types as HIR types under their SDK names, for programs that import the SDK's generated
//! declarations. The mapping mirrors the SDK's TypeScript:
//! records are object types, variants unions of `{ tag, val }` objects, options
//! `T | null`, tuples tuples and `list<u8>` a `Uint8Array`.
use crate::abi::types::{Contract, Input, convert, primitive};
use crate::sdk::{NamedType, canonical_type, collect_world_types, declaration_id, to_pascal_case};
use perry_lower::hir::{Foreign, ObjectType, PropertyInfo, Signedness, Type as HirType};
use std::collections::HashMap;
use wit_parser::{Resolve, TypeDefKind, WorldId};

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
        let Some(named) = NamedType::get(resolve, id) else {
            continue;
        };
        let entry = if matches!(definition.kind, TypeDefKind::Resource) {
            Foreign::Resource(canonical_type(resolve, id).index() as u32)
        } else {
            let Some(ty) = ty(resolve, Input::Definition(id)) else {
                continue;
            };
            Foreign::Type(ty)
        };
        let qualified = named.qualified();
        let input_entry =
            Foreign::Input(Box::new(Foreign::Type(HirType::Named(qualified.clone()))));
        types.insert(qualified.clone(), entry);
        types.insert(format!("{qualified}Input"), input_entry);
        for alias in [
            Some(named.in_scope(resolve.types.iter().map(|(id, _)| id))),
            world_types
                .as_ref()
                .map(|wt| named.in_scope(wt.iter().copied())),
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
            let Some(named) = NamedType::get(resolve, decl) else {
                continue;
            };
            let target = named.qualified();
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

fn object(fields: impl IntoIterator<Item = Option<(String, HirType)>>) -> Option<HirType> {
    Some(HirType::Object(ObjectType {
        properties: fields
            .into_iter()
            .map(|field| {
                let (name, ty) = field?;
                Some((
                    name,
                    PropertyInfo {
                        ty,
                        optional: false,
                        readonly: false,
                    },
                ))
            })
            .collect::<Option<_>>()?,
        ..ObjectType::default()
    }))
}

pub(super) fn ty(resolve: &Resolve, input: Input) -> Option<HirType> {
    convert(
        resolve,
        input,
        &|id| {
            Some(Some(HirType::Named(
                NamedType::get(resolve, id)?.qualified(),
            )))
        },
        &|contract| {
            Some(match contract {
                Contract::Scalar(ty) => primitive!(ty, hir)?,
                Contract::Resource { id, .. } => super::resources::source_type(resolve, id)?,
                Contract::Record(fields) => {
                    return object(fields.map(|(name, ty)| Some((name, ty?))));
                }
                Contract::Variant(cases) => HirType::Union(
                    cases
                        .map(|(name, payload)| {
                            object(
                                std::iter::once(Some((
                                    "tag".into(),
                                    HirType::StringLiteral(name.into()),
                                )))
                                .chain(payload.map(|ty| Some(("val".into(), ty?)))),
                            )
                        })
                        .collect::<Option<_>>()?,
                ),
                Contract::Enum(cases) => HirType::Union(
                    cases
                        .map(|name| HirType::StringLiteral(name.into()))
                        .collect(),
                ),
                Contract::Result { ok, err } => HirType::Union(
                    [("value", ok), ("error", err)]
                        .into_iter()
                        .map(|(name, payload)| {
                            object(
                                std::iter::once(Some(("ok".into(), HirType::Boolean)))
                                    .chain(payload.map(|ty| Some((name.into(), ty?)))),
                            )
                        })
                        .collect::<Option<_>>()?,
                ),
                Contract::Option(ty) => HirType::Union(vec![ty?, HirType::Null]),
                Contract::Bytes => HirType::Named("Uint8Array".into()),
                Contract::List(ty) => HirType::Array(Box::new(ty?)),
                Contract::Tuple(items) => HirType::Tuple(items.collect::<Option<_>>()?),
                Contract::ByteStream => HirType::Generic {
                    base: "ReadableStream".into(),
                    type_args: vec![HirType::Named("Uint8Array".into())],
                },
                Contract::Unsupported(_) => return None,
            })
        },
    )
}
