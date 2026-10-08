//! Intern core function signatures after linking, preserving every type reference.
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    convert::Infallible,
};
use wasm_encoder::{
    Module, TypeSection,
    reencode::{Error, Reencode},
};
use wasmparser::{FuncType, Parser, Payload, TypeSectionReader};

#[derive(Default)]
struct ReferencedTypes(BTreeSet<u32>);
impl Reencode for ReferencedTypes {
    type Error = Infallible;
    fn type_index(&mut self, index: u32) -> Result<u32, Error<Self::Error>> {
        self.0.insert(index);
        Ok(index)
    }
    fn parse_type_section(
        &mut self,
        _: &mut TypeSection,
        _: TypeSectionReader<'_>,
    ) -> Result<(), Error<Self::Error>> {
        Ok(())
    }
    fn parse_custom_name_subsection(
        &mut self,
        _: &mut wasm_encoder::NameSection,
        _: wasmparser::Name<'_>,
    ) -> Result<(), Error<Self::Error>> {
        Ok(())
    }
}

struct Types {
    indices: Vec<u32>,
    unique: Vec<FuncType>,
}
impl Reencode for Types {
    type Error = Infallible;

    fn type_index(&mut self, index: u32) -> Result<u32, Error<Self::Error>> {
        Ok(self.indices[index as usize])
    }

    fn parse_type_section(
        &mut self,
        section: &mut TypeSection,
        _reader: TypeSectionReader<'_>,
    ) -> Result<(), Error<Self::Error>> {
        for ty in std::mem::take(&mut self.unique) {
            section.ty().func_type(&self.func_type(ty)?);
        }
        Ok(())
    }

    fn parse_custom_name_subsection(
        &mut self,
        names: &mut wasm_encoder::NameSection,
        section: wasmparser::Name<'_>,
    ) -> Result<(), Error<Self::Error>> {
        if let wasmparser::Name::Type(entries) = section {
            let mut retained = BTreeMap::new();
            for entry in entries {
                let entry = entry?;
                if let Some(&index) = self.indices.get(entry.index as usize)
                    && index != u32::MAX
                {
                    retained.entry(index).or_insert(entry.name);
                }
            }
            let mut map = wasm_encoder::NameMap::new();
            for (index, name) in retained {
                map.append(index, name);
            }
            names.types(&map);
            Ok(())
        } else {
            wasm_encoder::reencode::utils::parse_custom_name_subsection(self, names, section)
        }
    }
}

pub(super) fn compact(core: &[u8]) -> Result<Vec<u8>> {
    let mut declarations = Vec::new();
    for payload in Parser::new(0).parse_all(core) {
        if let Payload::TypeSection(section) = payload? {
            for ty in section.into_iter_err_on_gc_types() {
                declarations.push(ty?);
            }
        }
    }
    let mut references = ReferencedTypes::default();
    references.parse_core_module(&mut Module::new(), Parser::new(0), core)?;
    let mut visited = BTreeSet::new();
    while let Some(&index) = references.0.difference(&visited).next() {
        visited.insert(index);
        references.func_type(declarations[index as usize].clone())?;
    }
    let mut interned = HashMap::new();
    let mut types = Types {
        indices: vec![u32::MAX; declarations.len()],
        unique: Vec::new(),
    };
    for index in visited {
        let ty = &declarations[index as usize];
        types.indices[index as usize] = *interned.entry(ty).or_insert_with(|| {
            let index = types.unique.len() as u32;
            types.unique.push(ty.clone());
            index
        });
    }
    let mut module = Module::new();
    types.parse_core_module(&mut module, Parser::new(0), core)?;
    Ok(module.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compacts_imports_calls_blocks_and_indirect_function_types() -> Result<()> {
        let core = wat::parse_str(
            r#"(module
            (type $a (func (param i32) (result i32)))
            (type $b (func (param i32) (result i32)))
            (type $pair (func (param i32 i32) (result i32 i32)))
            (type $duplicate-pair (func (param i32 i32) (result i32 i32)))
            (type $unused (func (param i64 i64 i64) (result f64)))
            (import "host" "double" (func $double (type $b)))
            (table 1 funcref)
            (elem (i32.const 0) $double)
            (func (export "run") (type $a) (param i32) (result i32)
                i32.const 0 i32.const 1
                (block (type $duplicate-pair))
                drop drop
                (call_indirect (type $b) (local.get 0) (i32.const 0))))"#,
        )?;
        let compact = compact(&core)?;
        assert!(compact.len() < core.len());
        let count = Parser::new(0)
            .parse_all(&compact)
            .filter_map(|p| match p.unwrap() {
                Payload::TypeSection(s) => Some(s.count()),
                _ => None,
            })
            .sum::<u32>();
        assert_eq!(count, 2);
        let engine = wasmtime::Engine::default();
        let module = wasmtime::Module::new(&engine, compact)?;
        let mut linker = wasmtime::Linker::new(&engine);
        linker.func_wrap("host", "double", |value: i32| value * 2)?;
        let mut store = wasmtime::Store::new(&engine, ());
        let instance = linker.instantiate(&mut store, &module)?;
        assert_eq!(
            instance
                .get_typed_func::<i32, i32>(&mut store, "run")?
                .call(&mut store, 21)?,
            42
        );
        Ok(())
    }
}
