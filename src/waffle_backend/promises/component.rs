//! Native task adapters sharing the generated guest's memory and allocator.

use std::fmt::Write;

use anyhow::{Result, bail};
use perry_hir::types::Type as HirType;
use waffle::Type;

use super::{PromisePlan, TaskTarget};
use crate::waffle_backend::{
    component::{component_value_type, entry_signature},
    registry::{canonical_param_types, map_return_type_to_waffle, map_type_to_waffle},
    resolve::ResolvedContract,
};

struct Forward {
    name: String,
    params: Vec<&'static str>,
    results: Vec<&'static str>,
    target: String,
}

fn core_type(ty: Type) -> &'static str {
    match ty {
        Type::I32 => "i32",
        Type::F64 => "f64",
        _ => unreachable!("unsupported source primitive"),
    }
}

pub(crate) fn frame(
    core_body: &str,
    host_imports: &str,
    host_wires: &str,
    contract: &ResolvedContract,
    plan: &PromisePlan,
) -> Result<String> {
    let mut forwards = [
        ("new", vec![], vec!["i32"]),
        ("bind", vec!["i32", "i32"], vec![]),
        ("await", vec!["i32"], vec!["i32", "f64"]),
        ("yield", vec![], vec![]),
    ]
    .into_iter()
    .map(|(name, params, results)| Forward {
        name: name.into(),
        params,
        results,
        target: format!("(func $runtime {name:?})"),
    })
    .collect::<Vec<_>>();
    for task in plan.tasks.values() {
        let mut params = vec!["i32"];
        params.extend(
            task.params
                .iter()
                .map(map_type_to_waffle)
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .map(core_type),
        );
        forwards.push(Forward {
            name: task.symbol.clone(),
            params,
            results: vec!["i32"],
            target: format!("(func $start-{})", task.symbol),
        });
    }
    let mut wat = format!("(component\n{host_imports}\n(core instance $host {host_wires})\n");
    writeln!(
        wat,
        "(core module $forward (table (export \"table\") {} funcref)",
        forwards.len()
    )?;
    for (index, forward) in forwards.iter().enumerate() {
        let params = forward.params.join(" ");
        let results = forward.results.join(" ");
        writeln!(
            wat,
            "(type $f{index} (func (param {params}) (result {results})))"
        )?;
        writeln!(
            wat,
            "(func (export {:?}) (param {params}) (result {results})",
            forward.name
        )?;
        for param in 0..forward.params.len() {
            writeln!(wat, "local.get {param}")?;
        }
        writeln!(wat, "i32.const {index} call_indirect (type $f{index}))")?;
    }
    writeln!(wat, ") (core instance $forward (instantiate $forward))")?;
    writeln!(wat, "(core module $guest {core_body})")?;
    writeln!(
        wat,
        "(core instance $guest (instantiate $guest (with \"host\" (instance $host)) (with \"promises\" (instance $forward))))"
    )?;
    wat.push_str(
        r#"
      (core func $new-set (canon waitable-set.new))
      (core func $join (canon waitable.join))
      (core func $wait (canon waitable-set.wait (memory (core memory $guest "memory"))))
      (core func $drop-task (canon subtask.drop))
      (core func $drop-set (canon waitable-set.drop))
      (core func $yield (canon thread.yield))
      (core func $return-task (canon task.return))
    "#,
    );
    wat.push_str(include_str!("runtime.wat"));
    wat.push_str(
        r#"
      (core instance $runtime (instantiate $promise-runtime
        (with "guest" (instance $guest))
        (with "native" (instance
          (export "new-set" (func $new-set)) (export "join" (func $join))
          (export "wait" (func $wait)) (export "drop-task" (func $drop-task))
          (export "drop-set" (func $drop-set)) (export "yield" (func $yield))))))
      (core module $tasks
        (import "runtime" "settle" (func $settle (param i32 i32 f64)))
        (import "native" "return" (func $return))
    "#,
    );
    for (target, task) in &plan.tasks {
        let params = task
            .params
            .iter()
            .map(map_type_to_waffle)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(core_type)
            .collect::<Vec<_>>()
            .join(" ");
        let (module, name, results) = match target {
            TaskTarget::Guest(_) => ("guest", &task.symbol, "i32 f64"),
            TaskTarget::Intrinsic(name) => (
                "host",
                name,
                if task.result == HirType::Void {
                    ""
                } else {
                    "f64"
                },
            ),
        };
        writeln!(
            wat,
            "(import {module:?} {name:?} (func $source-{} (param {params}) (result {results})))",
            task.symbol
        )?;
    }
    for (target, task) in &plan.tasks {
        writeln!(wat, "(func (export {:?}) (param $owner i32)", task.symbol)?;
        for (index, ty) in task.params.iter().enumerate() {
            writeln!(
                wat,
                "(param $p{index} {})",
                core_type(map_type_to_waffle(ty)?)
            )?;
        }
        wat.push_str("(local $tag i32) (local $payload f64)\n");
        write!(wat, "(call $source-{}", task.symbol)?;
        for index in 0..task.params.len() {
            write!(wat, " (local.get $p{index})")?;
        }
        wat.push_str(")\n");
        if matches!(target, TaskTarget::Guest(_)) {
            wat.push_str("local.set $payload local.set $tag\n");
        } else if task.result != HirType::Void {
            wat.push_str("local.set $payload\n");
        }
        wat.push_str("(call $settle (local.get $owner) (local.get $tag) (local.get $payload)) (call $return))\n");
    }
    wat.push_str(r#") (core instance $tasks (instantiate $tasks
      (with "guest" (instance $guest)) (with "host" (instance $host))
      (with "runtime" (instance $runtime)) (with "native" (instance (export "return" (func $return-task))))))
    "#);
    for task in plan.tasks.values() {
        write!(wat, "(func ${} async (param \"owner\" u32)", task.symbol)?;
        for (index, ty) in task.params.iter().enumerate() {
            let ty = match map_type_to_waffle(ty)? {
                Type::F64 => "f64",
                _ => "u32",
            };
            write!(wat, " (param \"arg-{index}\" {ty})")?;
        }
        writeln!(
            wat,
            " (canon lift (core func $tasks {:?}) async))",
            task.symbol
        )?;
        writeln!(
            wat,
            "(core func $start-{} (canon lower (func ${}) async))",
            task.symbol, task.symbol
        )?;
    }
    wat.push_str("(core module $wire (import \"forward\" \"table\" (table 0 funcref))\n");
    for (index, forward) in forwards.iter().enumerate() {
        writeln!(
            wat,
            "(import \"targets\" {:?} (func $f{index} (param {}) (result {})))",
            forward.name,
            forward.params.join(" "),
            forward.results.join(" ")
        )?;
    }
    wat.push_str("(elem (i32.const 0) func");
    for index in 0..forwards.len() {
        write!(wat, " $f{index}")?;
    }
    wat.push_str("))\n(core instance $wire (instantiate $wire (with \"forward\" (instance $forward)) (with \"targets\" (instance\n");
    for forward in &forwards {
        writeln!(wat, "(export {:?} {})", forward.name, forward.target)?;
    }
    wat.push_str("))))\n");
    wat.push_str(&entry_adapter(contract)?);
    wat.push(')');
    Ok(wat)
}

/// `task.return` lowers a value as parameters, unlike the synchronous result ABI.
fn entry_adapter(contract: &ResolvedContract) -> Result<String> {
    let return_type = contract.entry_result_type();
    let core_results = map_return_type_to_waffle(return_type)?
        .into_iter()
        .map(core_type)
        .collect::<Vec<_>>()
        .join(" ");
    let core_params = canonical_param_types(&contract.entry_params)?
        .into_iter()
        .map(core_type)
        .collect::<Vec<_>>();
    let mut return_type_text = String::new();
    if return_type != &HirType::Void {
        write!(
            return_type_text,
            "(result {})",
            component_value_type(return_type)?
        )?;
    }
    let (return_params, return_args) = match return_type {
        HirType::Void => ("", String::new()),
        HirType::Number | HirType::Any => ("f64", "(local.get $result)".into()),
        HirType::Boolean => ("i32", "(local.get $result)".into()),
        HirType::String => (
            "i32 i32",
            "(i32.load (local.get $result)) (i32.load offset=4 (local.get $result))".into(),
        ),
        HirType::Generic { base, type_args } if base == "Result" => {
            let tag = "(i32.load8_u (local.get $result))";
            match type_args[0] {
                HirType::Number | HirType::Any => (
                    "i32 f64",
                    format!("{tag} (f64.load offset=8 (local.get $result))"),
                ),
                HirType::Boolean => (
                    "i32 i64",
                    format!(
                        "{tag} (if (result i64) {tag} (then (i64.load offset=8 (local.get $result))) (else (i64.extend_i32_u (i32.load8_u offset=8 (local.get $result)))))"
                    ),
                ),
                HirType::String => (
                    "i32 i64 i32",
                    format!(
                        "{tag} (if (result i64) {tag} (then (i64.load offset=8 (local.get $result))) (else (i64.extend_i32_u (i32.load offset=8 (local.get $result))))) (if (result i32) {tag} (then (i32.const 0)) (else (i32.load offset=12 (local.get $result))))"
                    ),
                ),
                _ => bail!("Unsupported Promise entry result: {return_type:?}"),
            }
        }
        _ => bail!("Unsupported Promise entry result: {return_type:?}"),
    };
    let params = core_params.join(" ");
    let mut wat = format!(
        r#"
      (core func $return-entry (canon task.return {return_type_text} (memory (core memory $guest "memory"))))
      (core module $entry
        (import "guest" "run" (func $run (param {params}) (result {core_results})))
        (import "guest" "cabi_post_run" (func $cleanup (param {core_results})))
        (import "guest" "memory" (memory 1))
        (import "runtime" "finish" (func $finish))
        (import "runtime" "enter" (func $enter))
        (import "runtime" "leave" (func $leave))
        (import "native" "return" (func $return (param {return_params})))
        (func (export "run") (param {params})
    "#
    );
    if !core_results.is_empty() {
        writeln!(wat, "(local $result {core_results})")?;
    }
    wat.push_str("call $enter\n");
    for index in 0..core_params.len() {
        writeln!(wat, "local.get {index}")?;
    }
    wat.push_str("call $run\n");
    if !core_results.is_empty() {
        wat.push_str("local.set $result\n");
    }
    writeln!(wat, "(call $finish) (call $return {return_args})")?;
    if !core_results.is_empty() {
        wat.push_str("local.get $result\n");
    }
    wat.push_str(r#"call $cleanup call $leave))
      (core instance $entry (instantiate $entry (with "guest" (instance $guest)) (with "runtime" (instance $runtime))
        (with "native" (instance (export "return" (func $return-entry))))))
    "#);
    writeln!(
        wat,
        "(func (export \"run\") async {} (canon lift (core func $entry \"run\") async (memory (core memory $guest \"memory\")) (realloc (core func $guest \"cabi_realloc\"))))",
        entry_signature(contract)?
    )?;
    Ok(wat)
}
