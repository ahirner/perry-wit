//! Drop unreachable helper bodies while retaining exports and native callback tables.

use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use waffle::entity::EntityRef;
use waffle::{ExportKind, Func, FuncDecl, ImportKind, Module, Operator, ValueDef};

pub(super) fn retain_reachable(module: &mut Module<'_>) -> Result<()> {
    let mut pending = module
        .exports
        .iter()
        .filter_map(|export| match export.kind {
            ExportKind::Func(function) => Some(function),
            _ => None,
        })
        .collect::<Vec<_>>();
    pending.extend(module.start_func);
    for import in &module.imports {
        if let ImportKind::Func(function) = import.kind
            && import.module != super::link::HELPER_MODULE
        {
            pending.push(function);
        }
    }
    for (_, table) in module.tables.entries() {
        if let Some(elements) = &table.func_elements {
            pending.extend(
                elements
                    .iter()
                    .copied()
                    .filter(|function| function.is_valid()),
            );
        }
    }
    let mut reachable = BTreeSet::new();
    while let Some(function) = pending.pop() {
        if !reachable.insert(function) {
            continue;
        }
        match &module.funcs[function] {
            FuncDecl::Body(_, _, body) => {
                for (_, value) in body.values.entries() {
                    if let ValueDef::Operator(
                        Operator::Call { function_index }
                        | Operator::RefFunc {
                            func_index: function_index,
                        },
                        _,
                        _,
                    ) = value
                    {
                        pending.push(*function_index);
                    }
                }
            }
            FuncDecl::Import(..) => {}
            _ => bail!("Reachability requires lowered function bodies"),
        }
    }
    let relocated = reachable
        .iter()
        .enumerate()
        .map(|(index, function)| (*function, Func::from(index as u32)))
        .collect::<BTreeMap<_, _>>();
    let mut functions = Vec::new();
    for (index, mut function) in std::mem::take(&mut module.funcs)
        .into_vec()
        .into_iter()
        .enumerate()
    {
        if !reachable.contains(&Func::from(index as u32)) {
            continue;
        }
        if let FuncDecl::Body(_, _, body) = &mut function {
            for (_, value) in body.values.entries_mut() {
                if let ValueDef::Operator(
                    Operator::Call { function_index }
                    | Operator::RefFunc {
                        func_index: function_index,
                    },
                    _,
                    _,
                ) = value
                {
                    *function_index = relocated[function_index];
                }
            }
        }
        functions.push(function);
    }
    module.funcs = functions.into();
    for export in &mut module.exports {
        if let ExportKind::Func(function) = &mut export.kind {
            *function = relocated[function];
        }
    }
    module.imports.retain(|import| match import.kind {
        ImportKind::Func(function) => reachable.contains(&function),
        _ => true,
    });
    for import in &mut module.imports {
        if let ImportKind::Func(function) = &mut import.kind {
            *function = relocated[function];
        }
    }
    for (_, table) in module.tables.entries_mut() {
        if let Some(elements) = &mut table.func_elements {
            for function in elements.iter_mut().filter(|function| function.is_valid()) {
                *function = relocated[function];
            }
        }
    }
    module.start_func = module.start_func.map(|function| relocated[&function]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_calls_start_and_indirect_callbacks_after_compacting_functions() -> Result<()> {
        let source = wat::parse_str(
            r#"(module
            (type $callback (func (result i32)))
            (import "__perry_helper" "unused" (func $unused (result i64)))
            (global $value (mut i32) (i32.const 0))
            (func $dead (result i64) (call $unused))
            (func $initialize (global.set $value (i32.const 40)))
            (start $initialize)
            (func $callback (result i32) (i32.const 2))
            (table 1 funcref)
            (elem (i32.const 0) $callback)
            (func $direct (result i32) (global.get $value))
            (func (export "run") (result i32)
                (i32.add (call $direct) (call_indirect (type $callback) (i32.const 0)))))"#,
        )?;
        let mut module = Module::from_wasm_bytes(&source, &Default::default())?;
        module.expand_all_funcs()?;
        retain_reachable(&mut module)?;
        assert_eq!(module.funcs.len(), 4);
        let engine = wasmtime::Engine::default();
        let compiled = wasmtime::Module::new(&engine, module.to_wasm_bytes()?)?;
        let mut store = wasmtime::Store::new(&engine, ());
        let instance = wasmtime::Instance::new(&mut store, &compiled, &[])?;
        assert_eq!(
            instance
                .get_typed_func::<(), i32>(&mut store, "run")?
                .call(&mut store, ())?,
            42
        );
        Ok(())
    }
}
