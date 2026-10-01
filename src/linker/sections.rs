//! WebAssembly section parsing and inventory for TypeScript and guest runtime modules.

use std::collections::HashMap;

use anyhow::{Result, bail};
use wasmparser::{ExternalKind, FunctionBody, Global, Parser, Payload, TableType};

pub(crate) struct ParsedModuleA<'a> {
    pub(crate) types: Vec<wasmparser::FuncType>,
    pub(crate) imports: Vec<(&'a str, &'a str, u32)>,
    pub(crate) func_types: Vec<u32>,
    pub(crate) tables: Vec<TableType>,
    pub(crate) memories: Vec<wasmparser::MemoryType>,
    pub(crate) globals: Vec<Global<'a>>,
    pub(crate) exports: Vec<wasmparser::Export<'a>>,
    pub(crate) elements: Vec<wasmparser::Element<'a>>,
    pub(crate) bodies: Vec<FunctionBody<'a>>,
    pub(crate) data: Vec<wasmparser::Data<'a>>,
}

pub(crate) struct ParsedModuleB<'a> {
    pub(crate) types: Vec<wasmparser::FuncType>,
    pub(crate) wasi_imports: Vec<(&'a str, &'a str, u32)>,
    pub(crate) func_types: Vec<u32>,
    pub(crate) tables: Vec<TableType>,
    pub(crate) globals: Vec<Global<'a>>,
    pub(crate) exports: Vec<wasmparser::Export<'a>>,
    pub(crate) export_funcs: HashMap<&'a str, u32>,
    pub(crate) start: Option<u32>,
    pub(crate) elements: Vec<wasmparser::Element<'a>>,
    pub(crate) bodies: Vec<FunctionBody<'a>>,
    pub(crate) data: Vec<wasmparser::Data<'a>>,
}

pub(crate) fn parse_module_a<'a>(bytes: &'a [u8]) -> Result<ParsedModuleA<'a>> {
    let mut types = Vec::new();
    let mut imports = Vec::new();
    let mut func_types = Vec::new();
    let mut tables = Vec::new();
    let mut memories = Vec::new();
    let mut globals = Vec::new();
    let mut exports = Vec::new();
    let mut elements = Vec::new();
    let mut bodies = Vec::new();
    let mut data = Vec::new();

    for payload in Parser::new(0).parse_all(bytes) {
        match payload? {
            Payload::TypeSection(reader) => {
                types = reader
                    .into_iter_err_on_gc_types()
                    .collect::<Result<_, _>>()?;
            }
            Payload::ImportSection(reader) => {
                for imp in reader.into_imports() {
                    let imp = imp?;
                    match imp.ty {
                        wasmparser::TypeRef::Func(type_idx) => {
                            imports.push((imp.module, imp.name, type_idx));
                        }
                        other => bail!(
                            "TypeScript module contains unexpected non-function import: {other:?}"
                        ),
                    }
                }
            }
            Payload::FunctionSection(reader) => {
                for f in reader {
                    func_types.push(f?);
                }
            }
            Payload::TableSection(reader) => {
                for t in reader {
                    tables.push(t?.ty);
                }
            }
            Payload::MemorySection(reader) => {
                for m in reader {
                    memories.push(m?);
                }
            }
            Payload::GlobalSection(reader) => {
                for g in reader {
                    globals.push(g?);
                }
            }
            Payload::ExportSection(reader) => {
                for exp in reader {
                    exports.push(exp?);
                }
            }
            Payload::ElementSection(reader) => {
                for el in reader {
                    elements.push(el?);
                }
            }
            Payload::CodeSectionEntry(body) => {
                bodies.push(body);
            }
            Payload::DataSection(reader) => {
                for d in reader {
                    data.push(d?);
                }
            }
            _ => (),
        }
    }

    Ok(ParsedModuleA {
        types,
        imports,
        func_types,
        tables,
        memories,
        globals,
        exports,
        elements,
        bodies,
        data,
    })
}

pub(crate) fn parse_module_b<'a>(bytes: &'a [u8]) -> Result<ParsedModuleB<'a>> {
    let mut types = Vec::new();
    let mut wasi_imports = Vec::new();
    let mut func_types = Vec::new();
    let mut tables = Vec::new();
    let mut globals = Vec::new();
    let mut exports = Vec::new();
    let mut export_funcs = HashMap::new();
    let mut start = None;
    let mut elements = Vec::new();
    let mut bodies = Vec::new();
    let mut data = Vec::new();

    for payload in Parser::new(0).parse_all(bytes) {
        match payload? {
            Payload::TypeSection(reader) => {
                types = reader
                    .into_iter_err_on_gc_types()
                    .collect::<Result<_, _>>()?;
            }
            Payload::ImportSection(reader) => {
                for imp in reader.into_imports() {
                    let imp = imp?;
                    match imp.ty {
                        wasmparser::TypeRef::Func(type_idx) => {
                            wasi_imports.push((imp.module, imp.name, type_idx));
                        }
                        wasmparser::TypeRef::Memory(_)
                            if imp.module == "env" && imp.name == "memory" =>
                        {
                            // Borrowed application memory; unified in merged module
                        }
                        other => bail!("Guest runtime contains unexpected import: {other:?}"),
                    }
                }
            }
            Payload::FunctionSection(reader) => {
                for f in reader {
                    func_types.push(f?);
                }
            }
            Payload::TableSection(reader) => {
                for t in reader {
                    tables.push(t?.ty);
                }
            }
            Payload::GlobalSection(reader) => {
                for g in reader {
                    globals.push(g?);
                }
            }
            Payload::ExportSection(reader) => {
                for exp in reader {
                    let exp = exp?;
                    if exp.kind == ExternalKind::Func {
                        export_funcs.insert(exp.name, exp.index);
                    }
                    exports.push(exp);
                }
            }
            Payload::StartSection { func, .. } => {
                start = Some(func);
            }
            Payload::ElementSection(reader) => {
                for el in reader {
                    elements.push(el?);
                }
            }
            Payload::CodeSectionEntry(body) => {
                bodies.push(body);
            }
            Payload::DataSection(reader) => {
                for d in reader {
                    data.push(d?);
                }
            }
            _ => (),
        }
    }

    Ok(ParsedModuleB {
        types,
        wasi_imports,
        func_types,
        tables,
        globals,
        exports,
        export_funcs,
        start,
        elements,
        bodies,
        data,
    })
}
