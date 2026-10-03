//! Source Promise plans, distinct from the host's one-shot subtask transport.

pub(crate) mod component;

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use perry_hir::{
    ir::{Expr, Module},
    types::{FuncId, Type as HirType},
};

use super::resolve::TypedIntrinsic;
use super::visit::visit_function_expressions;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TaskTarget {
    Guest(FuncId),
    Intrinsic(String),
}

impl TaskTarget {
    pub(crate) fn from_callee(callee: &Expr) -> Option<Self> {
        match callee {
            Expr::FuncRef(id) => Some(Self::Guest(*id)),
            Expr::ExternFuncRef { name, .. } => Some(Self::Intrinsic(name.clone())),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TaskPlan {
    pub(crate) symbol: String,
    pub(crate) params: Vec<HirType>,
    pub(crate) result: HirType,
}

#[derive(Clone, Debug)]
pub(crate) struct PromisePlan {
    pub(crate) tasks: BTreeMap<TaskTarget, TaskPlan>,
}

/// Direct await keeps the existing suspension ABI until an operation escapes it.
pub(crate) fn plan_promises(
    hir: &Module,
    intrinsics: &BTreeMap<String, TypedIntrinsic>,
) -> Result<Option<PromisePlan>> {
    let mut candidates = BTreeMap::new();
    for function in &hir.functions {
        if function.is_async {
            let result = match &function.return_type {
                HirType::Promise(result) => result.as_ref().clone(),
                result => result.clone(),
            };
            candidates.insert(
                TaskTarget::Guest(function.id),
                TaskPlan {
                    symbol: format!("__perry.task.{}", function.id),
                    params: function
                        .params
                        .iter()
                        .map(|param| param.ty.clone())
                        .collect(),
                    result,
                },
            );
        }
    }
    for (index, (name, intrinsic)) in intrinsics.iter().enumerate() {
        if intrinsic.is_async() {
            let (_, params, result) = hir
                .extern_funcs
                .iter()
                .find(|(binding, _, _)| binding == name)
                .unwrap();
            let HirType::Promise(result) = result else {
                unreachable!()
            };
            candidates.insert(
                TaskTarget::Intrinsic(name.clone()),
                TaskPlan {
                    symbol: format!("__perry.import.{index}"),
                    params: params.clone(),
                    result: result.as_ref().clone(),
                },
            );
        }
    }
    let mut calls = 0;
    let mut direct_awaits = 0;
    let mut referenced = BTreeMap::new();
    for function in &hir.functions {
        visit_function_expressions(function, &mut |expression| {
            let (expression, awaited) = match expression {
                Expr::Await(inner) => (inner.as_ref(), true),
                expression => (expression, false),
            };
            if let Expr::Call { callee, .. } = expression
                && let Some(target) = TaskTarget::from_callee(callee)
                && let Some(plan) = candidates.get(&target)
            {
                if awaited {
                    direct_awaits += 1;
                } else {
                    calls += 1;
                }
                referenced.insert(target, plan.clone());
            }
        });
    }
    if calls == direct_awaits {
        return Ok(None);
    }
    for task in referenced.values() {
        ensure!(
            task.params.len() < 16,
            "Stored async calls support at most 15 primitive arguments"
        );
        ensure!(
            task.params
                .iter()
                .all(|param| matches!(param, HirType::Number | HirType::Boolean | HirType::String)
                    || matches!(param, HirType::Promise(inner) if matches!(inner.as_ref(), HirType::Number | HirType::Boolean | HirType::String | HirType::Void))),
            "Stored async task parameters require primitives or Promises of primitive outcomes"
        );
        ensure!(
            matches!(
                task.result,
                HirType::Number | HirType::Boolean | HirType::String | HirType::Void
            ),
            "Stored async task results currently support numbers, booleans, strings, and void"
        );
    }
    Ok(Some(PromisePlan { tasks: referenced }))
}
