//! Shared core import declarations for native runtime builders.

use std::collections::BTreeMap;
use std::fmt::Write;

use anyhow::Result;
use waffle::{Func, FuncDecl, Import, ImportKind, Module, SignatureData, Type};

pub(crate) struct Function {
    pub(crate) name: String,
    pub(crate) params: Vec<&'static str>,
    pub(crate) results: Vec<&'static str>,
}

pub(crate) fn declare_imports(
    module: &mut Module<'static>,
    namespace: &str,
    functions: &[Function],
) -> BTreeMap<String, Func> {
    functions
        .iter()
        .map(|function| {
            let core_type = |name: &&str| match *name {
                "i32" => Type::I32,
                "i64" => Type::I64,
                "f64" => Type::F64,
                _ => unreachable!("unsupported adapter core type"),
            };
            let signature = module.signatures.push(SignatureData {
                params: function.params.iter().map(core_type).collect(),
                returns: function.results.iter().map(core_type).collect(),
            });
            let index = module
                .funcs
                .push(FuncDecl::Import(signature, function.name.clone()));
            module.imports.push(Import {
                module: namespace.into(),
                name: function.name.clone(),
                kind: ImportKind::Func(index),
            });
            (function.name.clone(), index)
        })
        .collect()
}

pub(crate) fn module_imports(functions: &[Function]) -> Result<String> {
    let mut wat = String::from("(import \"host\" \"memory\" (memory 1))\n");
    for function in functions {
        writeln!(
            wat,
            "(import \"host\" {0:?} (func ${0} (param {1}) (result {2})))",
            function.name,
            function.params.join(" "),
            function.results.join(" ")
        )?;
    }
    Ok(wat)
}
