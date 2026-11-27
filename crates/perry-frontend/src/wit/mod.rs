//! Source types and ownership helpers for a resolved component contract.
mod types;
use indexmap::IndexMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MemberId(usize);
pub(crate) use types::supported;

use perry_contract::types::Integer;
use perry_contract::{
    names::{canonical_type, to_camel_case},
    types::Input,
};
use perry_hir::ir::Expr;
use std::collections::HashMap;
use wit_parser::{FunctionKind, Handle, Resolve, Type, TypeDefKind, TypeId, WorldId};

#[derive(Clone, Copy)]
pub(crate) struct WitValue<'r> {
    pub(crate) resolve: &'r Resolve,
    pub(crate) ty: Type,
}

impl<'r> WitValue<'r> {
    pub(crate) fn result(self) -> Option<&'r wit_parser::Result_> {
        perry_contract::result_type(self.resolve, self.ty)
    }

    pub(crate) fn integer(self) -> Option<Integer> {
        let ty = match perry_contract::type_kind(self.resolve, self.ty) {
            Some(TypeDefKind::Type(ty)) => *ty,
            Some(_) => return None,
            None => self.ty,
        };
        perry_contract::types::INTEGERS
            .iter()
            .find(|(_, wit, _)| *wit == ty)
            .map(|&(_, _, domain)| domain)
    }

    pub(crate) fn resource(self) -> Option<ResourceId> {
        let Type::Id(id) = self.ty else { return None };
        let id = canonical_type(self.resolve, id);
        let owner = match self.resolve.types[id].kind {
            TypeDefKind::Resource => id,
            TypeDefKind::Handle(Handle::Own(owner) | Handle::Borrow(owner)) => {
                canonical_type(self.resolve, owner)
            }
            _ => return None,
        };
        Some(ResourceId::of(owner))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct ResourceId(u32);

impl ResourceId {
    pub(crate) fn of(id: TypeId) -> Self {
        Self(id.index() as u32)
    }
    pub(crate) fn key(self) -> u32 {
        self.0
    }
}

impl std::borrow::Borrow<u32> for ResourceId {
    fn borrow(&self) -> &u32 {
        &self.0
    }
}

pub(crate) struct ImportSignature<'r> {
    pub(crate) abi_params: Option<Vec<WitValue<'r>>>,
    pub(crate) result: Option<WitValue<'r>>,
    pub(crate) is_async: bool,
}

pub(crate) enum Foreign<'r> {
    Type(WitValue<'r>),
    Input(WitValue<'r>),
}

#[derive(Default)]
pub struct WorldTypes<'r> {
    foreign: HashMap<String, Foreign<'r>>,
    imports: IndexMap<String, ImportSignature<'r>>,
    constructors: MemberGroup,
    statics: HashMap<String, MemberGroup>,
    methods: IndexMap<String, MemberGroup>,
    pub(crate) drops: MemberGroup,
}

impl<'r> WorldTypes<'r> {
    pub fn new(resolve: &'r Resolve, world: Option<WorldId>) -> Self {
        Self::with_imports(resolve, world, |_, _, _| None::<()>).0
    }

    /// Register the functions accepted by the component ABI alongside their source declarations.
    pub fn with_imports<I>(
        resolve: &'r Resolve,
        world: Option<WorldId>,
        mut bind: impl FnMut(&str, &str, &wit_parser::Function) -> Option<I>,
    ) -> (Self, Vec<I>) {
        let mut bindings = Vec::new();
        let mut types = Self {
            foreign: types::foreign(resolve, world),
            ..Default::default()
        };
        let Some(world) = world else {
            return (types, bindings);
        };
        let imports = &resolve.worlds[world].imports;
        for entry in perry_contract::world_functions(resolve, imports.iter()) {
            let f = entry.function;
            let module = entry
                .interface
                .map_or_else(|| "$root".into(), |_| resolve.name_world_key(entry.key));
            let binding = format!("{module}#{}", to_camel_case(&f.name));
            let import = bind(&module, &binding, f);
            let name = || to_camel_case(f.item_name());
            let group = match f.kind {
                FunctionKind::Method(id) => Some((id, types.methods.entry(name()).or_default())),
                FunctionKind::Static(id) => Some((id, types.statics.entry(name()).or_default())),
                FunctionKind::Constructor(id) => Some((id, &mut types.constructors)),
                _ => None,
            }
            .filter(|_| {
                f.params.iter().all(|p| {
                    !matches!(
                        perry_contract::type_kind(resolve, p.ty),
                        Some(TypeDefKind::Flags(_))
                    ) && types::supported(resolve, Input::Reference(p.ty))
                })
            });
            if import.is_some() || group.is_some() {
                let function = ImportSignature::new(
                    resolve,
                    import.as_ref().map(|_| f.params.iter().map(|p| p.ty)),
                    f.result,
                    f.kind.is_async(),
                );
                if import.is_some() {
                    types.foreign.remove(&binding);
                }
                let entry = types.imports.entry(binding);
                let id = MemberId(entry.index());
                if import.is_some() {
                    entry.insert_entry(function);
                } else {
                    entry.or_insert(function);
                }
                if let Some((owner, members)) = group {
                    members
                        .entry(canonical_type(resolve, owner).index() as u32)
                        .or_insert(id);
                }
            }
            bindings.extend(import);
        }
        for (key, id) in perry_contract::world_interfaces(imports) {
            let module = resolve.name_world_key(key);
            for (name, id) in perry_contract::interface_resources(resolve, id) {
                let drop = perry_contract::ResourceOperation::Drop.name(name);
                let binding = format!("{module}#{drop}");
                let entry = types.imports.entry(binding);
                let member = MemberId(entry.index());
                entry.or_insert(ImportSignature {
                    abi_params: None,
                    result: None,
                    is_async: false,
                });
                types.drops.entry(id.index() as u32).or_insert(member);
            }
        }
        (types, bindings)
    }

    pub(crate) fn foreign(&self) -> &HashMap<String, Foreign<'r>> {
        &self.foreign
    }

    pub(crate) fn function(&self, name: &str) -> Option<MemberId> {
        let (index, _, signature) = self.imports.get_full(name)?;
        signature.abi_params.as_ref().map(|_| MemberId(index))
    }

    pub(crate) fn signatures(&self) -> impl Iterator<Item = &ImportSignature<'r>> {
        self.imports.values()
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
        let function = ImportSignature::new(resolve, Some(params), result, is_async);
        self.foreign.remove(&binding);
        self.imports.insert(binding.clone(), function);
        binding
    }
}

impl<'r> ImportSignature<'r> {
    fn new(
        resolve: &'r Resolve,
        params: Option<impl IntoIterator<Item = Type>>,
        result: Option<Type>,
        is_async: bool,
    ) -> Self {
        let value = |ty| WitValue { resolve, ty };
        Self {
            abi_params: params.map(|params| params.into_iter().map(value).collect()),
            result: result.map(value),
            is_async,
        }
    }
}

impl MemberId {
    pub(crate) fn signature<'a, 'r>(self, world: &'a WorldTypes<'r>) -> &'a ImportSignature<'r> {
        &world.imports[self.0]
    }

    pub(crate) fn binding<'a, 'r>(
        self,
        world: &'a WorldTypes<'r>,
    ) -> perry_wit_diagnostics::Result<(&'a str, &'a ImportSignature<'r>)> {
        world
            .imports
            .get_index(self.0)
            .map(|(name, signature)| (name.as_str(), signature))
            .ok_or_else(|| perry_wit_diagnostics::internal!("unknown WIT import"))
    }
}

pub(crate) type MemberGroup = HashMap<u32, MemberId>;

#[derive(Clone, Copy)]
pub(crate) enum MemberGroupId {
    Method(usize),
    Drop,
}

impl MemberGroupId {
    pub(crate) fn member(self, world: &WorldTypes, owner: u32) -> Option<MemberId> {
        let group = match self {
            Self::Method(id) => world.methods.get_index(id)?.1,
            Self::Drop => &world.drops,
        };
        group.get(&owner).copied()
    }
}

pub(crate) enum BoundMember<'h> {
    Static(MemberId),
    Receiver(&'h Expr, MemberGroupId),
}

impl<'h> BoundMember<'h> {
    pub(crate) fn at(
        ctx: &pliron::context::Context,
        expr: &'h Expr,
        decls: &crate::shapes::Declarations,
        locations: &crate::hir_compat::Locations,
    ) -> Option<Self> {
        use crate::hir_compat::{class_receiver, member, method};
        let world = decls.world;
        if world.imports.is_empty() {
            return None;
        }
        let (class, group) = match expr {
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
                match property.name().map(|n| method(n).name()) {
                    Some(name) if let Some(class) = class_receiver(object) => {
                        (class, world.statics.get(name)?)
                    }
                    Some(name) => {
                        return Some(Self::Receiver(
                            object,
                            MemberGroupId::Method(world.methods.get_index_of(name)?),
                        ));
                    }
                    None if !world.drops.is_empty()
                        && locations.calls_dispose(*byte_offset, *byte_end) =>
                    {
                        return Some(Self::Receiver(object, MemberGroupId::Drop));
                    }
                    None => return None,
                }
            }
            _ => return None,
        };
        group
            .get(&decls.resource(ctx, crate::shapes::TypeName::new(ctx, class.into()))?)
            .copied()
            .map(Self::Static)
    }
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
