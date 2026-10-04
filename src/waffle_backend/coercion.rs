//! String coercion of guest primitives, plain objects, and nested arrays.

use super::{allocation::AllocationFuncs, runtime, strings};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{Func, Memory, Module};

pub(crate) const LITERALS: [&str; 8] = [
    "undefined",
    "null",
    "true",
    "false",
    "[object Object]",
    "NaN",
    "Infinity",
    "-Infinity",
];

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    strings: strings::StringHelperFuncs,
    serializer: Func,
    pool: &strings::StringPool,
) -> Result<Func> {
    let mut source = include_str!("coercion/runtime.wat").to_owned();
    for name in LITERALS {
        source = source.replace(
            &format!("{{{{{name}}}}}"),
            &pool.get(name).expect("coercion literal").to_string(),
        );
    }
    let functions = runtime::emit_functions(
        module,
        memory,
        &source,
        &BTreeMap::from([
            ("realloc", allocator.realloc),
            ("lift", strings.lift_canonical),
            ("serialize", serializer),
        ]),
    )?;
    Ok(functions["value.to-string"])
}
