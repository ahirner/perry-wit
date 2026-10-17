//! Resolve constant helper data addresses before instantiation.
use super::libraries::{Global, Library};
use std::{borrow::Cow, collections::BTreeSet};
use wasmparser::Operator;

pub(super) struct RelocatedData {
    pub(super) functions: BTreeSet<u32>,
    pub(super) segments: Vec<(u32, Cow<'static, [u8]>)>,
}

/// Evaluate constant initialization in order within the library's private data placement.
/// Unsupported instructions or destinations preserve the entire runtime sequence.
pub(super) fn fold(
    lib: &Library,
    memory_base: u32,
    table_base: u32,
    heap_base: u32,
) -> Option<RelocatedData> {
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
    let mut functions = BTreeSet::new();
    for function in lib.initializers() {
        if !functions.insert(function) {
            continue;
        }
        let body = lib
            .bodies
            .get((function as usize).checked_sub(lib.imports.len())?)?;
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
                Operator::MemoryFill { mem: 0 } => {
                    let size = stack.pop()?;
                    let value = stack.pop()? as u8;
                    let address = stack.pop()?;
                    let relative = address.checked_sub(memory_base)?;
                    let end = relative.checked_add(size)?;
                    address.checked_add(size)?;
                    if value != 0 || end > lib.data_size {
                        return None;
                    }
                    // Uninitialized bytes in this disjoint placement are already zero.
                    for (offset, bytes) in &mut segments {
                        let from = relative.max(*offset);
                        let to = end.min(offset.checked_add(bytes.len().try_into().ok()?)?);
                        if from < to {
                            bytes.to_mut()[(from - *offset) as usize..(to - *offset) as usize]
                                .fill(0);
                        }
                    }
                }
                Operator::End if reader.eof() && stack.is_empty() => {}
                _ => return None,
            }
        }
    }
    (!functions.is_empty()).then_some(RelocatedData {
        functions,
        segments,
    })
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

    #[test]
    fn folds_ordered_initialization_without_removing_callable_functions() {
        let wasm = wat::parse_str(
            r#"(module
            (import "env" "__memory_base" (global i32))
            (import "env" "memory" (memory 1))
            (data (i32.const 0) "abcdefgh")
            (func $zero
                global.get 0 i32.const 0 i32.const 16 memory.fill)
            (start $zero)
            (func $relocate (export "__wasm_apply_data_relocs")
                global.get 0 global.get 0 i32.const 7 i32.add i32.store offset=4)
            (func (export "read") (result i32)
                global.get 0 i32.load offset=4)
            (func (export "reset") call $relocate))"#,
        )
        .unwrap();
        let mut lib = Library::parse(Box::leak(wasm.into_boxed_slice())).unwrap();
        lib.data_size = 16;
        let folded = fold(&lib, 1024, 0, 4096).unwrap();
        assert_eq!(&folded.segments[0].1[..4], &[0; 4]);
        assert_eq!(&folded.segments[0].1[4..], &1031_u32.to_le_bytes());
        assert_eq!(lib.data[0].1, b"abcdefgh");
        let reachable = lib.reachable(["read"], &folded.functions).unwrap();
        assert!(reachable.is_disjoint(&folded.functions));
        let reachable = lib.reachable(["reset"], &folded.functions).unwrap();
        assert!(reachable.contains(&lib.exports["__wasm_apply_data_relocs"]));
        assert!(!reachable.contains(&lib.start.unwrap()));

        // Zeroing outside private storage cannot be moved to static initialization.
        lib.data_size = 8;
        assert!(fold(&lib, 1024, 0, 4096).is_none());
        lib.data_size = 16;
        assert!(fold(&lib, u32::MAX - 8, 0, 4096).is_none());

        // An ordinary call in the initializer preserves the full runtime sequence.
        lib.start = Some(lib.exports["reset"]);
        assert!(fold(&lib, 1024, 0, 4096).is_none());
    }
}
