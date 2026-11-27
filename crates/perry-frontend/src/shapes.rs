//! Source declarations used by type inference and MIR lowering.
use perry_hir::types::Type as HirType;
use perry_wit_diagnostics::Result;
use pliron::{builtin::types::Signedness, context::Context};
use std::collections::{HashMap, HashSet};
pub(crate) mod generics;
mod imports;
mod records;
pub(crate) use imports::Imported;
pub(crate) use records::ClassShape;
pub(crate) type TypeName = pliron::uniqued_any::Uniqued<String>;

pub(crate) struct Declarations<'h> {
    pub(crate) interfaces: HashMap<&'h str, &'h perry_hir::ir::Interface>,
    pub(crate) type_aliases: HashMap<&'h str, &'h perry_hir::ir::TypeAlias>,
    pub(crate) classes: HashMap<&'h str, ClassShape<'h>>,
    pub(crate) world: &'h crate::wit::WorldTypes<'h>,
    integer_imports: HashMap<&'h str, (u32, Signedness)>,
    pub(crate) imported_types: HashMap<&'h str, &'h crate::wit::Foreign<'h>>,
    pub(crate) imports: HashMap<&'h str, Imported<'h>>,
}

impl<'h> Declarations<'h> {
    pub(crate) fn new(
        interfaces: &'h [perry_hir::ir::Interface],
        type_aliases: &'h [perry_hir::ir::TypeAlias],
        imports: &'h [perry_hir::ir::Import],
        classes: &'h [perry_hir::ir::Class],
        world: &'h crate::wit::WorldTypes<'h>,
        locations: &crate::hir_compat::Locations,
    ) -> Result<Self> {
        let foreign = world.foreign();
        let interfaces = interfaces
            .iter()
            .map(|interface| (interface.name.as_str(), interface))
            .collect();
        let type_aliases = type_aliases
            .iter()
            .map(|alias| (alias.name.as_str(), alias))
            .collect();
        let mut integer_imports = HashMap::new();
        let mut imported_types = HashMap::new();
        let mut bindings = HashMap::new();
        for import in imports {
            let source = import.source.as_str();
            let annotate = |e: perry_wit_diagnostics::Diagnostic| {
                e.with_fallback_span(locations.import_span(source))
            };
            let module = imports::Module::classify(source).map_err(annotate)?;
            for specifier in &import.specifiers {
                use perry_hir::ir::ImportSpecifier::*;
                let (local, name) = match specifier {
                    Named { imported, local } => (local.as_str(), Some(imported.as_str())),
                    Default { local } | Namespace { local } => (local.as_str(), None),
                };
                let value = module.resolve(source, name).map_err(annotate)?;
                bindings.insert(local, value);
                let Some(imported) = name else { continue };
                if import.source == "perry:types"
                    && let Some(&(_, _, spec)) = crate::wit::INTEGERS
                        .iter()
                        .find(|(name, _, _)| *name == imported)
                {
                    integer_imports.insert(local, spec);
                    continue;
                }
                let source = source.strip_prefix("wit:").unwrap_or(source);
                let binding = format!("{source}#{imported}");
                if let Some(function) = world.function(&binding) {
                    if matches!(value, Imported::Other) {
                        bindings.insert(local, Imported::Foreign(function));
                    }
                } else if let Some(entry) = foreign.get(&binding).or_else(|| foreign.get(imported))
                {
                    imported_types.insert(local, entry);
                }
            }
        }
        Ok(Self {
            interfaces,
            type_aliases,
            classes: HashMap::new(),
            world,
            integer_imports,
            imported_types,
            imports: bindings,
        }
        .with_classes(classes.iter().map(ClassShape::Source)))
    }

    pub(crate) fn with_classes(mut self, classes: impl Iterator<Item = ClassShape<'h>>) -> Self {
        self.classes = classes
            .filter(|c| !crate::hir_compat::is_object_literal(c.name()))
            .map(|c| (c.name(), c))
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
            let alias = self.type_aliases.get(name)?;
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
