//! Borrowed Wasm sections and the function index space shared by linker inputs.
use super::wasm_err;
use perry_wit_diagnostics::{Result, ensure_internal, internal};
use wasm_encoder::{CodeSection, FunctionSection, reencode::Reencode};
use wasmparser::*;

#[derive(Default)]
pub(super) struct ParsedModule<'a> {
    pub(super) types: Vec<wasm_encoder::FuncType>,
    pub(super) imports: Vec<(&'a str, &'a str, u32)>,
    pub(super) other_imports: Vec<Import<'a>>,
    pub(super) function_types: Vec<u32>,
    pub(super) bodies: Vec<FunctionBody<'a>>,
    pub(super) tables: Option<TableSectionReader<'a>>,
    pub(super) memories: Option<MemorySectionReader<'a>>,
    pub(super) globals: Option<GlobalSectionReader<'a>>,
    pub(super) exports: Option<ExportSectionReader<'a>>,
    pub(super) elements: Option<ElementSectionReader<'a>>,
    pub(super) data: Option<DataSectionReader<'a>>,
    pub(super) tags: Option<TagSectionReader<'a>>,
    pub(super) custom: Vec<CustomSectionReader<'a>>,
    pub(super) start: Option<u32>,
    pub(super) data_count: Option<u32>,
}

impl<'a> ParsedModule<'a> {
    pub(super) fn parse(bytes: &'a [u8]) -> Result<Self> {
        let mut module = Self::default();
        let mut sections = 0u32;
        for payload in Parser::new(0).parse_all(bytes) {
            let payload = payload.map_err(wasm_err)?;
            if let Some((id, _)) = payload.as_section()
                && id != 0
            {
                let bit = 1u32
                    .checked_shl(u32::from(id))
                    .ok_or_else(|| internal!("unsupported section id"))?;
                ensure_internal!(sections & bit == 0, "duplicate Wasm section {id}");
                sections |= bit;
            }
            match payload {
                Payload::TypeSection(section) => {
                    module.types = section
                        .into_iter_err_on_gc_types()
                        .map(|ty| {
                            wasm_encoder::reencode::RoundtripReencoder
                                .func_type(ty.map_err(wasm_err)?)
                                .map_err(wasm_err)
                        })
                        .collect::<Result<_>>()?;
                }
                Payload::ImportSection(section) => {
                    for import in section.into_imports() {
                        let import = import.map_err(wasm_err)?;
                        if let TypeRef::Func(ty) = import.ty {
                            module.imports.push((import.module, import.name, ty));
                        } else {
                            module.other_imports.push(import);
                        }
                    }
                }
                Payload::FunctionSection(section) => {
                    module.function_types = section
                        .into_iter()
                        .map(|ty| ty.map_err(wasm_err))
                        .collect::<Result<_>>()?;
                }
                Payload::CodeSectionEntry(body) => module.bodies.push(body),
                Payload::TableSection(section) => module.tables = Some(section),
                Payload::MemorySection(section) => module.memories = Some(section),
                Payload::GlobalSection(section) => module.globals = Some(section),
                Payload::ExportSection(section) => module.exports = Some(section),
                Payload::ElementSection(section) => module.elements = Some(section),
                Payload::DataSection(section) => module.data = Some(section),
                Payload::TagSection(section) => module.tags = Some(section),
                Payload::CustomSection(section) => module.custom.push(section),
                Payload::StartSection { func, .. } => module.start = Some(func),
                Payload::DataCountSection { count, .. } => module.data_count = Some(count),
                _ => (),
            }
        }
        ensure_internal!(
            module.bodies.len() == module.function_types.len(),
            "function declarations must match bodies"
        );
        if let Some(count) = module.data_count {
            ensure_internal!(
                count == module.data.as_ref().map_or(0, |data| data.count()),
                "data count must match segments"
            );
        }
        Ok(module)
    }

    pub(super) fn emit_functions(
        &self,
        functions: &mut FunctionSection,
        code: &mut CodeSection,
        selected: impl IntoIterator<Item = usize>,
        relocation: &mut impl Reencode<Error = String>,
    ) -> Result<()> {
        for local in selected {
            let ty = *self
                .function_types
                .get(local)
                .ok_or_else(|| internal!("function declaration out of range"))?;
            let body = self
                .bodies
                .get(local)
                .ok_or_else(|| internal!("function body out of range"))?;
            functions.function(relocation.type_index(ty).map_err(wasm_err)?);
            relocation
                .parse_function_body(code, body.clone())
                .map_err(wasm_err)?;
        }
        Ok(())
    }
}
