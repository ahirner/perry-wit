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

fn referenced(ty: ValType) -> Option<u32> {
    match ty {
        ValType::Ref(reference) => match reference.heap_type {
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
        let mut references = ty
            .params()
            .iter()
            .chain(ty.results())
            .copied()
            .filter_map(referenced)
            .peekable();
        let mapped = if references.peek().is_none() {
            self.output.intern(ty)
        } else {
            let mut recursive = false;
            for dependency in references {
                if dependency < index {
                    self.resolve(dependency)?;
                } else if dependency == index {
                    recursive = true;
                } else {
                    return Err(Error::UserError(format!(
                        "forward type reference {dependency} in single-type recursion group {index}"
                    )));
                }
            }
            let relocated = |self_index| {
                let relocate = |mut value: ValType| {
                    if let ValType::Ref(reference) = &mut value {
                        let source = match &mut reference.heap_type {
                            HeapType::Concrete(id) | HeapType::Exact(id) => id,
                            HeapType::Abstract { .. } => return value,
                        };
                        *source = if *source == index {
                            self_index
                        } else {
                            self.mapped[*source as usize].expect("dependency was interned")
                        };
                    }
                    value
                };
                FuncType::new(
                    ty.params().iter().copied().map(relocate),
                    ty.results().iter().copied().map(relocate),
                )
            };
            // Wasm bounds module type counts below u32::MAX. Reserve that index in the
            // interner key for a reference local to the current recursion group.
            let key = relocated(u32::MAX);
            if !recursive {
                self.output.intern(&key)
            } else if let Some(&mapped) = self.output.indices.get(&key) {
                mapped
            } else {
                let mapped = self.output.section.len();
                self.output.section.ty().func_type(&relocated(mapped));
                self.output.indices.insert(key, mapped);
                mapped
            }
        };
        self.mapped[slot] = Some(mapped);
        Ok(mapped)
    }
}
