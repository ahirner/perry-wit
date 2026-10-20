//! Binary structure and behavior of optimized WIT-constrained programs.
use crate::world::compile_signature as compile;
use anyhow::Result;
use wasmparser::{Operator, Parser, Payload};

mod asynchronous;
mod boundaries;
mod constraints;
mod integers;
mod json_copy;

/// Decoded sections and instruction bodies of a core Wasm module for structural assertions.
struct WasmSummary<'a> {
    functions: u32,
    memories: u32,
    imports: u32,
    has_start: bool,
    has_name_section: bool,
    exports: Vec<&'a str>,
    bodies: Vec<Vec<Operator<'a>>>,
}

impl<'a> WasmSummary<'a> {
    fn instructions(&self) -> impl Iterator<Item = &Operator<'a>> {
        self.bodies.iter().flat_map(|body| body.iter())
    }
}

fn inspect_wasm(core: &[u8]) -> Result<WasmSummary<'_>> {
    let mut summary = WasmSummary {
        functions: 0,
        memories: 0,
        imports: 0,
        has_start: false,
        has_name_section: false,
        exports: Vec::new(),
        bodies: Vec::new(),
    };
    for payload in Parser::new(0).parse_all(core) {
        match payload? {
            Payload::FunctionSection(section) => summary.functions = section.count(),
            Payload::MemorySection(section) => summary.memories = section.count(),
            Payload::ImportSection(section) => summary.imports = section.count(),
            Payload::StartSection { .. } => summary.has_start = true,
            Payload::ExportSection(section) => {
                for export in section {
                    summary.exports.push(export?.name);
                }
            }
            Payload::CodeSectionEntry(body) => {
                let ops = body
                    .get_operators_reader()?
                    .into_iter()
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                summary.bodies.push(ops);
            }
            Payload::CustomSection(section) => {
                summary.has_name_section |= section.name() == "name";
            }
            _ => {}
        }
    }
    Ok(summary)
}
