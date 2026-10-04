//! Preopen-confined filesystem operations over the shared native byte transfers.

use std::collections::BTreeMap;

use anyhow::Result;
use perry_hir::types::Type as HirType;
use waffle::{Func, Memory, Module};

use super::{allocation::AllocationFuncs, component::forward, runtime};

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
    forward::declare_imports(module, "filesystem", &native_functions())
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    imports: BTreeMap<String, Func>,
    compare: Func,
    keys: &super::strings::StringPool,
) -> Result<FilesystemHelpers> {
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
      {} {} {} {} {} {} {} {} {} {} {})"#,
        forward::module_imports(&native_functions())?,
        include_str!("streams/write.wat"),
        include_str!("streams/read.wat"),
        include_str!("streams/read.wat")
            .replace("$read-transfer", "$read-directory-transfer")
            .replace("(call $read ", "(call $read-entry "),
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

pub(crate) fn declare_adapters() -> Result<String> {
    Ok(format!(
        "{}{}",
        include_str!("filesystem/interfaces.wat"),
        forward::declare("filesystem", &native_functions())?
    ))
}

pub(crate) fn bind_adapters() -> Result<String> {
    Ok(format!(
        r#"
      (core func $fs-directories (canon lower (func $fs-preopens "get-directories")
        (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))))
      (core func $fs-open (canon lower (func $fs-types "[method]descriptor.open-at")
        (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))))
      (core func $fs-start-write (canon lower (func $fs-types "[method]descriptor.write-via-stream")))
      (core func $fs-start-read (canon lower (func $fs-types "[method]descriptor.read-via-stream")
        (memory (core memory $guest "memory"))))
      (core func $fs-stat (canon lower (func $fs-types "[method]descriptor.stat-at")
        (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))))
      (core func $fs-mkdir (canon lower (func $fs-types "[method]descriptor.create-directory-at")
        (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))))
      (core func $fs-unlink (canon lower (func $fs-types "[method]descriptor.unlink-file-at")
        (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))))
      (core func $fs-rmdir (canon lower (func $fs-types "[method]descriptor.remove-directory-at")
        (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))))
      (core func $fs-start-directory (canon lower (func $fs-types "[method]descriptor.read-directory")
        (memory (core memory $guest "memory"))))
      (core func $fs-read-entry (canon stream.read $fs-entries
        (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))))
      (core func $fs-drop-entries (canon stream.drop-readable $fs-entries))
      (core func $fs-drop-descriptor (canon resource.drop $fs-descriptor))
      (core func $fs-await (canon future.read $fs-completion (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))))
      (core func $fs-drop-future (canon future.drop-readable $fs-completion))
      (core func $fs-new (canon stream.new $fs-bytes))
      (core func $fs-write (canon stream.write $fs-bytes (memory (core memory $guest "memory"))))
      (core func $fs-drop-writer (canon stream.drop-writable $fs-bytes))
      (core func $fs-read (canon stream.read $fs-bytes (memory (core memory $guest "memory"))))
      (core func $fs-drop-reader (canon stream.drop-readable $fs-bytes))
      {}"#,
        forward::bind("filesystem", &native_functions())?
    ))
}

fn native_functions() -> Vec<forward::Function> {
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
    .map(|(name, params, results)| forward::Function {
        name: name.into(),
        params,
        results,
        target: format!("(func $fs-{name})"),
    })
    .collect()
}
