//! Bounded HTTP requests and responses share the source guest's canonical heap.

use crate::waffle_backend::{
    registry::ModuleRegistry, runtime, runtime::imports, streams, wit::WitWorld,
};
use anyhow::{Result, ensure};
use std::collections::BTreeMap;
use waffle::{Export, ExportKind, Func, Module};

#[derive(Clone, Copy, Debug)]
pub struct HttpHandlerOptions {
    pub max_request_bytes: u32,
    pub max_response_bytes: u32,
}

pub(crate) fn world() -> Result<WitWorld> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("handler.wit", include_str!("handler/world.wit"))?;
    let world = resolve.select_world(&[package], Some("handler"))?;
    WitWorld::new(resolve, world)
}

pub(crate) fn native_world() -> Result<WitWorld> {
    let (resolve, package) = crate::component::wit::resolve_source(
        "package perry:compiler-http; world handler { include wasi:cli/imports@0.3.0; import wasi:http/client@0.3.0; export wasi:http/handler@0.3.0; }",
    )?;
    let world = resolve.select_world(&[package], Some("handler"))?;
    WitWorld::for_encoding(resolve, world)
}

pub(crate) fn emit_entry(
    module: &mut Module<'static>,
    registry: &ModuleRegistry,
    limits: HttpHandlerOptions,
    imports: &BTreeMap<String, Func>,
) -> Result<()> {
    let allocator = registry.allocator.unwrap();
    let finish_write = super::future::emit_finish_write(module, registry.memory, imports)?;
    let functions = native_functions();
    let read = streams::emit_read_transfer(module, registry.memory, imports["read"])?;
    let write = streams::transfer::write(module, registry.memory, imports["write"])?;
    let buffered = streams::buffered::emit(module, registry.memory, allocator, read)?;
    let handle = registry
        .functions
        .values()
        .find_map(|function| {
            function
                .export
                .as_ref()
                .filter(|export| export.name == "handle")
        })
        .unwrap();
    ensure!(
        module.signatures[handle.sig].params == [waffle::Type::I32],
        "HTTP request bridge requires indirect canonical parameters"
    );
    let mut imports: BTreeMap<_, _> = imports
        .iter()
        .map(|(name, &function)| (name.as_str(), function))
        .collect();
    imports.extend([
        ("read-buffered", buffered),
        ("finish-write", finish_write),
        ("write-buffer", write),
        ("realloc", allocator.realloc),
        ("frame-new", allocator.frame_new),
        ("frame-drop", allocator.frame_drop),
        ("cleanup", allocator.post_return),
        ("handle", handle.func_index),
    ]);
    let source = format!(
        "(module {} (import \"host\" \"write-buffer\" (func $write-buffer (param i32 i32 i32) (result i32))) {})",
        imports::module_imports(&functions)?,
        include_str!("handler/runtime.wat")
            .replace("REQUEST_LIMIT", &limits.max_request_bytes.to_string())
            .replace("RESPONSE_LIMIT", &limits.max_response_bytes.to_string())
    );
    let emitted = runtime::emit_functions(module, registry.memory, &source, &imports)?;
    module.exports.push(Export {
        name: "[async-lift-stackful]wasi:http/handler@0.3.0#handle".into(),
        kind: ExportKind::Func(emitted["handle"]),
    });
    Ok(())
}

pub(crate) fn declare_imports(module: &mut Module<'static>) -> BTreeMap<String, Func> {
    imports::declare_imports(module, "http-server", &native_functions())
}

fn native_functions() -> Vec<imports::Function> {
    [
        ("fields", vec!["i32"; 3], vec![]),
        ("copy-fields", vec!["i32"; 2], vec![]),
        ("method", vec!["i32"; 2], vec![]),
        ("scheme", vec!["i32"; 2], vec![]),
        ("authority", vec!["i32"; 2], vec![]),
        ("path", vec!["i32"; 2], vec![]),
        ("headers", vec!["i32"], vec!["i32"]),
        ("consume", vec!["i32"; 3], vec![]),
        ("response", vec!["i32"; 5], vec![]),
        ("status", vec!["i32"; 2], vec!["i32"]),
        ("drop-fields", vec!["i32"], vec![]),
        ("read", vec!["i32"; 3], vec!["i32"]),
        ("drop-reader", vec!["i32"], vec![]),
        ("new-stream", vec![], vec!["i64"]),
        ("write", vec!["i32"; 3], vec!["i32"]),
        ("drop-writer", vec!["i32"], vec![]),
        ("backpressure-inc", vec![], vec![]),
        ("backpressure-dec", vec![], vec![]),
        (
            "return",
            vec!["i32", "i32", "i32", "i64", "i32", "i32", "i32", "i32"],
            vec![],
        ),
    ]
    .into_iter()
    .map(|(name, params, results)| imports::Function {
        name: name.into(),
        params,
        results,
    })
    .chain(super::future::functions())
    .collect()
}
