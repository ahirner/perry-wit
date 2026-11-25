//! Source declarations used by type inference and MIR lowering.
use perry_hir::types::Type as HirType;
use perry_wit_diagnostics::Result;
use pliron::{builtin::types::Signedness, context::Context};
use std::collections::{HashMap, HashSet};
pub(crate) mod generics;
mod globals;
pub(crate) use globals::captured;
pub(crate) use globals::class_functions;
mod records;
pub(crate) use records::Classes;
pub(crate) type TypeName = pliron::uniqued_any::Uniqued<String>;

pub(crate) struct Declarations<'h> {
    pub(crate) interfaces: HashMap<&'h str, (usize, &'h perry_hir::ir::Interface)>,
    pub(crate) type_aliases: HashMap<&'h str, (usize, &'h perry_hir::ir::TypeAlias)>,
    pub(crate) classes: Classes<'h>,
    pub(crate) foreign: &'h HashMap<String, crate::wit::Foreign<'h>>,
    pub(crate) integer_imports: HashMap<&'h str, (u32, Signedness)>,
    pub(crate) imported_types: HashMap<&'h str, &'h crate::wit::Foreign<'h>>,
    pub(crate) foreign_functions: HashMap<&'h str, (&'h str, &'h crate::wit::ImportSignature<'h>)>,
}

impl<'h> Declarations<'h> {
    pub(crate) fn new(
        interfaces: &'h [perry_hir::ir::Interface],
        type_aliases: &'h [perry_hir::ir::TypeAlias],
        imports: &'h [perry_hir::ir::Import],
        classes: &'h [perry_hir::ir::Class],
        foreign: &'h HashMap<String, crate::wit::Foreign<'h>>,
    ) -> Self {
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
        let classes = Classes::new(classes);
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
                    && let Some(spec) = perry_integer_spec(imported)
                {
                    integer_imports.insert(local.as_str(), spec);
                    continue;
                }
                let binding = format!("{source}#{imported}");
                if let Some((key, crate::wit::Foreign::Function(signature))) =
                    foreign.get_key_value(&binding)
                {
                    foreign_functions.insert(local.as_str(), (key.as_str(), signature));
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
            classes,
            foreign,
            integer_imports,
            imported_types,
            foreign_functions,
        }
    }

    pub(crate) fn is_user_type(&self, name: &str) -> bool {
        self.interfaces.contains_key(name)
            || self.type_aliases.contains_key(name)
            || self.classes.named.contains_key(name)
    }

    pub(crate) fn integer_spec(&self, name: &str) -> Option<(u32, Signedness)> {
        let mut seen = HashSet::new();
        self.integer_spec_by_name(name, &mut seen)
    }

    fn integer_spec_by_name<'a>(
        &'a self,
        name: &'a str,
        seen: &mut HashSet<&'a str>,
    ) -> Option<(u32, Signedness)> {
        if let Some(spec) = name
            .strip_prefix("perry:types/")
            .and_then(perry_integer_spec)
        {
            return Some(spec);
        }
        if !seen.insert(name) {
            return None;
        }
        if !self.is_user_type(name) {
            if let Some(&spec) = self.integer_imports.get(name) {
                return Some(spec);
            }
            if let Some(entry) = self
                .imported_types
                .get(name)
                .copied()
                .or_else(|| self.foreign.get(name))
            {
                return match entry {
                    crate::wit::Foreign::Type(value) | crate::wit::Foreign::Input(value) => {
                        value.integer()
                    }
                    _ => None,
                };
            }
        }
        if let Some((_, alias)) = self
            .type_aliases
            .get(name)
            .copied()
            .filter(|(_, a)| a.type_params.is_empty())
            && let HirType::Named(target) = &alias.ty
        {
            return self.integer_spec_by_name(target, seen);
        }
        None
    }

    pub(crate) fn foreign_type(&self, name: &str) -> Option<crate::wit::WitValue<'h>> {
        if self.is_user_type(name) {
            return None;
        }
        match self
            .imported_types
            .get(name)
            .copied()
            .or_else(|| self.foreign.get(name))?
        {
            crate::wit::Foreign::Type(value) => Some(*value),
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

pub(crate) fn perry_integer_spec(name: &str) -> Option<(u32, Signedness)> {
    Some(match name {
        "u8" => (8, Signedness::Unsigned),
        "s8" => (8, Signedness::Signed),
        "u16" => (16, Signedness::Unsigned),
        "s16" => (16, Signedness::Signed),
        "u32" => (32, Signedness::Unsigned),
        "s32" => (32, Signedness::Signed),
        "u64" => (64, Signedness::Unsigned),
        "s64" => (64, Signedness::Signed),
        _ => return None,
    })
}
