//! Raw inspection modules have no P3 host. Unreachable canonical observer paths trap if
//! exercised; only component exports support suspension and native completion callbacks.
use anyhow::Result;
use std::{collections::HashMap, convert::Infallible};
use wasm_encoder::{
    reencode::{self, Reencode},
    *,
};
use wasmparser::{Parser, Payload, TypeRef};

pub(crate) fn without_p3(core: Vec<u8>) -> Result<Vec<u8>> {
    let mut imported = 0;
    let mut defined = 0;
    let mut stubs = Vec::new();
    for payload in Parser::new(0).parse_all(&core) {
        match payload? {
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    let import = import?;
                    if let TypeRef::Func(ty) = import.ty {
                        if import.name.ends_with("perry-internal-promise")
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
    let removed: HashMap<_, _> = stubs
        .iter()
        .enumerate()
        .map(|(i, &(index, _))| (index, i as u32))
        .collect();
    let retained = imported - stubs.len() as u32;
    let mut indices = Vec::new();
    let mut next = 0;
    for old in 0..imported + defined {
        indices.push(if let Some(&stub) = removed.get(&old) {
            retained + defined + stub
        } else {
            let index = next;
            next += 1;
            index
        });
    }
    let mut module = Module::new();
    Raw {
        indices,
        removed,
        types: stubs.into_iter().map(|(_, ty)| ty).collect(),
    }
    .parse_core_module(&mut module, Parser::new(0), &core)?;
    Ok(module.finish())
}
struct Raw {
    indices: Vec<u32>,
    removed: HashMap<u32, u32>,
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
                let removed = self.removed.contains_key(&function);
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
