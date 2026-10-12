//! Compile a source for a single-world WIT package and instantiate the component, which
//! imports nothing from the host.
use anyhow::Result;
use wasmtime::{
    Engine, Store,
    component::{Component, Instance, Linker},
};

/// The stripped component of `source` (named `file`) for `world` in the package `wit`.
pub(crate) fn component(wit: &str, world: &str, file: &str, source: &str) -> Result<Vec<u8>> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(format!("{world}.wit"), wit)?;
    let world = resolve.select_world(&[package], Some(world))?;
    let compiled = perry_wit::compile_for_world(source, file, &resolve, world, true)?;
    compiled
        .component
        .map(|component| component.stripped)
        .ok_or_else(|| anyhow::anyhow!("no component for {file}"))
}

/// A fresh store and instance of `component`.
pub(crate) fn instantiate(engine: &Engine, component: &[u8]) -> Result<(Store<()>, Instance)> {
    let component = Component::new(engine, component)?;
    let mut store = Store::new(engine, ());
    let instance = Linker::new(engine).instantiate(&mut store, &component)?;
    Ok((store, instance))
}
