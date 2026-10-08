//! Invocation-scoped P3 HTTP resources over the shared bounded body transfer.

use std::collections::BTreeMap;

use waffle::{Func, Module};

use super::{allocation::AllocationFuncs, runtime::imports};

pub(crate) mod body;
pub(crate) mod fetch;
mod future;
pub(crate) mod handler;
pub(crate) mod headers;
pub(crate) mod operations;
pub(crate) mod request;
pub(crate) mod response;

#[cfg(test)]
#[path = "http/tests.rs"]
mod tests;

pub(crate) struct FetchRuntime<'a> {
    pub(crate) abort: Option<super::abort::Helpers>,
    pub(crate) allocator: AllocationFuncs,
    pub(crate) imports: &'a BTreeMap<String, Func>,
    pub(crate) strings: super::strings::StringHelperFuncs,
    pub(crate) headers: Option<headers::Helpers>,
    pub(crate) request: Option<request::Helpers>,
    pub(crate) pool: &'a super::strings::StringPool,
    pub(crate) promises: Option<&'a super::registry::PromiseImports>,
    pub(crate) operations: Option<super::runtime::operations::Operations>,
}

pub(crate) fn declare_imports(module: &mut Module<'static>, owned: bool) -> BTreeMap<String, Func> {
    let mut functions = native_functions();
    if owned {
        functions.retain(|function| {
            !matches!(
                function.name.as_str(),
                "send"
                    | "read"
                    | "read-trailers"
                    | "read-completion"
                    | "write-trailers"
                    | "write-completion"
            )
        });
    }
    imports::declare_imports(module, "http", &functions)
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
