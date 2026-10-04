//! Preopen-confined filesystem operations over the shared native byte transfers.

use std::collections::BTreeMap;

use anyhow::Result;
use perry_hir::types::Type as HirType;
use waffle::{Func, Memory, Module};

use super::{allocation::AllocationFuncs, runtime, runtime::imports};

pub(crate) const OPTION_KEYS: [&str; 6] = [
    "encoding",
    "flag",
    "bigint",
    "throwIfNoEntry",
    "recursive",
    "withFileTypes",
];

#[derive(Clone, Copy)]
pub(crate) struct FilesystemHelpers {
    pub(crate) write: Func,
    pub(crate) write_options: Func,
    pub(crate) read: Func,
    pub(crate) read_options: Func,
    pub(crate) metadata: Func,
    pub(crate) read_directory: Func,
    pub(crate) directory_options: Func,
    pub(crate) read_object_options: Func,
    pub(crate) write_object_options: Func,
    pub(crate) metadata_object_options: Func,
}

pub(crate) fn is_stats(ty: &HirType) -> bool {
    matches!(ty, HirType::Named(name) if name == "Stats")
}

pub(crate) fn declare_imports(module: &mut Module<'static>) -> BTreeMap<String, Func> {
    imports::declare_imports(module, "filesystem", &native_functions())
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    imports: BTreeMap<String, Func>,
    compare: Func,
    keys: &super::strings::StringPool,
) -> Result<FilesystemHelpers> {
    let read_transfer = super::streams::emit_read_transfer(module, memory, imports["read"])?;
    let read_directory_transfer =
        super::streams::emit_read_transfer(module, memory, imports["read-entry"])?;
    let read_buffered = super::streams::buffered::emit(module, memory, allocator, read_transfer)?;
    let mut object_options = include_str!("filesystem/object-options.wat").to_string();
    for key in OPTION_KEYS {
        object_options = object_options.replace(
            &format!("__KEY_{key}__"),
            &keys
                .get(key)
                .expect("filesystem keys are interned")
                .to_string(),
        );
    }
    let wat = format!(
        r#"(module {}
      (import "host" "realloc" (func $realloc (param i32 i32 i32 i32) (result i32)))
      (import "host" "frame-new" (func $frame-new (param i32) (result i32)))
      (import "host" "frame-drop" (func $frame-drop (param i32)))
      (import "host" "compare" (func $compare (param i32 i32) (result i32)))
      (import "host" "read-buffered" (func $read-buffered (param i32 i32) (result i32 i32 i32)))
      (import "host" "read-directory-transfer" (func $read-directory-transfer (param i32 i32 i32) (result i32 i32)))
      {} {} {} {} {} {} {} {} {})"#,
        imports::module_imports(&native_functions())?,
        include_str!("streams/write.wat"),
        include_str!("strings/utf8.wat"),
        include_str!("filesystem/options.wat"),
        include_str!("filesystem/path.wat"),
        include_str!("filesystem/runtime.wat"),
        include_str!("filesystem/read.wat"),
        include_str!("filesystem/metadata.wat"),
        include_str!("filesystem/directory.wat"),
        object_options,
    );
    let mut imports: BTreeMap<_, _> = imports
        .iter()
        .map(|(name, &index)| (name.as_str(), index))
        .collect();
    imports.extend([
        ("realloc", allocator.realloc),
        ("frame-new", allocator.frame_new),
        ("frame-drop", allocator.frame_drop),
        ("compare", compare),
        ("read-buffered", read_buffered),
        ("read-directory-transfer", read_directory_transfer),
    ]);
    let functions = runtime::emit_functions(module, memory, &wat, &imports)?;
    Ok(FilesystemHelpers {
        write: functions["fs.write"],
        write_options: functions["fs.write-options"],
        read: functions["fs.read"],
        read_options: functions["fs.read-options"],
        metadata: functions["fs.metadata"],
        read_directory: functions["fs.read-directory"],
        directory_options: functions["fs.directory-options"],
        read_object_options: functions["fs.read-object-options"],
        write_object_options: functions["fs.write-object-options"],
        metadata_object_options: functions["fs.metadata-object-options"],
    })
}

fn native_functions() -> Vec<imports::Function> {
    [
        ("directories", vec!["i32"], vec![]),
        ("open", vec!["i32"; 7], vec![]),
        ("start-write", vec!["i32", "i32", "i64"], vec!["i32"]),
        ("start-read", vec!["i32", "i64", "i32"], vec![]),
        ("stat", vec!["i32"; 5], vec![]),
        ("mkdir", vec!["i32"; 4], vec![]),
        ("unlink", vec!["i32"; 4], vec![]),
        ("rmdir", vec!["i32"; 4], vec![]),
        ("start-directory", vec!["i32"; 2], vec![]),
        ("read-entry", vec!["i32"; 3], vec!["i32"]),
        ("drop-entries", vec!["i32"], vec![]),
        ("drop-descriptor", vec!["i32"], vec![]),
        ("await", vec!["i32"; 2], vec!["i32"]),
        ("drop-future", vec!["i32"], vec![]),
        ("new", vec![], vec!["i64"]),
        ("write", vec!["i32"; 3], vec!["i32"]),
        ("drop-writer", vec!["i32"], vec![]),
        ("read", vec!["i32"; 3], vec!["i32"]),
        ("drop-reader", vec!["i32"], vec![]),
    ]
    .into_iter()
    .map(|(name, params, results)| imports::Function {
        name: name.into(),
        params,
        results,
    })
    .collect()
}
