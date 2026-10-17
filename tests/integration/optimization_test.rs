//! Binary structure and behavior of optimized WIT-constrained programs.
use anyhow::Result;
use wasmparser::{KnownCustom, Name, Operator, Parser, Payload};

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

/// Decoded sections and instruction bodies of a core Wasm module for structural assertions.
struct WasmSummary<'a> {
    functions: u32,
    memories: u32,
    imports: u32,
    has_start: bool,
    has_name_section: bool,
    exports: Vec<&'a str>,
    function_names: Vec<&'a str>,
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
        function_names: Vec::new(),
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
                if let KnownCustom::Name(subsections) = section.as_known() {
                    for subsection in subsections {
                        if let Name::Function(entries) = subsection? {
                            for entry in entries {
                                summary.function_names.push(entry?.name);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(summary)
}
