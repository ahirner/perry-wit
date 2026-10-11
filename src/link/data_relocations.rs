//! Resolve constant helper data addresses before instantiation.
use super::libraries::{Global, Library};
use std::borrow::Cow;
use wasmparser::Operator;

pub(super) struct RelocatedData {
    pub function: u32,
    pub segments: Vec<(u32, Cow<'static, [u8]>)>,
}

/// Fold only straight-line stores of placed addresses into disjoint initialized segments.
/// Unsupported instructions or destinations preserve the entire runtime initializer.
pub(super) fn fold(
    lib: &Library,
    memory_base: u32,
    table_base: u32,
    heap_base: u32,
) -> Option<RelocatedData> {
    let function = *lib.exports.get("__wasm_apply_data_relocs")?;
    if lib.start.is_some_and(|start| start != function) {
        return None;
    }
    let body = lib
        .bodies
        .get((function as usize).checked_sub(lib.imports.len())?)?;
    let mut segments: Vec<_> = lib
        .data
        .iter()
        .map(|&(offset, bytes)| (offset, Cow::Borrowed(bytes)))
        .collect();
    let mut ranges: Vec<_> = lib
        .data
        .iter()
        .map(|&(offset, bytes)| (u64::from(offset), u64::from(offset) + bytes.len() as u64))
        .collect();
    ranges.sort_unstable();
    if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return None;
    }
    let mut stack = Vec::<u32>::new();
    let mut reader = body.get_operators_reader().ok()?;
    while !reader.eof() {
        match reader.read().ok()? {
            Operator::I32Const { value } => stack.push(value as u32),
            Operator::GlobalGet { global_index } => {
                stack.push(match lib.globals.get(global_index as usize)? {
                    Global::MemoryBase => memory_base,
                    Global::TableBase => table_base,
                    Global::HeapBase => heap_base,
                    Global::Constant(value) => *value as u32,
                    Global::Stack => return None,
                })
            }
            Operator::I32Add => {
                let right = stack.pop()?;
                let left = stack.pop()?;
                stack.push(left.wrapping_add(right));
            }
            Operator::I32Store { memarg } if memarg.memory == 0 => {
                let value = stack.pop()?;
                let address = stack
                    .pop()?
                    .checked_add(u32::try_from(memarg.offset).ok()?)?;
                let relative = address.checked_sub(memory_base)?;
                let (offset, bytes) = segments.iter_mut().find(|(offset, bytes)| {
                    relative >= *offset
                        && u64::from(relative) + 4 <= u64::from(*offset) + bytes.len() as u64
                })?;
                let at = (relative - *offset) as usize;
                bytes.to_mut()[at..at + 4].copy_from_slice(&value.to_le_bytes());
            }
            Operator::End if reader.eof() && stack.is_empty() => {}
            _ => return None,
        }
    }
    Some(RelocatedData { function, segments })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library(body: &str) -> Library {
        let wasm = wat::parse_str(format!(
            r#"(module
            (import "env" "__memory_base" (global i32))
            (import "env" "__table_base" (global i32))
            (import "env" "memory" (memory 1))
            (data (i32.const 0) "abcdefgh")
            (func (export "__wasm_apply_data_relocs") {body}))"#
        ))
        .unwrap();
        Library::parse(Box::leak(wasm.into_boxed_slice())).unwrap()
    }

    #[test]
    fn folds_placed_addresses_and_keeps_unrecognized_initializers() {
        let lib = library("global.get 0 global.get 1 i32.const 3 i32.add i32.store offset=4");
        let folded = fold(&lib, 1024, 200, 4096).unwrap();
        assert_eq!(&folded.segments[0].1[..4], b"abcd");
        assert_eq!(&folded.segments[0].1[4..], &203_u32.to_le_bytes());
        assert_eq!(lib.data[0].1, b"abcdefgh");
        for body in [
            "global.get 0 i32.const 7 i32.store offset=5",
            "global.get 0 i32.const 7 i32.store i32.const 0 drop",
            "i32.const -1 i32.const 7 i32.store offset=1",
        ] {
            assert!(fold(&library(body), 1024, 200, 4096).is_none(), "{body}");
        }
    }
}
