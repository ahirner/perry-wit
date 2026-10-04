//! Invocation-scoped P3 HTTP resources over the shared bounded body transfer.

use std::collections::BTreeMap;
use std::fmt::Write;

use anyhow::Result;
use perry_hir::types::{ObjectType, Type as HirType};
use waffle::{Func, Memory, Module};

use super::{allocation::AllocationFuncs, component::forward, runtime, streams};

#[cfg(test)]
#[path = "http/tests.rs"]
mod tests;

pub(crate) const RESPONSE_TYPE: &str = "__perry_http_response";

pub(crate) fn is_response(ty: &HirType) -> bool {
    matches!(ty, HirType::Named(name) if name == RESPONSE_TYPE)
}
pub(crate) fn headers_type() -> HirType {
    HirType::Object(ObjectType {
        property_order: Some(Vec::new()),
        index_signature: Some(Box::new(HirType::String)),
        ..Default::default()
    })
}

#[derive(Clone, Copy)]
pub(crate) struct HttpHelpers {
    pub(crate) get: Func,
    pub(crate) header: Func,
}

pub(crate) fn emit_source_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    imports: &BTreeMap<String, Func>,
    strings: super::strings::StringHelperFuncs,
    bytes: super::bytes::ByteHelpers,
    pool: &super::strings::StringPool,
) -> Result<HttpHelpers> {
    let get = emit_runtime(module, memory, allocator, imports)?;
    let source = include_str!("http/source.wat")
        .replace("HTTP_TEXT", &pool.get("http").unwrap().to_string())
        .replace("HTTPS_TEXT", &pool.get("https").unwrap().to_string());
    let functions = runtime::emit_functions(
        module,
        memory,
        &source,
        &BTreeMap::from([
            ("get", get),
            ("realloc", allocator.realloc),
            ("frame-new", allocator.frame_new),
            ("frame-drop", allocator.frame_drop),
            ("compare", strings.str_compare),
            ("string-lift", strings.lift_canonical),
            ("bytes-lift", bytes.lift_canonical),
        ]),
    )?;
    Ok(HttpHelpers {
        get: functions["get"],
        header: functions["header"],
    })
}

pub(crate) fn declare_imports(module: &mut Module<'static>) -> BTreeMap<String, Func> {
    forward::declare_imports(module, "http", &native_functions())
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    imports: &BTreeMap<String, Func>,
) -> Result<Func> {
    let transfer = streams::emit_read_transfer(module, memory, imports["read"])?;
    let buffered = streams::buffered::emit(module, memory, allocator, transfer)?;
    let mut imports: BTreeMap<_, _> = imports
        .iter()
        .map(|(name, &function)| (name.as_str(), function))
        .collect();
    imports.extend([
        ("read-buffered", buffered),
        ("realloc", allocator.realloc),
        ("frame-new", allocator.frame_new),
        ("frame-drop", allocator.frame_drop),
    ]);
    let source = format!(
        "(module {} {})",
        forward::module_imports(&native_functions())?,
        include_str!("http/runtime.wat")
    );
    Ok(runtime::emit_functions(module, memory, &source, &imports)?["http.get"])
}

pub(crate) fn declare_adapters() -> Result<String> {
    Ok(format!(
        "{}{}",
        include_str!("http/interfaces.wat"),
        forward::declare("http", &native_functions())?
    ))
}

pub(crate) fn bind_adapters() -> Result<String> {
    let mut wat = String::new();
    for (name, method) in [
        ("fields", "[static]fields.from-list"),
        ("copy-fields", "[method]fields.copy-all"),
        ("request", "[static]request.new"),
        ("scheme", "[method]request.set-scheme"),
        ("authority", "[method]request.set-authority"),
        ("path", "[method]request.set-path-with-query"),
        ("status", "[method]response.get-status-code"),
        ("headers", "[method]response.get-headers"),
        ("consume", "[static]response.consume-body"),
    ] {
        writeln!(
            wat,
            "(core func $http-{name} (canon lower (func $http-types {method:?}) (memory (core memory $guest \"memory\")) (realloc (core func $guest \"cabi_realloc\"))))"
        )?;
    }
    wat.push_str(r#"
      (core func $http-send (canon lower (func $http-client "send") (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))))
      (core func $http-drop-fields (canon resource.drop $http-fields))
      (core func $http-drop-request (canon resource.drop $http-request))
      (core func $http-read (canon stream.read $http-bytes (memory (core memory $guest "memory"))))
      (core func $http-drop-reader (canon stream.drop-readable $http-bytes))
      (core func $http-new-trailers (canon future.new $http-trailers))
      (core func $http-write-trailers (canon future.write $http-trailers async (memory (core memory $guest "memory"))))
      (core func $http-read-trailers (canon future.read $http-trailers (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))))
      (core func $http-drop-trailers-reader (canon future.drop-readable $http-trailers))
      (core func $http-drop-trailers-writer (canon future.drop-writable $http-trailers))
      (core func $http-new-completion (canon future.new $http-completion))
      (core func $http-write-completion (canon future.write $http-completion async (memory (core memory $guest "memory"))))
      (core func $http-read-completion (canon future.read $http-completion (memory (core memory $guest "memory")) (realloc (core func $guest "cabi_realloc"))))
      (core func $http-drop-completion-reader (canon future.drop-readable $http-completion))
      (core func $http-drop-completion-writer (canon future.drop-writable $http-completion))
      (core func $http-new-set (canon waitable-set.new))
      (core func $http-join (canon waitable.join))
      (core func $http-wait (canon waitable-set.wait (memory (core memory $guest "memory"))))
      (core func $http-drop-set (canon waitable-set.drop))
    "#);
    wat.push_str(&forward::bind("http", &native_functions())?);
    Ok(wat)
}

fn native_functions() -> Vec<forward::Function> {
    [
        ("fields", vec!["i32"; 3], vec![]),
        ("copy-fields", vec!["i32"; 2], vec![]),
        ("request", vec!["i32"; 7], vec![]),
        ("scheme", vec!["i32"; 5], vec!["i32"]),
        ("authority", vec!["i32"; 4], vec!["i32"]),
        ("path", vec!["i32"; 4], vec!["i32"]),
        ("status", vec!["i32"], vec!["i32"]),
        ("headers", vec!["i32"], vec!["i32"]),
        ("consume", vec!["i32"; 3], vec![]),
        ("send", vec!["i32"; 2], vec![]),
        ("drop-fields", vec!["i32"], vec![]),
        ("drop-request", vec!["i32"], vec![]),
        ("read", vec!["i32"; 3], vec!["i32"]),
        ("drop-reader", vec!["i32"], vec![]),
        ("new-trailers", vec![], vec!["i64"]),
        ("write-trailers", vec!["i32"; 2], vec!["i32"]),
        ("read-trailers", vec!["i32"; 2], vec!["i32"]),
        ("drop-trailers-reader", vec!["i32"], vec![]),
        ("drop-trailers-writer", vec!["i32"], vec![]),
        ("new-completion", vec![], vec!["i64"]),
        ("write-completion", vec!["i32"; 2], vec!["i32"]),
        ("read-completion", vec!["i32"; 2], vec!["i32"]),
        ("drop-completion-reader", vec!["i32"], vec![]),
        ("drop-completion-writer", vec!["i32"], vec![]),
        ("new-set", vec![], vec!["i32"]),
        ("join", vec!["i32"; 2], vec![]),
        ("wait", vec!["i32"; 2], vec!["i32"]),
        ("drop-set", vec!["i32"], vec![]),
    ]
    .into_iter()
    .map(|(name, params, results)| forward::Function {
        name: name.into(),
        params,
        results,
        target: format!("(func $http-{name})"),
    })
    .collect()
}
