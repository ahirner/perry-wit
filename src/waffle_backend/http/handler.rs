//! Bounded HTTP requests and responses share the source guest's canonical heap.

use crate::waffle_backend::{
    component::forward, registry::ModuleRegistry, runtime, streams, wit::WitWorld,
};
use anyhow::{Result, ensure};
use std::{collections::BTreeMap, fmt::Write};
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

pub(crate) fn emit_entry(
    module: &mut Module<'static>,
    registry: &ModuleRegistry,
    limits: HttpHandlerOptions,
    imports: &BTreeMap<String, Func>,
) -> Result<()> {
    let allocator = registry.allocator.unwrap();
    let functions = native_functions();
    let read = streams::emit_read_transfer(module, registry.memory, imports["read"])?;
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
        ("realloc", allocator.realloc),
        ("frame-new", allocator.frame_new),
        ("frame-drop", allocator.frame_drop),
        ("cleanup", allocator.post_return),
        ("handle", handle.func_index),
    ]);
    let source = format!(
        "(module {} {} {} {})",
        forward::module_imports(&functions)?,
        include_str!("handler/runtime.wat")
            .replace("REQUEST_LIMIT", &limits.max_request_bytes.to_string())
            .replace("RESPONSE_LIMIT", &limits.max_response_bytes.to_string()),
        include_str!("future.wat"),
        include_str!("../streams/write.wat")
    );
    let emitted = runtime::emit_functions(module, registry.memory, &source, &imports)?;
    module.exports.push(Export {
        name: "__perry_http_handle".into(),
        kind: ExportKind::Func(emitted["handle"]),
    });
    Ok(())
}

pub(crate) fn declare_imports(module: &mut Module<'static>) -> BTreeMap<String, Func> {
    forward::declare_imports(module, "http-server", &native_functions())
}

pub(crate) fn declare_adapters() -> Result<String> {
    forward::declare("http-server", &native_functions())
}

pub(crate) fn bind_adapters() -> Result<String> {
    let mut wat = String::new();
    for (name, method) in [
        ("fields", "[static]fields.from-list"),
        ("copy-fields", "[method]fields.copy-all"),
        ("method", "[method]request.get-method"),
        ("scheme", "[method]request.get-scheme"),
        ("authority", "[method]request.get-authority"),
        ("path", "[method]request.get-path-with-query"),
        ("headers", "[method]request.get-headers"),
        ("consume", "[static]request.consume-body"),
        ("response", "[static]response.new"),
        ("status", "[method]response.set-status-code"),
    ] {
        writeln!(
            wat,
            "(core func $server-{name} (canon lower (func $http-types {method:?}) (memory (core memory $guest \"memory\")) (realloc (core func $guest \"cabi_realloc\"))))"
        )?;
    }
    wat.push_str(r#"
      (core func $server-drop-fields (canon resource.drop $http-fields))
      (core func $server-read (canon stream.read $http-bytes (memory (core memory $guest "memory"))))
      (core func $server-drop-reader (canon stream.drop-readable $http-bytes))
      (core func $server-new-stream (canon stream.new $http-bytes))
      (core func $server-write (canon stream.write $http-bytes (memory (core memory $guest "memory"))))
      (core func $server-drop-writer (canon stream.drop-writable $http-bytes))
      (core func $server-return (canon task.return (result (result (own $http-response) (error $http-error))) (memory (core memory $guest "memory"))))
      (core func $server-backpressure-inc (canon backpressure.inc))
      (core func $server-backpressure-dec (canon backpressure.dec))
    "#);
    wat.push_str(&super::future::bindings("server"));
    wat.push_str(&forward::bind("http-server", &native_functions())?);
    Ok(wat)
}

pub(crate) fn export_adapter() -> &'static str {
    r#"(func $http-handle async (param "request" (own $http-request)) (result (result (own $http-response) (error $http-error)))
      (canon lift (core func $guest "__perry_http_handle") async (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))))
    (instance $http-handler (export "request" (type $http-request)) (export "response" (type $http-response))
      (export "error-code" (type $http-error)) (export "handle" (func $http-handle)))
    (export "wasi:http/handler@0.3.0" (instance $http-handler))"#
}

fn native_functions() -> Vec<forward::Function> {
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
    .map(|(name, params, results)| forward::Function {
        name: name.into(),
        params,
        results,
        target: format!("(func $server-{name})"),
    })
    .chain(super::future::functions("server"))
    .collect()
}
