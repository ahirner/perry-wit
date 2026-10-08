//! Specialize internal functions only when every direct caller supplies the same constant.

use std::collections::{BTreeMap, BTreeSet};

use waffle::{
    ExportKind, Func, FuncDecl, FunctionBody, Module, Operator, Terminator, Value, ValueDef,
};

fn constant(body: &FunctionBody, value: Value) -> Option<Operator> {
    match body.values[body.resolve_alias(value)] {
        ValueDef::Operator(
            op @ (Operator::I32Const { .. }
            | Operator::I64Const { .. }
            | Operator::F32Const { .. }
            | Operator::F64Const { .. }),
            _,
            _,
        ) => Some(op),
        _ => None,
    }
}

/// These immutable task slots are written only by their matching start function.
pub(crate) struct CapturedArguments {
    pub(crate) start: Func,
    pub(crate) worker: Func,
    pub(crate) loads: Vec<Value>,
}

pub(super) fn optimize(module: &mut Module<'_>, captures: &[CapturedArguments]) {
    loop {
        for (_, function) in module.funcs.entries_mut() {
            if let FuncDecl::Body(_, _, body) = function {
                loop {
                    body.optimize(&Default::default());
                    let changed = simplify_values(body);
                    if !fold_branches(body) && !changed {
                        break;
                    }
                }
            }
        }
        let (exposed, arguments) = call_arguments(module);
        let mut changed = false;
        for capture in captures {
            let Some(arguments) = arguments.get(&capture.start) else {
                continue;
            };
            let body = module.funcs[capture.worker].body_mut().unwrap();
            for (&value, argument) in capture.loads.iter().zip(&arguments[1..]) {
                if let Some(argument) = argument
                    && let ValueDef::Operator(op, args, _) = &mut body.values[value]
                    && *op != *argument
                {
                    *op = *argument;
                    *args = Default::default();
                    changed = true;
                }
            }
        }
        changed |= specialize(module, exposed, arguments);
        if !changed {
            break;
        }
    }
    remove_unused_parameters(module);
}

fn simplify_values(body: &mut FunctionBody) -> bool {
    let mut changed = false;
    for value in body.values.iter().collect::<Vec<_>>() {
        let ValueDef::Operator(op, args, _) = body.values[value] else {
            continue;
        };
        let args = &body.arg_pool[args];
        let replacement = match (op, args) {
            (Operator::Select | Operator::TypedSelect { .. }, &[yes, no, condition]) => {
                if body.resolve_alias(yes) == body.resolve_alias(no) {
                    Some(yes)
                } else if let Some(Operator::I32Const { value }) = constant(body, condition) {
                    Some(if value == 0 { no } else { yes })
                } else {
                    None
                }
            }
            (
                Operator::I32And
                | Operator::I32Or
                | Operator::I32Xor
                | Operator::I32Add
                | Operator::I32Mul,
                &[left, right],
            ) => {
                let (other, known) =
                    if matches!(constant(body, right), Some(Operator::I32Const { .. })) {
                        (left, right)
                    } else {
                        (right, left)
                    };
                match (op, constant(body, known)) {
                    (
                        Operator::I32And | Operator::I32Mul,
                        Some(Operator::I32Const { value: 0 }),
                    ) => Some(known),
                    (Operator::I32Or, Some(Operator::I32Const { value: u32::MAX })) => Some(known),
                    (Operator::I32And, Some(Operator::I32Const { value: u32::MAX }))
                    | (Operator::I32Mul, Some(Operator::I32Const { value: 1 }))
                    | (
                        Operator::I32Or | Operator::I32Xor | Operator::I32Add,
                        Some(Operator::I32Const { value: 0 }),
                    ) => Some(other),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            body.values[value] = ValueDef::Alias(body.resolve_alias(replacement));
            changed = true;
        }
    }
    changed
}

fn remove_unused_parameters(module: &mut Module<'_>) {
    let (exposed, _) = call_arguments(module);
    let mut retained = BTreeMap::new();
    for (function, declaration) in module.funcs.entries_mut() {
        if exposed.contains(&function) {
            continue;
        }
        let FuncDecl::Body(signature, _, body) = declaration else {
            continue;
        };
        let mut used = BTreeSet::new();
        for (_, value) in body.values.entries() {
            if !matches!(value, ValueDef::None) {
                value.visit_uses(&body.arg_pool, |value| {
                    used.insert(value);
                });
            }
        }
        for (_, block) in body.blocks.entries() {
            block.terminator.visit_uses(|value| {
                used.insert(value);
            });
        }
        let keep = body.blocks[body.entry]
            .params
            .iter()
            .map(|(_, value)| used.contains(value))
            .collect::<Vec<_>>();
        if keep.iter().all(|keep| *keep) {
            continue;
        }
        let parameters = std::mem::take(&mut body.blocks[body.entry].params);
        for ((ty, value), keep) in parameters.into_iter().zip(&keep) {
            if *keep {
                let index = body.blocks[body.entry].params.len() as u32;
                body.values[value] = ValueDef::BlockParam(body.entry, index, ty);
                body.blocks[body.entry].params.push((ty, value));
            } else {
                body.values[value] = ValueDef::None;
            }
        }
        let params = body.blocks[body.entry]
            .params
            .iter()
            .map(|(ty, _)| *ty)
            .collect::<Vec<_>>();
        let locals = params
            .iter()
            .copied()
            .chain(body.locals.values().skip(body.n_params).copied())
            .collect::<Vec<_>>();
        body.locals = locals.into();
        body.n_params = params.len();
        body.value_locals = Default::default();
        *signature = module.signatures.push(waffle::SignatureData {
            params,
            returns: body.rets.clone(),
        });
        retained.insert(function, keep);
    }
    for (_, function) in module.funcs.entries_mut() {
        let Some(body) = function.body_mut() else {
            continue;
        };
        for (_, value) in body.values.entries_mut() {
            if let ValueDef::Operator(Operator::Call { function_index }, args, _) = value
                && let Some(keep) = retained.get(function_index)
            {
                let values = body.arg_pool[*args]
                    .iter()
                    .zip(keep)
                    .filter_map(|(&value, &keep)| keep.then_some(value))
                    .collect::<Vec<_>>();
                *args = body.arg_pool.from_iter(values.into_iter());
            }
        }
    }
}

fn fold_branches(body: &mut FunctionBody) -> bool {
    let mut changed = false;
    for block in body.blocks.iter().collect::<Vec<_>>() {
        let target = match &body.blocks[block].terminator {
            Terminator::CondBr {
                cond,
                if_true,
                if_false,
            } => match constant(body, *cond) {
                Some(Operator::I32Const { value }) => {
                    Some(if value != 0 { if_true } else { if_false })
                }
                _ => None,
            },
            Terminator::Select {
                value,
                targets,
                default,
            } => match constant(body, *value) {
                Some(Operator::I32Const { value }) => {
                    Some(targets.get(value as usize).unwrap_or(default))
                }
                _ => None,
            },
            _ => None,
        }
        .cloned();
        if let Some(target) = target {
            body.blocks[block].terminator = Terminator::Br { target };
            changed = true;
        }
    }
    body.recompute_edges();
    let cfg = waffle::cfg::CFGInfo::new(body);
    for (block, definition) in body.blocks.entries_mut() {
        if cfg.rpo_pos[block].is_none() {
            definition.insts.clear();
            definition.terminator = Terminator::Unreachable;
        }
    }
    body.recompute_edges();
    changed
}

fn call_arguments(module: &Module<'_>) -> (BTreeSet<Func>, BTreeMap<Func, Vec<Option<Operator>>>) {
    let mut exposed = BTreeSet::new();
    for export in &module.exports {
        if let ExportKind::Func(function) = export.kind {
            exposed.insert(function);
        }
    }
    exposed.extend(module.start_func);
    for (_, table) in module.tables.entries() {
        if let Some(elements) = &table.func_elements {
            exposed.extend(elements);
        }
    }
    let mut arguments: BTreeMap<Func, Vec<Option<Operator>>> = BTreeMap::new();
    for (_, function) in module.funcs.entries() {
        let Some(body) = function.body() else {
            continue;
        };
        for (_, block) in body.blocks.entries() {
            for &value in &block.insts {
                match body.values[body.resolve_alias(value)] {
                    ValueDef::Operator(Operator::RefFunc { func_index }, _, _) => {
                        exposed.insert(func_index);
                    }
                    ValueDef::Operator(Operator::Call { function_index }, args, _) => {
                        let values = &body.arg_pool[args];
                        arguments
                            .entry(function_index)
                            .and_modify(|known| {
                                for (known, &value) in known.iter_mut().zip(values) {
                                    if *known != constant(body, value) {
                                        *known = None;
                                    }
                                }
                            })
                            .or_insert_with(|| {
                                values.iter().map(|&value| constant(body, value)).collect()
                            });
                    }
                    _ => {}
                }
            }
        }
    }
    (exposed, arguments)
}

fn specialize(
    module: &mut Module<'_>,
    exposed: BTreeSet<Func>,
    arguments: BTreeMap<Func, Vec<Option<Operator>>>,
) -> bool {
    let mut changed = false;
    for (function, arguments) in arguments {
        if exposed.contains(&function) {
            continue;
        }
        let Some(body) = module.funcs[function].body_mut() else {
            continue;
        };
        for (index, argument) in arguments.into_iter().enumerate() {
            let Some(argument) = argument else { continue };
            let (ty, parameter) = body.blocks[body.entry].params[index];
            let mut used = false;
            for (_, value) in body.values.entries() {
                if !matches!(value, ValueDef::None) {
                    value.visit_uses(&body.arg_pool, |value| used |= value == parameter);
                }
            }
            for (_, block) in body.blocks.entries() {
                block
                    .terminator
                    .visit_uses(|value| used |= value == parameter);
            }
            if !used {
                continue;
            }
            let instructions = std::mem::take(&mut body.blocks[body.entry].insts);
            let replacement = body.add_op(body.entry, argument, &[], &[ty]);
            body.blocks[body.entry].insts.extend(instructions);
            let replace = |value: &mut Value| {
                if *value == parameter {
                    *value = replacement;
                }
            };
            for (_, value) in body.values.entries_mut() {
                if !matches!(value, ValueDef::None) {
                    value.update_uses(&mut body.arg_pool, replace);
                }
            }
            for (_, block) in body.blocks.entries_mut() {
                block.terminator.update_uses(replace);
            }
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specializes_private_calls_without_changing_public_or_indirect_arguments()
    -> anyhow::Result<()> {
        let source = wat::parse_str(
            r#"(module
            (type $callback (func (param i32) (result i32)))
            (global $calls (mut i32) (i32.const 0))
            (func $dead (result i32) unreachable)
            (func $private (param i32) (result i32)
                (global.set $calls (i32.add (global.get $calls) (i32.const 1)))
                (if (result i32) (local.get 0) (then (call $dead)) (else (i32.const 10))))
            (func $forward (param i32) (result i32) (call $private (local.get 0)))
            (func $mixed (param i32) (result i32) (local.get 0))
            (func $ignored (param i32) (result i32) (i32.const 0))
            (func $effect (result i32)
                (global.set $calls (i32.add (global.get $calls) (i32.const 1)))
                (i32.const 2))
            (func $callback (type $callback) (param i32) (result i32) (local.get 0))
            (func $public (export "public") (param i32) (result i32) (local.get 0))
            (table 1 funcref)
            (elem (i32.const 0) $callback)
            (func (export "run") (result i32)
                (drop (call $forward (i32.const 0)))
                (drop (call $callback (i32.const 0)))
                (drop (call $public (i32.const 0)))
                (drop (call $mixed (i32.const 1)))
                (drop (call $ignored (call $effect)))
                (drop (i32.and (call $effect) (i32.const 0)))
                (drop (select (call $effect) (i32.const 7) (i32.const 0)))
                (i32.add (global.get $calls)
                    (i32.add (call $mixed (i32.const 20))
                        (call_indirect (type $callback) (i32.const 21) (i32.const 0))))))"#,
        )?;
        let mut module = Module::from_wasm_bytes(&source, &Default::default())?;
        module.expand_all_funcs()?;
        optimize(&mut module, &[]);
        super::super::reachability::retain_reachable(&mut module)?;
        assert!(
            module
                .funcs
                .entries()
                .all(|(_, function)| function.name() != "dead")
        );
        for (_, function) in module.funcs.entries() {
            if let Some(body) = function.body() {
                body.validate()?;
            }
        }
        let engine = wasmtime::Engine::default();
        let compiled = wasmtime::Module::new(&engine, module.to_wasm_bytes()?)?;
        let mut store = wasmtime::Store::new(&engine, ());
        let instance = wasmtime::Instance::new(&mut store, &compiled, &[])?;
        assert_eq!(
            instance
                .get_typed_func::<(), i32>(&mut store, "run")?
                .call(&mut store, ())?,
            45
        );
        assert_eq!(
            instance
                .get_typed_func::<i32, i32>(&mut store, "public")?
                .call(&mut store, 43)?,
            43
        );
        Ok(())
    }
}
