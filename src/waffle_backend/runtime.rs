//! Append Wasm support helpers as ordinary, validated WAFFLE functions.

pub(crate) mod builder;
pub(crate) mod callbacks;
pub(crate) mod imports;
pub(crate) mod operations;
pub(crate) mod pending_result;
pub(crate) mod scheduler;
pub(crate) mod subtasks;
pub(crate) mod transfers;

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use waffle::{ExportKind, Func, FuncDecl, ImportKind, Memory, Module, Operator, ValueDef};

pub(crate) fn emit_functions(
    module: &mut Module<'static>,
    memory: Memory,
    source: &str,
    imports: &BTreeMap<&str, Func>,
) -> Result<BTreeMap<String, Func>> {
    let bytes = wat::parse_str(source)?;
    let mut runtime = Module::from_wasm_bytes(&bytes, &Default::default())?;
    runtime.expand_all_funcs()?;
    ensure!(
        runtime.globals.len() == 0 && runtime.tables.len() == 0 && runtime.start_func.is_none(),
        "Support helpers cannot own globals, tables, or initialization"
    );
    ensure!(
        runtime.memories.len() == 1
            && runtime
                .memories
                .iter()
                .all(|id| runtime.memories[id].segments.is_empty()),
        "Support helpers use one existing memory and no data segments"
    );
    let mut functions = BTreeMap::new();
    for import in &runtime.imports {
        if let ImportKind::Func(function) = import.kind {
            let target = *imports
                .get(import.name.as_str())
                .context("Missing support helper import")?;
            ensure!(
                runtime.signatures[runtime.funcs[function].sig()]
                    == module.signatures[module.funcs[target].sig()],
                "Support helper import signature mismatch"
            );
            functions.insert(function, target);
        }
    }
    for (function, declaration) in runtime.funcs.entries() {
        if matches!(declaration, FuncDecl::Body(..)) {
            functions.insert(function, module.funcs.push(FuncDecl::None));
        }
    }
    for (function, declaration) in runtime.funcs.entries_mut() {
        let FuncDecl::Body(signature, name, mut body) =
            std::mem::replace(declaration, FuncDecl::None)
        else {
            continue;
        };
        let signature = module
            .signatures
            .push(runtime.signatures[signature].clone());
        for (_, value) in body.values.entries_mut() {
            if let ValueDef::Operator(operator, _, _) = value {
                match operator {
                    Operator::Call { function_index } => {
                        *function_index = functions[function_index]
                    }
                    Operator::I32Load { memory: arg }
                    | Operator::I32Load8U { memory: arg }
                    | Operator::I64Load { memory: arg }
                    | Operator::F64Load { memory: arg }
                    | Operator::I32Store8 { memory: arg }
                    | Operator::I32Store { memory: arg } => arg.memory = memory,
                    Operator::MemoryFill { mem } => *mem = memory,
                    Operator::MemoryCopy { src_mem, dst_mem } => {
                        *src_mem = memory;
                        *dst_mem = memory;
                    }
                    _ => {}
                }
            }
        }
        body.validate()?;
        body.verify_reducible()?;
        module.funcs[functions[&function]] = FuncDecl::Body(signature, name, body);
    }
    runtime
        .exports
        .into_iter()
        .map(|export| match export.kind {
            ExportKind::Func(function) => Ok((export.name, functions[&function])),
            _ => bail!("Computation helpers export functions only"),
        })
        .collect()
}
