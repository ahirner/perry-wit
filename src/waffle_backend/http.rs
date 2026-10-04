//! Invocation-scoped P3 HTTP resources over the shared bounded body transfer.

use std::collections::BTreeMap;

use anyhow::Result;
use perry_hir::types::{ObjectType, Type as HirType};
use waffle::{Func, Memory, Module};

use super::{allocation::AllocationFuncs, runtime, runtime::imports, streams};

pub(crate) mod fetch;
mod future;
pub(crate) mod handler;
pub(crate) mod headers;

#[cfg(test)]
#[path = "http/tests.rs"]
mod tests;

pub(crate) const RESPONSE_TYPE: &str = "__perry_http_response";

pub(crate) fn is_response(ty: &HirType) -> bool {
    matches!(ty, HirType::Named(name) if name == RESPONSE_TYPE) || fetch::is_response(ty)
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
    pub(crate) fetch: Option<fetch::Helpers>,
}

pub(crate) struct SourceRuntime<'a> {
    pub(crate) allocator: AllocationFuncs,
    pub(crate) imports: &'a BTreeMap<String, Func>,
    pub(crate) strings: super::strings::StringHelperFuncs,
    pub(crate) bytes: super::bytes::ByteHelpers,
    pub(crate) json: Option<super::json::JsonHelpers>,
    pub(crate) values: super::values::ValueHelpers,
    pub(crate) pool: &'a super::strings::StringPool,
    pub(crate) promises: Option<&'a super::registry::PromiseImports>,
}

pub(crate) fn emit_source_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    runtime: SourceRuntime<'_>,
) -> Result<HttpHelpers> {
    let SourceRuntime {
        allocator,
        imports,
        strings,
        bytes,
        pool,
        ..
    } = runtime;
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
        fetch: imports
            .contains_key("fetch_url")
            .then(|| fetch::emit(module, memory, &runtime))
            .transpose()?,
        header: functions["header"],
    })
}

pub(crate) fn declare_imports(module: &mut Module<'static>) -> BTreeMap<String, Func> {
    imports::declare_imports(module, "http", &native_functions())
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    imports: &BTreeMap<String, Func>,
) -> Result<Func> {
    let finish_write = future::emit_finish_write(module, memory, imports)?;
    let transfer = streams::emit_read_transfer(module, memory, imports["read"])?;
    let buffered = streams::buffered::emit(module, memory, allocator, transfer)?;
    let mut imports: BTreeMap<_, _> = imports
        .iter()
        .map(|(name, &function)| (name.as_str(), function))
        .collect();
    imports.extend([
        ("read-buffered", buffered),
        ("finish-write", finish_write),
        ("realloc", allocator.realloc),
        ("frame-new", allocator.frame_new),
        ("frame-drop", allocator.frame_drop),
    ]);
    let source = format!(
        "(module {} {})",
        imports::module_imports(&native_functions())?,
        include_str!("http/runtime.wat")
    );
    Ok(runtime::emit_functions(module, memory, &source, &imports)?["http.get"])
}

fn native_functions() -> Vec<imports::Function> {
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
    ]
    .into_iter()
    .map(|(name, params, results)| imports::Function {
        name: name.into(),
        params,
        results,
    })
    .chain(future::functions())
    .collect()
}
