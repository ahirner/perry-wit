//! Declaration identities, reachable WIT types and their TypeScript names.
use std::collections::HashSet;
use wit_parser::{Resolve, Type, TypeDefKind, TypeId, TypeOwner, World, WorldItem};

/// Converts a kebab-case or snake_case string into camelCase.
pub(crate) fn to_camel_case(s: &str) -> String {
    convert_case(s, false)
}

/// Converts a kebab-case or snake_case string into PascalCase.
pub(crate) fn to_pascal_case(s: &str) -> String {
    convert_case(s, true)
}

fn convert_case(s: &str, mut capitalize_next: bool) -> String {
    let mut result = String::with_capacity(s.len());
    for (i, c) in s.chars().enumerate() {
        if !c.is_ascii_alphanumeric() {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(c.to_ascii_uppercase());
            capitalize_next = false;
        } else if i == 0 {
            result.push(c.to_ascii_lowercase());
        } else {
            result.push(c);
        }
    }
    result
}

/// Resolve aliases while preserving renamed declarations in the generated SDK.
pub(crate) fn declaration_id(resolve: &Resolve, mut id: TypeId) -> TypeId {
    while let TypeDefKind::Type(Type::Id(inner)) = resolve.types[id].kind {
        if resolve.types[id].name != resolve.types[inner].name {
            break;
        }
        id = inner;
    }
    id
}

/// Resolve through all type aliases to the underlying canonical definition.
pub(crate) fn canonical_type(resolve: &Resolve, mut id: TypeId) -> TypeId {
    while let TypeDefKind::Type(Type::Id(inner)) = resolve.types[id].kind {
        id = inner;
    }
    id
}

pub(crate) struct NamedType<'a> {
    resolve: &'a Resolve,
    id: TypeId,
    name: &'a str,
}

impl<'a> NamedType<'a> {
    pub(crate) fn get(resolve: &'a Resolve, id: TypeId) -> Option<Self> {
        let id = declaration_id(resolve, id);
        Some(Self {
            resolve,
            id,
            name: resolve.types[id].name.as_deref()?,
        })
    }

    pub(crate) fn qualified(&self) -> String {
        let resolve = self.resolve;
        let owner = match resolve.types[self.id].owner {
            TypeOwner::Interface(interface) => resolve
                .id_of(interface)
                .unwrap_or_else(|| format!("interface-{}", interface.index())),
            TypeOwner::World(world) => {
                let world = &resolve.worlds[world];
                world
                    .package
                    .map(|package| resolve.id_of_name(package, &world.name))
                    .unwrap_or_else(|| world.name.clone())
            }
            TypeOwner::None => format!("type-{}", self.id.index()),
        };
        format!("{}{}", to_pascal_case(&owner), to_pascal_case(self.name))
    }

    pub(crate) fn in_scope(&self, universe: impl IntoIterator<Item = TypeId>) -> String {
        let name = to_pascal_case(self.name);
        let collides = universe.into_iter().any(|other| {
            let other = declaration_id(self.resolve, other);
            other != self.id
                && self.resolve.types[other]
                    .name
                    .as_deref()
                    .is_some_and(|other| to_pascal_case(other) == name)
        });
        if collides { self.qualified() } else { name }
    }
}

pub(super) struct TsNames<'a> {
    pub(super) resolve: &'a Resolve,
    pub(super) types: HashSet<TypeId>,
}

impl<'a> TsNames<'a> {
    pub(super) fn new(resolve: &'a Resolve, world: &World) -> Self {
        Self {
            resolve,
            types: collect_world_types(resolve, world),
        }
    }

    pub(super) fn name(&self, id: TypeId) -> Option<String> {
        Some(NamedType::get(self.resolve, id)?.in_scope(self.types.iter().copied()))
    }
}

pub(super) fn collect_types(resolve: &Resolve, ty: &Type, ids: &mut HashSet<TypeId>) {
    let Type::Id(id) = ty else {
        return;
    };
    let id = declaration_id(resolve, *id);
    if ids.insert(id) {
        visit_type_children(&resolve.types[id].kind, |ty| {
            collect_types(resolve, ty, ids)
        });
    }
}

pub(crate) fn collect_world_types(resolve: &Resolve, world: &World) -> HashSet<TypeId> {
    let mut seen = HashSet::new();
    let items = || world.imports.iter().chain(world.exports.iter());
    for (_, item) in items() {
        match item {
            WorldItem::Interface { id, .. } => {
                for &id in resolve.interfaces[*id].types.values() {
                    collect_types(resolve, &Type::Id(id), &mut seen);
                }
            }
            WorldItem::Type { id, .. } => collect_types(resolve, &Type::Id(*id), &mut seen),
            WorldItem::Function(_) => {}
        }
    }
    for entry in crate::abi::world_functions(resolve, items()) {
        for param in &entry.function.params {
            collect_types(resolve, &param.ty, &mut seen);
        }
        if let Some(ret) = &entry.function.result {
            collect_types(resolve, ret, &mut seen);
        }
    }
    seen.retain(|id| resolve.types[*id].name.is_some());
    seen
}

pub(super) fn visit_type_children(kind: &TypeDefKind, mut visit: impl FnMut(&Type)) {
    match kind {
        TypeDefKind::Handle(wit_parser::Handle::Own(id) | wit_parser::Handle::Borrow(id)) => {
            visit(&Type::Id(*id))
        }
        TypeDefKind::List(inner) | TypeDefKind::Option(inner) | TypeDefKind::Type(inner) => {
            visit(inner);
        }
        TypeDefKind::Result(result) => result.ok.iter().chain(result.err.iter()).for_each(visit),
        TypeDefKind::Tuple(tuple) => tuple.types.iter().for_each(visit),
        TypeDefKind::Record(record) => record.fields.iter().map(|field| &field.ty).for_each(visit),
        TypeDefKind::Variant(variant) => variant
            .cases
            .iter()
            .filter_map(|case| case.ty.as_ref())
            .for_each(visit),
        _ => {}
    }
}
