//! WIT types as HIR types under their SDK names, for programs that import the SDK's generated
//! declarations (which the parser does not read). The mapping mirrors the SDK's TypeScript:
//! records are object types, variants unions of `{ tag, val }` objects, options
//! `T | null | undefined`, tuples tuples and `list<u8>` a `Uint8Array`.
use crate::sdk::codegen::{to_camel_case, type_name};
use perry_lower::hir::{ObjectType, PropertyInfo, Type as HirType};
use std::collections::HashMap;
use wit_parser::{Resolve, Type, TypeDefKind};

/// Every named WIT type of `resolve` that has a HIR equivalent, by SDK name prefixed with
/// `wit:` (references between them use these names too), so that only imported names reach
/// them and a WIT `headers` does not shadow the `Headers` class.
pub(crate) fn foreign(resolve: &Resolve) -> HashMap<String, HirType> {
    resolve
        .types
        .iter()
        .filter(|(_, definition)| definition.name.is_some())
        // A `use` makes an alias with the declaration's SDK name: keep the declaration.
        .filter(|(id, definition)| {
            !matches!(definition.kind, TypeDefKind::Type(Type::Id(inner))
                if type_name(resolve, inner) == type_name(resolve, *id))
        })
        .filter_map(|(id, definition)| {
            Some((foreign_name(resolve, id), kind(resolve, &definition.kind)?))
        })
        .collect()
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

fn ty(resolve: &Resolve, ty: &Type) -> Option<HirType> {
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
        Type::U64 | Type::S64 | Type::ErrorContext => return None,
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
