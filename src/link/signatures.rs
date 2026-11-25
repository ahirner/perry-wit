//! Intern only signatures referenced by retained Wasm sections and helper bodies.
use std::collections::HashMap;
use wasm_encoder::{FuncType, HeapType, TypeSection, ValType, reencode::Error};

#[derive(Default)]
pub(super) struct Signatures {
    pub(super) section: TypeSection,
    indices: HashMap<FuncType, u32>,
}

impl Signatures {
    pub(super) fn intern(&mut self, ty: &FuncType) -> u32 {
        if let Some(&index) = self.indices.get(ty) {
            return index;
        }
        let index = self.section.len();
        self.section.ty().func_type(ty);
        self.indices.insert(ty.clone(), index);
        index
    }
}

pub(super) struct TypeIndices<'a> {
    pub(super) definitions: &'a [FuncType],
    pub(super) mapped: &'a mut [Option<u32>],
    pub(super) output: &'a mut Signatures,
}

fn referenced(ty: &mut ValType) -> Option<&mut u32> {
    match ty {
        ValType::Ref(reference) => match &mut reference.heap_type {
            HeapType::Concrete(id) | HeapType::Exact(id) => Some(id),
            HeapType::Abstract { .. } => None,
        },
        _ => None,
    }
}

impl TypeIndices<'_> {
    pub(super) fn resolve(&mut self, index: u32) -> Result<u32, Error<String>> {
        let slot = index as usize;
        let ty = self
            .definitions
            .get(slot)
            .ok_or_else(|| Error::UserError(format!("undefined Wasm type {index}")))?;
        if let Some(mapped) = self.mapped[slot] {
            return Ok(mapped);
        }
        let values = ty.params().iter().chain(ty.results()).copied();
        let mapped = if values
            .clone()
            .any(|mut value| referenced(&mut value).is_some())
        {
            let mut values: Vec<_> = values.collect();
            let mut recursive = false;
            // A Wasm module cannot contain u32::MAX types. In the interner key
            // this index denotes the current single-type recursion group.
            for dependency in values.iter_mut().filter_map(referenced) {
                *dependency = match (*dependency).cmp(&index) {
                    std::cmp::Ordering::Less => self.resolve(*dependency)?,
                    std::cmp::Ordering::Equal => {
                        recursive = true;
                        u32::MAX
                    }
                    std::cmp::Ordering::Greater => {
                        return Err(Error::UserError(format!(
                            "forward type reference {dependency} in single-type recursion group {index}"
                        )));
                    }
                };
            }
            let signature = |values: &[ValType]| {
                let (params, results) = values.split_at(ty.params().len());
                FuncType::new(params.iter().copied(), results.iter().copied())
            };
            let key = signature(&values);
            if !recursive {
                self.output.intern(&key)
            } else if let Some(&mapped) = self.output.indices.get(&key) {
                mapped
            } else {
                let mapped = self.output.section.len();
                for dependency in values.iter_mut().filter_map(referenced) {
                    if *dependency == u32::MAX {
                        *dependency = mapped;
                    }
                }
                self.output.section.ty().func_type(&signature(&values));
                self.output.indices.insert(key, mapped);
                mapped
            }
        } else {
            self.output.intern(ty)
        };
        self.mapped[slot] = Some(mapped);
        Ok(mapped)
    }
}
