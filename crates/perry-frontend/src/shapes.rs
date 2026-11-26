//! Source declarations used by type inference and MIR lowering.
use perry_hir::types::Type as HirType;
use perry_wit_diagnostics::Result;
use pliron::{builtin::types::Signedness, context::Context};
use std::collections::{HashMap, HashSet};
pub(crate) mod generics;
mod records;
pub(crate) use records::ClassShape;
pub(crate) type TypeName = pliron::uniqued_any::Uniqued<String>;

pub(crate) struct Declarations<'h> {
    pub(crate) interfaces: HashMap<&'h str, (usize, &'h perry_hir::ir::Interface)>,
    pub(crate) type_aliases: HashMap<&'h str, (usize, &'h perry_hir::ir::TypeAlias)>,
    pub(crate) classes: HashMap<&'h str, (usize, ClassShape<'h>)>,
    pub(crate) world: &'h crate::wit::WorldTypes<'h>,
    pub(crate) resource_signatures: crate::engine::types::IdxVec<
        crate::wit::MemberId,
        Option<&'h crate::wit::ImportSignature<'h>>,
    >,
    integer_imports: HashMap<&'h str, (u32, Signedness)>,
    pub(crate) imported_types: HashMap<&'h str, &'h crate::wit::Foreign<'h>>,
    pub(crate) foreign_functions: HashMap<&'h str, (&'h str, &'h crate::wit::ImportSignature<'h>)>,
}

impl<'h> Declarations<'h> {
    pub(crate) fn new(
        interfaces: &'h [perry_hir::ir::Interface],
        type_aliases: &'h [perry_hir::ir::TypeAlias],
        imports: &'h [perry_hir::ir::Import],
        classes: &'h [perry_hir::ir::Class],
        world: &'h crate::wit::WorldTypes<'h>,
    ) -> Self {
        let foreign = world.foreign();
        let interfaces = interfaces
            .iter()
            .enumerate()
            .map(|(idx, interface)| (interface.name.as_str(), (idx, interface)))
            .collect();
        let type_aliases = type_aliases
            .iter()
            .enumerate()
            .map(|(idx, alias)| (alias.name.as_str(), (idx, alias)))
            .collect();
        let mut integer_imports = HashMap::new();
        let mut imported_types = HashMap::new();
        let mut foreign_functions = HashMap::new();
        for import in imports {
            let source = import
                .source
                .strip_prefix("wit:")
                .unwrap_or(import.source.as_str());
            for specifier in &import.specifiers {
                let perry_hir::ir::ImportSpecifier::Named { imported, local } = specifier else {
                    continue;
                };
                if import.source == "perry:types"
                    && let Some(&(_, _, spec)) = crate::wit::INTEGERS
                        .iter()
                        .find(|(name, _, _)| name == imported)
                {
                    integer_imports.insert(local.as_str(), spec);
                    continue;
                }
                let binding = format!("{source}#{imported}");
                if let Some(function) = world.function(&binding) {
                    foreign_functions.insert(local.as_str(), function);
                }
                if let Some(entry) = [binding.as_str(), imported.as_str()]
                    .into_iter()
                    .find_map(|key| foreign.get(key))
                    .filter(|entry| !matches!(entry, crate::wit::Foreign::Function(_)))
                {
                    imported_types.insert(local.as_str(), entry);
                }
            }
        }
        Self {
            interfaces,
            type_aliases,
            classes: HashMap::new(),
            world,
            resource_signatures: world.members.map(|member| {
                world
                    .function(&member.binding)
                    .map(|(_, signature)| signature)
            }),
            integer_imports,
            imported_types,
            foreign_functions,
        }
        .with_classes(classes.iter().map(ClassShape::Source))
    }

    pub(crate) fn with_classes(mut self, classes: impl Iterator<Item = ClassShape<'h>>) -> Self {
        self.classes = classes
            .enumerate()
            .filter(|(_, c)| !crate::hir_compat::is_object_literal(c.name()))
            .map(|(index, c)| (c.name(), (index, c)))
            .collect();
        self
    }

    pub(crate) fn is_user_type(&self, name: &str) -> bool {
        self.interfaces.contains_key(name)
            || self.type_aliases.contains_key(name)
            || self.classes.contains_key(name)
    }

    pub(crate) fn integer_spec<'a>(
        &'a self,
        ctx: &Context,
        mut name: &'a str,
    ) -> Option<(u32, Signedness)> {
        let mut seen = HashSet::new();
        while self.is_user_type(name) {
            let (_, alias) = self.type_aliases.get(name)?;
            let HirType::Named(target) = &alias.ty else {
                return None;
            };
            if !alias.type_params.is_empty() || !seen.insert(name) {
                return None;
            }
            name = target;
        }
        if let Some(&domain) = self.integer_imports.get(name) {
            return Some(domain);
        }
        let domain = match self.foreign_entry(name)? {
            crate::wit::Foreign::Type(value) | crate::wit::Foreign::Input(value) => {
                value.integer()?
            }
            _ => return None,
        };
        (self.imported_types.contains_key(name)
            || perry_mir::builtin_value_type(ctx, name).is_none())
        .then_some(domain)
    }

    fn foreign_entry(&self, name: &str) -> Option<&crate::wit::Foreign<'h>> {
        self.imported_types
            .get(name)
            .copied()
            .or_else(|| self.world.foreign().get(name))
    }

    pub(crate) fn foreign_type(&self, name: &str) -> Option<crate::wit::WitValue<'h>> {
        match self.foreign_entry(name)? {
            crate::wit::Foreign::Type(value) if !self.is_user_type(name) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn resource(&self, name: &str) -> Option<u32> {
        let value = self.foreign_type(name)?;
        let resource = value.resource()?;
        if !self.imported_types.contains_key(name) {
            let wit_parser::Type::Id(id) = value.ty else {
                return None;
            };
            if perry_contract::names::NamedType::get(value.resolve, id)?.qualified() != name {
                return None;
            }
        }
        Some(resource)
    }
}
