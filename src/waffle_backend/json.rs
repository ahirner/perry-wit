//! Conversion between shared guest values and the confined Rust JSON codec graph.

use super::{
    allocation::AllocationFuncs, objects::ObjectHelpers, runtime, strings::StringHelperFuncs,
    values::ValueTag,
};
use anyhow::Result;
use std::collections::BTreeMap;
use waffle::{Func, FuncDecl, Import, ImportKind, Memory, Module, SignatureData, Type};

#[derive(Clone, Copy)]
pub(crate) struct JsonHelpers {
    pub(crate) parse: Func,
    pub(crate) stringify: Func,
}

pub(crate) fn declare_imports(module: &mut Module<'static>) -> BTreeMap<&'static str, Func> {
    [
        ("measure", "json_measure", 2),
        ("populate", "json_populate", 4),
        ("serialized-size", "json_serialized_size", 3),
        ("serialize", "json_serialize", 5),
    ]
    .into_iter()
    .map(|(local, name, arity)| (local, declare_import(module, name, arity)))
    .collect()
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    strings: StringHelperFuncs,
    objects: ObjectHelpers,
    boxed_value: Func,
    mut imports: BTreeMap<&str, Func>,
) -> Result<JsonHelpers> {
    imports.extend([
        ("realloc", allocator.realloc),
        ("box", boxed_value),
        ("lift", strings.lift_canonical),
        ("object-new", objects.new),
        ("object-set", objects.set),
    ]);
    let source = include_str!("json/runtime.wat")
        .replace("{{array-tag}}", &(ValueTag::Array as u32).to_string());
    let functions = runtime::emit_functions(module, memory, &source, &imports)?;
    Ok(JsonHelpers {
        parse: functions["json.parse"],
        stringify: functions["json.stringify"],
    })
}

fn declare_import(module: &mut Module<'static>, name: &str, arity: usize) -> Func {
    let signature = module.signatures.push(SignatureData {
        params: vec![Type::I32; arity],
        returns: vec![Type::I64],
    });
    let function = module.funcs.push(FuncDecl::Import(signature, name.into()));
    module.imports.push(Import {
        module: super::link::HELPER_MODULE.into(),
        name: name.into(),
        kind: ImportKind::Func(function),
    });
    function
}
