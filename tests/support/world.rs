//! Compile a source for a single-world WIT package and instantiate the component, which
//! imports nothing from the host.
use anyhow::Result;
use wasmtime::{
    Engine, Store,
    component::{Component, Instance, Linker},
};

/// Compile `source` (named `file`) for `world` of the WIT package text `wit`; ambient WASI
/// packages resolve as dependencies.
pub(crate) fn compile(
    wit: &str,
    world: &str,
    file: &str,
    source: &str,
) -> Result<perry_wit::Compiled> {
    let (resolve, package) =
        perry_wit::component::wit::resolve_source(&format!("{world}.wit"), wit)?;
    let world = resolve.select_world(&[package], Some(world))?;
    Ok(perry_wit::compile_for_world(
        source, file, &resolve, world, true,
    )?)
}

/// Compile `source` against a one-export world whose `run` has `signature`. Async worlds may
/// report errors and wait on timers.
pub(crate) fn compile_signature(source: &str, signature: &str) -> Result<perry_wit::Compiled> {
    let imports = if signature.starts_with("async ") {
        "import wasi:cli/stderr@0.3.0; import wasi:clocks/monotonic-clock@0.3.0;"
    } else {
        ""
    };
    compile(
        &format!("package test:signature; world test {{ {imports} export run: {signature}; }}"),
        "test",
        "signature.ts",
        source,
    )
}

/// The stripped component of `source` (named `file`) for `world` in the package `wit`.
pub(crate) fn component(wit: &str, world: &str, file: &str, source: &str) -> Result<Vec<u8>> {
    compile(wit, world, file, source)?
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

/// Function names in the `name` custom section of a core module, empty when it is stripped.
pub(crate) fn function_names(core: &[u8]) -> Result<Vec<&str>> {
    let mut names = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(core) {
        if let wasmparser::Payload::CustomSection(section) = payload?
            && let wasmparser::KnownCustom::Name(subsections) = section.as_known()
        {
            for subsection in subsections {
                if let wasmparser::Name::Function(entries) = subsection? {
                    for entry in entries {
                        names.push(entry?.name);
                    }
                }
            }
        }
    }
    Ok(names)
}
