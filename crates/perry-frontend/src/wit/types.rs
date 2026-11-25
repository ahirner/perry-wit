//! World and interface names for supported WIT contracts.
use crate::wit::{Foreign, WitValue};
use perry_contract::names::{NamedType, collect_world_types, declaration_id, to_pascal_case};
use perry_contract::types::{Contract, Input, convert};

use std::collections::HashMap;
use wit_parser::{Resolve, Type, WorldId};

/// Every named WIT type supported by the frontend, registered under its
/// qualified SDK name, world-scoped SDK name (when non-colliding), and `module#Name`
/// interface binding.
pub(super) fn foreign(resolve: &Resolve, world: Option<WorldId>) -> HashMap<String, Foreign<'_>> {
    let world_types = world.map(|w| collect_world_types(resolve, &resolve.worlds[w]));
    let mut types = HashMap::new();
    for (id, definition) in &resolve.types {
        if definition.name.is_none() || declaration_id(resolve, id) != id {
            continue;
        }
        let Some(named) = NamedType::get(resolve, id) else {
            continue;
        };
        if !supported(resolve, Input::Definition(id)) {
            continue;
        }
        let value = WitValue {
            resolve,
            ty: Type::Id(id),
        };
        let qualified = named.qualified();
        types.insert(qualified.clone(), Foreign::Type(value));
        types.insert(format!("{qualified}Input"), Foreign::Input(value));
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
                types.entry(alias.clone()).or_insert(Foreign::Type(value));
                types
                    .entry(format!("{alias}Input"))
                    .or_insert(Foreign::Input(value));
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
                let value = WitValue {
                    resolve,
                    ty: Type::Id(decl),
                };
                types.insert(format!("{module}#{local}"), Foreign::Type(value));
                types.insert(format!("{module}#{local}Input"), Foreign::Input(value));
            }
        }
    }
    for (key, ty) in [
        ("u8", Type::U8),
        ("s8", Type::S8),
        ("u16", Type::U16),
        ("s16", Type::S16),
        ("u32", Type::U32),
        ("s32", Type::S32),
        ("u64", Type::U64),
        ("s64", Type::S64),
    ] {
        types
            .entry(key.into())
            .or_insert(Foreign::Type(WitValue { resolve, ty }));
    }
    types
}

pub(crate) fn supported(resolve: &Resolve, input: Input) -> bool {
    convert(
        resolve,
        input,
        &|id| NamedType::get(resolve, id).map(|_| true),
        &|contract| match contract {
            Contract::Unsupported(_) | Contract::Scalar(Type::ErrorContext) => false,
            Contract::List(ok) | Contract::Option(ok) => ok,
            Contract::Result { ok, err } => ok.unwrap_or(true) && err.unwrap_or(true),
            Contract::Record(mut fields) => Iterator::all(&mut fields, |(_, ok)| ok),
            Contract::Tuple(mut items) => Iterator::all(&mut items, |ok| ok),
            Contract::Variant(mut cases) => Iterator::all(&mut cases, |(_, ok)| ok.unwrap_or(true)),
            _ => true,
        },
    )
}
