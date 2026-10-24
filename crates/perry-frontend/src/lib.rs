//! TypeScript parsing, Perry HIR compatibility, specialization and source metadata.
use anyhow::Result;
use pliron::context::Context;
use std::collections::{BTreeMap, HashMap};
mod diagnostics;
mod open_types;
pub use diagnostics::{LowerError, LowerErrorKind, ShapeError};
pub use open_types::{TypeState, contextual_type, has_reader_overload};
mod absence;
pub mod collections;
pub mod completion;
pub mod hir_compat;
mod infer;
pub mod methods;
mod modules;
pub mod sdk;
pub mod shapes;
pub mod source_files;
pub mod wit;
pub use absence::{Absence, merge_absence};
/// HIR types and structural foreign (WIT) symbol descriptors.
pub mod hir {
    pub use perry_hir::types::{FunctionType, ObjectType, PropertyInfo, Type};
    pub use perry_hir::*;
    pub use pliron::builtin::types::Signedness;

    /// A foreign (WIT) symbol in the world's symbol table.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Foreign {
        Type(Type),
        Integer { width: u32, signedness: Signedness },
        Resource(u32),
        Input(Box<Foreign>),
        Function(FunctionType),
    }
}

/// Resolved, specialized HIR and source facts, shared read-only with elaboration.
pub struct Program<'s> {
    module: hir::ir::Module,
    metadata: hir_compat::Metadata<'s>,
    foreign: &'s HashMap<String, hir::Foreign>,
}
impl<'s> Program<'s> {
    pub fn module(&self) -> &hir::ir::Module {
        &self.module
    }
    pub fn metadata(&self) -> &hir_compat::Metadata<'s> {
        &self.metadata
    }
    pub fn foreign(&self) -> &HashMap<String, hir::Foreign> {
        self.foreign
    }
}

pub fn load_sources<'s>(
    ctx: &mut Context,
    source: &'s str,
    identity: &str,
    sources: &'s BTreeMap<String, String>,
    foreign: &'s HashMap<String, hir::Foreign>,
) -> Result<Program<'s>> {
    let loaded = modules::load(source, identity, sources, foreign)?;
    let mut lowered = hir_compat::lower(loaded, identity)?;
    infer::specialize_functions(ctx, &mut lowered.module, &lowered.metadata, foreign)?;
    infer::unresolved_types(ctx, &mut lowered.module, &lowered.metadata, foreign);
    open_types::mark(&mut lowered.module, &mut lowered.metadata);
    Ok(Program {
        module: lowered.module,
        metadata: lowered.metadata,
        foreign,
    })
}

pub fn is_identifier(name: &str) -> bool {
    perry_parser::swc_ecma_ast::Ident::verify_symbol(name).is_ok()
}
