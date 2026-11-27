//! Source declarations used by type inference and MIR lowering.
use indexmap::IndexMap;
use perry_contract::types::{INTEGERS, Integer};
use perry_hir::types::Type as HirType;
use perry_wit_diagnostics::Result;
use pliron::context::Context;
use std::collections::HashSet;
pub(crate) mod generics;
mod imports;
mod records;
pub(crate) use imports::Imported;
pub(crate) use records::ClassShape;
pub(crate) type TypeName = pliron::uniqued_any::Uniqued<String>;

#[derive(Default)]
pub(crate) struct Symbol<'h> {
    interface: Option<&'h perry_hir::ir::Interface>,
    alias: Option<&'h perry_hir::ir::TypeAlias>,
    class: Option<ClassShape<'h>>,
    pub(crate) import: Option<Imported<'h>>,
    foreign: Option<&'h crate::wit::Foreign<'h>>,
    integer: Option<Integer>,
}

#[derive(Clone, Copy)]
pub(crate) struct SymbolId(usize);

impl<'h> Symbol<'h> {
    pub(crate) fn class_id(&self) -> Option<crate::source::ClassId> {
        crate::source::ClassId::of(match self.class? {
            ClassShape::Source(c) => c,
            ClassShape::Instance(c) => c.declaration,
        })
    }
    pub(crate) fn instance(&self) -> Option<&'h crate::source::InstanceClass<'h>> {
        match self.class? {
            ClassShape::Instance(c) => Some(c),
            ClassShape::Source(_) => None,
        }
    }
}

pub(crate) struct Declarations<'h> {
    symbols: IndexMap<&'h str, Symbol<'h>>,
    pub(crate) world: &'h crate::wit::WorldTypes<'h>,
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
        let mut symbols = IndexMap::<&str, Symbol>::new();
        for interface in interfaces {
            symbols.entry(&interface.name).or_default().interface = Some(interface);
        }
        for alias in type_aliases {
            symbols.entry(&alias.name).or_default().alias = Some(alias);
        }
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
                let symbol = symbols.entry(local).or_default();
                symbol.import = Some(value);
                let Some(imported) = name else { continue };
                if import.source == "perry:types"
                    && let Some(&(_, _, spec)) =
                        INTEGERS.iter().find(|(name, _, _)| *name == imported)
                {
                    symbol.integer = Some(spec);
                    continue;
                }
                let source = source.strip_prefix("wit:").unwrap_or(source);
                let binding = format!("{source}#{imported}");
                if let Some(function) = world.function(&binding) {
                    if matches!(value, Imported::Other) {
                        symbol.import = Some(Imported::Foreign(function));
                    }
                } else if let Some(entry) = foreign.get(&binding).or_else(|| foreign.get(imported))
                {
                    symbol.foreign = Some(entry);
                }
            }
        }
        Ok(Self { symbols, world }.with_classes(classes.iter().map(ClassShape::Source)))
    }

    pub(crate) fn with_classes(mut self, classes: impl Iterator<Item = ClassShape<'h>>) -> Self {
        for symbol in self.symbols.values_mut() {
            symbol.class = None;
        }
        for class in classes.filter(|c| !crate::hir_compat::is_object_literal(c.name())) {
            self.symbols.entry(class.name()).or_default().class = Some(class);
        }
        self
    }

    pub(crate) fn is_user_type(&self, name: &str) -> bool {
        generics::Declaration::find(self, name).is_some()
    }

    pub(crate) fn bind(&self, name: &str) -> Option<SymbolId> {
        self.symbols.get_index_of(name).map(SymbolId)
    }
    pub(crate) fn symbol(&self, id: SymbolId) -> &Symbol<'h> {
        &self.symbols[id.0]
    }
    pub(crate) fn class_id(&self, name: &str) -> Option<crate::source::ClassId> {
        self.symbols.get(name)?.class_id()
    }

    pub(crate) fn imported_type(&self, name: &str) -> Option<&'h crate::wit::Foreign<'h>> {
        self.symbols.get(name)?.foreign
    }

    pub(crate) fn integer_spec<'a>(&'a self, ctx: &Context, mut name: &'a str) -> Option<Integer> {
        let mut seen = HashSet::new();
        while self.is_user_type(name) {
            let alias = self.symbols.get(name)?.alias?;
            let HirType::Named(target) = &alias.ty else {
                return None;
            };
            if !alias.type_params.is_empty() || !seen.insert(name) {
                return None;
            }
            name = target;
        }
        if let Some(domain) = self.symbols.get(name).and_then(|s| s.integer) {
            return Some(domain);
        }
        let domain = match self.foreign_entry(name)? {
            crate::wit::Foreign::Type(value) | crate::wit::Foreign::Input(value) => {
                value.integer()?
            }
        };
        (self.imported_type(name).is_some() || perry_mir::builtin_value_type(ctx, name).is_none())
            .then_some(domain)
    }

    fn foreign_entry(&self, name: &str) -> Option<&crate::wit::Foreign<'h>> {
        self.imported_type(name)
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
        if self.imported_type(name).is_none() {
            let wit_parser::Type::Id(id) = value.ty else {
                return None;
            };
            if perry_contract::names::NamedType::get(value.resolve, id)?.qualified() != name {
                return None;
            }
        }
        Some(resource.key())
    }
}
