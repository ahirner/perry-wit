//! TypeScript parsing, Perry HIR compatibility, specialization and source metadata.
use perry_wit_diagnostics::Result;
use pliron::context::Context;
use std::collections::BTreeMap;

mod lower;
pub use lower::{ExportTarget, LoweredProgram, MAIN};
mod collections;
mod completion;
pub(crate) mod engine;
mod hir_compat;
mod methods;
mod modules;
mod narrowing;
pub mod sdk;
mod shapes;
mod source;
pub mod source_files;
pub mod wit;
use perry_mir::Absence;
pub fn lower_sources(
    ctx: &mut Context,
    source: &str,
    identity: &str,
    sources: &BTreeMap<String, String>,
    types: &wit::WorldTypes,
) -> Result<pliron::builtin::ops::ModuleOp> {
    Ok(lower_world(ctx, source, identity, sources, types, None)?.module)
}

pub fn lower_world(
    ctx: &mut Context,
    source: &str,
    identity: &str,
    sources: &BTreeMap<String, String>,
    types: &wit::WorldTypes,
    world: Option<(&wit_parser::Resolve, wit_parser::WorldId)>,
) -> Result<LoweredProgram> {
    let foreign = types.foreign();
    let loaded = modules::load(source, identity, sources, foreign)?;
    let mut lowered = hir_compat::lower(loaded, identity, world)?;
    let specialization =
        engine::run_engine(ctx, &mut lowered.module, &mut lowered.metadata, foreign)?;
    lowered
        .metadata
        .locations
        .record_call_origins(&lowered.module);
    lower::program::build_module(
        ctx,
        &lowered.module,
        foreign,
        &lowered.metadata,
        &specialization,
    )
}

pub(crate) fn is_identifier(name: &str) -> bool {
    perry_parser::swc_ecma_ast::Ident::verify_symbol(name).is_ok()
}
