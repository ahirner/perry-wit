//! Source Promise plans, distinct from the host's one-shot subtask transport.

pub(crate) mod native;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Combinator {
    All,
    AllSettled,
    Race,
}

impl Combinator {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "all" => Some(Self::All),
            "allSettled" => Some(Self::AllSettled),
            "race" => Some(Self::Race),
            _ => None,
        }
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::AllSettled => "allSettled",
            Self::Race => "race",
        }
    }
}

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
    FetchBody(super::http::fetch::BodyMethod),
    FetchUpload,
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
    pub(crate) arguments: TaskArguments,
    pub(crate) result: HirType,
}

#[derive(Clone, Debug)]
pub(crate) enum TaskArguments {
    Source(Vec<HirType>),
    Fetch,
    FetchUpload,
    Filesystem(super::capabilities::FilesystemOperation),
}

impl TaskArguments {
    pub(crate) fn core_types(&self) -> Result<Vec<waffle::Type>> {
        match self {
            Self::Fetch => Ok(vec![waffle::Type::I32; 7]),
            Self::FetchUpload => Ok(vec![waffle::Type::I32; 2]),
            Self::Source(types) => types
                .iter()
                .map(super::registry::map_type_to_waffle)
                .collect(),
            Self::Filesystem(operation) => Ok(vec![
                waffle::Type::I32;
                if *operation == super::capabilities::FilesystemOperation::WriteFile {
                    3
                } else {
                    2
                }
            ]),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PromisePlan {
    pub(crate) combinators: bool,
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
                    arguments: TaskArguments::Source(
                        function
                            .params
                            .iter()
                            .map(|param| param.ty.clone())
                            .collect(),
                    ),
                    result,
                },
            );
        }
    }
    for (index, (name, intrinsic)) in intrinsics.iter().enumerate() {
        if matches!(
            intrinsic,
            TypedIntrinsic::Capability(super::capabilities::CapabilityOperation::Promise(_))
        ) {
            continue;
        }
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
                    arguments: match intrinsic {
                        TypedIntrinsic::Capability(
                            super::capabilities::CapabilityOperation::Fetch,
                        ) => TaskArguments::Fetch,
                        TypedIntrinsic::Capability(
                            super::capabilities::CapabilityOperation::FilesystemPromise(operation),
                        ) => TaskArguments::Filesystem(*operation),
                        _ => TaskArguments::Source(params.clone()),
                    },
                    result: result.as_ref().clone(),
                },
            );
        }
    }
    let has_fetch = intrinsics.values().any(|intrinsic| {
        matches!(
            intrinsic,
            TypedIntrinsic::Capability(super::capabilities::CapabilityOperation::Fetch)
        )
    });
    if has_fetch {
        for method in super::http::fetch::BodyMethod::ALL {
            candidates.insert(
                TaskTarget::FetchBody(method),
                TaskPlan {
                    symbol: format!("__perry.fetch.{method:?}"),
                    arguments: TaskArguments::Source(vec![HirType::Named(
                        super::http::fetch::RESPONSE_TYPE.into(),
                    )]),
                    result: method.result(),
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
                && let Some(target) = TaskTarget::from_callee(callee).or_else(|| {
                    if has_fetch && let Expr::PropertyGet { property, .. } = callee.as_ref() {
                        super::http::fetch::BodyMethod::named(property).map(TaskTarget::FetchBody)
                    } else {
                        None
                    }
                })
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
    let combinators = intrinsics.values().any(|intrinsic| {
        matches!(
            intrinsic,
            TypedIntrinsic::Capability(super::capabilities::CapabilityOperation::Promise(_))
        )
    });
    if has_fetch {
        referenced.insert(
            TaskTarget::FetchUpload,
            TaskPlan {
                symbol: "__perry.fetch.upload".into(),
                arguments: TaskArguments::FetchUpload,
                result: HirType::Number,
            },
        );
    }
    if calls == direct_awaits && !combinators && !has_fetch {
        return Ok(None);
    }
    for task in referenced.values() {
        ensure!(
            task.arguments.core_types()?.len() < 16,
            "Stored async calls support at most 15 arguments"
        );
        if let TaskArguments::Source(params) = &task.arguments {
            ensure!(
                params
                    .iter()
                    .all(|param| (is_task_outcome(param) && param != &HirType::Void)
                        || matches!(param, HirType::Promise(inner) if is_task_outcome(inner))),
                "Stored async task parameters require supported values or Promises of supported outcomes"
            );
        }
        ensure!(
            is_task_outcome(&task.result),
            "Stored async task results require supported scalar, text, byte, object, or list values"
        );
    }
    Ok(Some(PromisePlan {
        tasks: referenced,
        combinators,
    }))
}

/// Retained outcomes whose value and ownership fit the completion record.
pub(crate) fn is_task_outcome(ty: &HirType) -> bool {
    if super::values::is_string_type(ty) {
        return true;
    }
    matches!(
        ty,
        HirType::Number | HirType::Boolean | HirType::BigInt | HirType::String | HirType::Void
    ) || super::bytes::is_byte_storage(ty)
        || super::text_or_bytes::is_text_or_bytes(ty)
        || super::filesystem::is_stats(ty)
        || super::objects::is_object(ty)
        || super::date::is_date(ty)
        || super::time::is_time(ty)
        || super::values::is_dynamic(ty)
        || super::values::is_boxed_union(ty)
        || super::structured::is_string_array(ty)
        || matches!(ty, HirType::Array(_) | HirType::Tuple(_))
        || super::http::is_response(ty)
        || super::http::headers::is_headers(ty)
}
