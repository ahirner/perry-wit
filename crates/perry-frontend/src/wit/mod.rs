//! Source types and ownership helpers for a resolved component contract.
mod types;
use crate::engine::types::IdxVec;
pub(crate) use crate::engine::types::MemberId;
pub(crate) use types::supported;

use perry_contract::{
    names::{canonical_type, to_camel_case, to_pascal_case},
    types::Input,
};
use perry_hir::ir::Expr;
use pliron::builtin::types::Signedness;
use std::collections::HashMap;
use wit_parser::{FunctionKind, Handle, Resolve, Type, TypeDefKind, TypeId, WorldId};

pub(crate) const INTEGERS: [(&str, Type, (u32, Signedness)); 8] = [
    ("u8", Type::U8, (8, Signedness::Unsigned)),
    ("s8", Type::S8, (8, Signedness::Signed)),
    ("u16", Type::U16, (16, Signedness::Unsigned)),
    ("s16", Type::S16, (16, Signedness::Signed)),
    ("u32", Type::U32, (32, Signedness::Unsigned)),
    ("s32", Type::S32, (32, Signedness::Signed)),
    ("u64", Type::U64, (64, Signedness::Unsigned)),
    ("s64", Type::S64, (64, Signedness::Signed)),
];

#[derive(Clone, Copy)]
pub(crate) struct WitValue<'r> {
    pub(crate) resolve: &'r Resolve,
    pub(crate) ty: Type,
}

impl<'r> WitValue<'r> {
    pub(crate) fn result(self) -> Option<&'r wit_parser::Result_> {
        perry_contract::result_type(self.resolve, self.ty)
    }

    pub(crate) fn integer(self) -> Option<(u32, Signedness)> {
        let ty = match perry_contract::type_kind(self.resolve, self.ty) {
            Some(TypeDefKind::Type(ty)) => *ty,
            Some(_) => return None,
            None => self.ty,
        };
        INTEGERS
            .iter()
            .find(|(_, wit, _)| *wit == ty)
            .map(|&(_, _, domain)| domain)
    }

    pub(crate) fn resource(self) -> Option<u32> {
        let Type::Id(id) = self.ty else { return None };
        let id = canonical_type(self.resolve, id);
        let owner = match self.resolve.types[id].kind {
            TypeDefKind::Resource => id,
            TypeDefKind::Handle(Handle::Own(owner) | Handle::Borrow(owner)) => {
                canonical_type(self.resolve, owner)
            }
            _ => return None,
        };
        Some(owner.index() as u32)
    }
}

pub(crate) struct ImportSignature<'r> {
    pub(crate) params: Vec<WitValue<'r>>,
    pub(crate) result: Option<WitValue<'r>>,
    pub(crate) is_async: bool,
}

pub(crate) enum Foreign<'r> {
    Type(WitValue<'r>),
    Input(WitValue<'r>),
    Function(ImportSignature<'r>),
}

#[derive(Default)]
pub struct WorldTypes<'r> {
    foreign: HashMap<String, Foreign<'r>>,
    pub(crate) members: IdxVec<MemberId, ResourceMember<'r>>,
    constructors: MemberGroup,
    statics: HashMap<String, MemberGroup>,
    methods: HashMap<String, MemberGroup>,
    pub(crate) drops: MemberGroup,
}

impl<'r> WorldTypes<'r> {
    pub fn new(resolve: &'r Resolve, world: Option<WorldId>) -> Self {
        let mut types = Self {
            foreign: types::foreign(resolve, world),
            ..Default::default()
        };
        let Some(world) = world else { return types };
        let imports = &resolve.worlds[world].imports;
        for entry in perry_contract::world_functions(resolve, imports.iter()) {
            let f = entry.function;
            let name = || to_camel_case(f.item_name());
            let (owner, members) = match f.kind {
                FunctionKind::Method(owner) => (owner, types.methods.entry(name()).or_default()),
                FunctionKind::Static(owner) => (owner, types.statics.entry(name()).or_default()),
                FunctionKind::Constructor(owner) => (owner, &mut types.constructors),
                _ => continue,
            };
            if !f.params.iter().all(|param| match param.ty {
                Type::Id(id)
                    if matches!(
                        resolve.types[canonical_type(resolve, id)].kind,
                        TypeDefKind::Flags(_)
                    ) =>
                {
                    false
                }
                ty => types::supported(resolve, Input::Reference(ty)),
            }) {
                continue;
            }
            let owner = canonical_type(resolve, owner).index() as u32;
            let result = f
                .result
                .and_then(|ty| perry_contract::result_type(resolve, ty).map_or(Some(ty), |r| r.ok));
            let module = resolve.name_world_key(entry.key);
            members.entry(owner).or_insert_with(|| {
                types.members.push(ResourceMember {
                    binding: format!("{module}#{}", to_camel_case(&f.name)),
                    result: result.map(|ty| WitValue { resolve, ty }),
                })
            });
        }
        for (key, item) in imports {
            let wit_parser::WorldItem::Interface { id, .. } = item else {
                continue;
            };
            let module = resolve.name_world_key(key);
            for (name, id) in &resolve.interfaces[*id].types {
                if matches!(resolve.types[*id].kind, TypeDefKind::Resource) {
                    types.drops.entry(id.index() as u32).or_insert_with(|| {
                        types.members.push(ResourceMember {
                            binding: format!("{module}#drop{}", to_pascal_case(name)),
                            result: None,
                        })
                    });
                }
            }
        }
        types
    }

    pub(crate) fn foreign(&self) -> &HashMap<String, Foreign<'r>> {
        &self.foreign
    }

    pub(crate) fn function(&self, name: &str) -> Option<(&str, &ImportSignature<'r>)> {
        let (binding, Foreign::Function(signature)) = self.foreign.get_key_value(name)? else {
            return None;
        };
        Some((binding, signature))
    }

    pub fn import_function(
        &mut self,
        resolve: &'r Resolve,
        module: &str,
        name: &str,
        params: impl IntoIterator<Item = Type>,
        result: Option<Type>,
        is_async: bool,
    ) -> String {
        let binding = format!("{module}#{name}");
        self.foreign.insert(
            binding.clone(),
            Foreign::Function(ImportSignature {
                params: params
                    .into_iter()
                    .map(|ty| WitValue { resolve, ty })
                    .collect(),
                result: result.map(|ty| WitValue { resolve, ty }),
                is_async,
            }),
        );
        binding
    }
}

pub(crate) type MemberGroup = HashMap<u32, MemberId>;

#[derive(Default)]
pub(crate) struct ReceiverMembers<'h>(HashMap<*const Expr, &'h MemberGroup>);

impl<'h> ReceiverMembers<'h> {
    pub(crate) fn bind(
        &mut self,
        expr: &Expr,
        decls: &crate::shapes::Declarations<'h>,
        locations: &crate::hir_compat::Locations,
    ) -> Option<MemberId> {
        use crate::hir_compat::{class_receiver, member, method};
        let world = decls.world;
        if world.members.is_empty() {
            return None;
        }
        let (class, members) = match expr {
            Expr::New { class_name, .. } => (class_name.as_str(), &world.constructors),
            Expr::StaticMethodCall {
                class_name,
                method_name,
                ..
            } => (class_name.as_str(), world.statics.get(method_name)?),
            Expr::Call {
                callee,
                byte_offset,
                byte_end,
                ..
            } => {
                let (object, property) = member(callee)?;
                match property.map(|name| method(name).name()) {
                    Some(name) if let Some(class) = class_receiver(object) => {
                        (class, world.statics.get(name)?)
                    }
                    Some(name) => {
                        self.0.insert(object, world.methods.get(name)?);
                        return None;
                    }
                    None if !world.drops.is_empty()
                        && locations.calls_dispose(*byte_offset, *byte_end) =>
                    {
                        self.0.insert(object, &world.drops);
                        return None;
                    }
                    None => return None,
                }
            }
            _ => return None,
        };
        members.get(&decls.resource(class)?).copied()
    }

    pub(crate) fn method<'s>(&self, callee: &'s Expr) -> Option<(&'s Expr, Option<&'s str>)> {
        let (object, property) = crate::hir_compat::member(callee)?;
        (property.is_some() || self.0.contains_key(&std::ptr::from_ref(object)))
            .then_some((object, property))
    }

    pub(crate) fn get(&self, object: &Expr, owner: u32) -> Option<MemberId> {
        self.0
            .get(&std::ptr::from_ref(object))?
            .get(&owner)
            .copied()
    }
}

pub(crate) struct ResourceMember<'r> {
    pub(crate) binding: String,
    pub(crate) result: Option<WitValue<'r>>,
}

pub(crate) fn owned_resource(resolve: &Resolve, ty: Type) -> Option<TypeId> {
    let Type::Id(id) = ty else { return None };
    let id = canonical_type(resolve, id);
    match resolve.types[id].kind {
        TypeDefKind::Handle(Handle::Own(owner)) => Some(canonical_type(resolve, owner)),
        TypeDefKind::Resource => Some(id),
        _ => None,
    }
}
