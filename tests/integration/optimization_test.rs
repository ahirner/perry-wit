//! Binary structure and behavior of optimized WIT-constrained programs.
use anyhow::Result;
use wasmparser::{KnownCustom, Name, Parser, Payload};

mod asynchronous;
mod boundaries;
mod collections;
mod constraints;
mod integers;

/// Compiles `source` against a one-export world whose `run` has `signature`.
fn compile(source: &str, signature: &str) -> Result<perry_wit::Compiled> {
    let stderr = if signature.starts_with("async ") {
        "import wasi:cli/stderr@0.3.0;"
    } else {
        ""
    };
    let (resolve, package) = perry_wit::component::wit::resolve_source(
        "optimization.wit",
        &format!("package test:optimization; world test {{ {stderr} export run: {signature}; }}"),
    )?;
    let world = resolve.select_world(&[package], Some("test"))?;
    Ok(perry_wit::compile_for_world(
        source,
        "optimization.ts",
        &resolve,
        world,
        true,
    )?)
}

/// Names exported by a core module, borrowed from its bytes.
fn export_names(core: &[u8]) -> Result<Vec<&str>> {
    let mut names = Vec::new();
    for payload in Parser::new(0).parse_all(core) {
        if let Payload::ExportSection(section) = payload? {
            for export in section {
                names.push(export?.name);
            }
        }
    }
    Ok(names)
}

/// Function names recorded in a core module's `name` section.
fn function_names(core: &[u8]) -> Result<Vec<&str>> {
    let mut names = Vec::new();
    for payload in Parser::new(0).parse_all(core) {
        if let Payload::CustomSection(section) = payload?
            && let KnownCustom::Name(subsections) = section.as_known()
        {
            for subsection in subsections {
                if let Name::Function(entries) = subsection? {
                    for entry in entries {
                        names.push(entry?.name);
                    }
                }
            }
        }
    }
    Ok(names)
}
