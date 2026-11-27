//! Source declarations used by type inference and MIR lowering.

use perry_contract::types::{INTEGERS, Integer};
use perry_hir::types::Type as HirType;
use perry_wit_diagnostics::Result;
use pliron::context::Context;
use std::collections::{HashMap, HashSet};
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
    pub(crate) foreign: Option<&'h crate::wit::Foreign<'h>>,
    integer: Option<Integer>,
}

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
    symbols: HashMap<TypeName, Symbol<'h>>,
    pub(crate) world: &'h crate::wit::WorldTypes<'h>,
}

impl<'h> Declarations<'h> {
    pub(crate) fn new(
        ctx: &Context,
        interfaces: &'h [perry_hir::ir::Interface],
        type_aliases: &'h [perry_hir::ir::TypeAlias],
        imports: &'h [perry_hir::ir::Import],
        classes: &'h [perry_hir::ir::Class],
        world: &'h crate::wit::WorldTypes<'h>,
        locations: &crate::hir_compat::Locations,
    ) -> Result<Self> {
        let foreign = world.foreign();
        let mut declarations = Self {
            symbols: HashMap::new(),
            world,
        };
        for interface in interfaces {
            declarations.entry(ctx, &interface.name).interface = Some(interface);
        }
        for alias in type_aliases {
            declarations.entry(ctx, &alias.name).alias = Some(alias);
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
                let symbol = declarations.entry(ctx, local);
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
        Ok(declarations.with_classes(ctx, classes.iter().map(ClassShape::Source)))
    }

    pub(crate) fn with_classes(
        mut self,
        ctx: &Context,
        classes: impl Iterator<Item = ClassShape<'h>>,
    ) -> Self {
        for symbol in self.symbols.values_mut() {
            symbol.class = None;
        }
        for class in classes.filter(|c| !crate::hir_compat::is_object_literal(c.name())) {
            self.entry(ctx, class.name()).class = Some(class);
        }
        self
    }

    fn entry(&mut self, ctx: &Context, name: &str) -> &mut Symbol<'h> {
        self.symbols
            .entry(TypeName::new(ctx, name.into()))
            .or_default()
    }

    pub(crate) fn declaration(&self, id: TypeName) -> Option<generics::Declaration<'h>> {
        self.symbol(id)?.declaration()
    }

    pub(crate) fn symbol(&self, id: TypeName) -> Option<&Symbol<'h>> {
        self.symbols.get(&id)
    }

    pub(crate) fn integer_spec(&self, ctx: &Context, mut id: TypeName) -> Option<Integer> {
        let mut seen = HashSet::new();
        while self.declaration(id).is_some() {
            let alias = self.symbol(id)?.alias?;
            let HirType::Named(target) = &alias.ty else {
                return None;
            };
            if !alias.type_params.is_empty() || !seen.insert(id) {
                return None;
            }
            id = TypeName::new(ctx, target.clone());
        }
        if let Some(domain) = self.symbol(id).and_then(|s| s.integer) {
            return Some(domain);
        }
        let domain = match self.foreign_entry(ctx, id)? {
            crate::wit::Foreign::Type(value) | crate::wit::Foreign::Input(value) => {
                value.integer()?
            }
        };
        (self.symbol(id).is_some_and(|s| s.foreign.is_some())
            || perry_mir::builtin_value_type(ctx, id.get(ctx)).is_none())
        .then_some(domain)
    }

    fn foreign_entry(&self, ctx: &Context, id: TypeName) -> Option<&crate::wit::Foreign<'h>> {
        self.symbol(id)
            .and_then(|s| s.foreign)
            .or_else(|| self.world.foreign().get(id.get(ctx).as_str()))
    }

    pub(crate) fn foreign_type(
        &self,
        ctx: &Context,
        id: TypeName,
    ) -> Option<crate::wit::WitValue<'h>> {
        match self.foreign_entry(ctx, id)? {
            crate::wit::Foreign::Type(value) if self.declaration(id).is_none() => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn resource(&self, ctx: &Context, name: TypeName) -> Option<u32> {
        let value = self.foreign_type(ctx, name)?;
        let resource = value.resource()?;
        if self.symbol(name).is_none_or(|s| s.foreign.is_none()) {
            let wit_parser::Type::Id(id) = value.ty else {
                return None;
            };
            if perry_contract::names::NamedType::get(value.resolve, id)?.qualified()
                != *name.get(ctx)
            {
                return None;
            }
        }
        Some(resource.key())
    }
}
