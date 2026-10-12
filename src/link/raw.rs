//! Raw inspection modules have no P3 host. Unreachable canonical observer paths trap if
//! exercised; only component exports support suspension and native completion callbacks.
use anyhow::Result;
use std::convert::Infallible;
use wasm_encoder::{
    reencode::{self, Reencode},
    *,
};
use wasmparser::{Parser, Payload, TypeRef};

/// `component::reactions::ANCHOR`, repeated because the integration tests compile `link`
/// without the rest of the crate.
const ANCHOR: &str = "perry-internal-promise";

/// `core` with its promise-signal and waitable-set imports replaced by trapping functions,
/// so raw modules instantiate without a component-model host.
pub(crate) fn without_p3(core: Vec<u8>) -> Result<Vec<u8>> {
    let mut imported = 0;
    let mut defined = 0;
    // Function index and type of each replaced import, in import order.
    let mut stubs = Vec::new();
    for payload in Parser::new(0).parse_all(&core) {
        match payload? {
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    let import = import?;
                    if let TypeRef::Func(ty) = import.ty {
                        if import.name.ends_with(ANCHOR)
                            || (import.module == "$root"
                                && matches!(
                                    import.name,
                                    "[waitable-set-new]"
                                        | "[waitable-set-drop]"
                                        | "[waitable-join]"
                                ))
                        {
                            stubs.push((imported, ty));
                        }
                        imported += 1;
                    }
                }
            }
            Payload::FunctionSection(section) => defined = section.count(),
            _ => {}
        }
    }
    if stubs.is_empty() {
        return Ok(core);
    }
    // Retained imports and defined functions keep their order; stubs follow them.
    let first_stub = imported - stubs.len() as u32 + defined;
    let mut indices = Vec::with_capacity((imported + defined) as usize);
    let mut stub_ordinals = stubs.iter().map(|&(index, _)| index).peekable();
    let (mut next, mut next_stub) = (0, first_stub);
    for old in 0..imported + defined {
        indices.push(if stub_ordinals.next_if_eq(&old).is_some() {
            next_stub += 1;
            next_stub - 1
        } else {
            next += 1;
            next - 1
        });
    }
    let mut module = Module::new();
    Raw {
        indices,
        first_stub,
        types: stubs.into_iter().map(|(_, ty)| ty).collect(),
    }
    .parse_core_module(&mut module, Parser::new(0), &core)?;
    Ok(module.finish())
}

/// Re-encoder moving replaced imports to trapping functions appended after the defined ones.
struct Raw {
    /// New function index by old function index.
    indices: Vec<u32>,
    /// New index of the first stub; old imports mapped at or above it are removed.
    first_stub: u32,
    /// Type index of each stub, in order.
    types: Vec<u32>,
}
impl Reencode for Raw {
    type Error = Infallible;
    fn function_index(&mut self, index: u32) -> Result<u32, reencode::Error<Self::Error>> {
        Ok(self.indices[index as usize])
    }
    fn parse_import_section(
        &mut self,
        imports: &mut ImportSection,
        section: wasmparser::ImportSectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        let mut function = 0;
        for import in section.into_imports() {
            let import = import?;
            if matches!(import.ty, TypeRef::Func(_)) {
                let removed = self.indices[function] >= self.first_stub;
                function += 1;
                if removed {
                    continue;
                }
            }
            reencode::utils::parse_import(self, imports, import)?;
        }
        Ok(())
    }
    fn parse_function_section(
        &mut self,
        functions: &mut FunctionSection,
        section: wasmparser::FunctionSectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        reencode::utils::parse_function_section(self, functions, section)?;
        for &ty in &self.types {
            functions.function(ty);
        }
        Ok(())
    }
    fn parse_code_section(
        &mut self,
        code: &mut CodeSection,
        section: wasmparser::CodeSectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        reencode::utils::parse_code_section(self, code, section)?;
        for _ in &self.types {
            let mut function = Function::new([]);
            function
                .instruction(&Instruction::Unreachable)
                .instruction(&Instruction::End);
            code.function(&function);
        }
        Ok(())
    }
}
