//! Source Promise plans, distinct from the host's one-shot subtask transport.

pub(crate) mod native;

/// The remaining bits carry the present value's tag; payload zero means undefined.
pub(crate) const OPTIONAL_REFERENCE_TAG: u32 = 1 << 8;

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

use std::collections::{BTreeMap, BTreeSet};

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
    HttpBody(super::http::body::BodyMethod),
    WebStream(super::streams::web::Method),
    FetchUpload,
}

impl TaskTarget {
    pub(crate) fn http_body(callee: &Expr) -> Option<Self> {
        if let Expr::PropertyGet { property, .. } = callee {
            super::http::body::BodyMethod::named(property).map(Self::HttpBody)
        } else {
            None
        }
    }
    pub(crate) fn web_stream(callee: &Expr, args: &[Expr]) -> Option<Self> {
        if let Expr::PropertyGet { property, .. } = callee {
            let method = if property == "read" && !args.is_empty() {
                Some(super::streams::web::Method::ReadInto)
            } else {
                super::streams::web::Method::named(property)
            };
            method.map(Self::WebStream)
        } else {
            None
        }
    }
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
    HttpBody,
    WebStream,
    ByobRead,
    Filesystem(super::capabilities::FilesystemOperation),
    TimerValue,
}

impl TaskArguments {
    pub(crate) fn core_types(&self) -> Result<Vec<waffle::Type>> {
        match self {
            Self::TimerValue => Ok(vec![
                waffle::Type::F64,
                waffle::Type::F64,
                waffle::Type::I32,
                waffle::Type::I32,
            ]),
            Self::Fetch => Ok(vec![waffle::Type::I32; 9]),
            Self::FetchUpload => Ok(vec![waffle::Type::I32; 3]),
            Self::ByobRead => Ok(vec![
                waffle::Type::I32,
                waffle::Type::I32,
                waffle::Type::F64,
            ]),
            Self::HttpBody | Self::WebStream => Ok(vec![waffle::Type::I32; 2]),
            Self::Source(types) => types
                .iter()
                .map(super::registry::map_type_to_waffle)
                .collect(),
            Self::Filesystem(operation) => Ok(vec![
                waffle::Type::I32;
                if *operation == super::capabilities::FilesystemOperation::WriteFile {
                    4
                } else {
                    3
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
                            super::capabilities::CapabilityOperation::Clock(
                                super::capabilities::ClockOperation::TimeoutValue,
                            ),
                        ) => TaskArguments::TimerValue,
                        TypedIntrinsic::Capability(
                            super::capabilities::CapabilityOperation::Fetch,
                        ) => TaskArguments::Fetch,
                        TypedIntrinsic::Capability(
                            super::capabilities::CapabilityOperation::Filesystem(operation),
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
    let has_body = has_fetch
        || intrinsics.values().any(|intrinsic| {
            matches!(
                intrinsic,
                TypedIntrinsic::RequestNew | TypedIntrinsic::ResponseNew
            )
        });
    let has_stream = has_body
        || intrinsics.values().any(|i| {
            matches!(
                i,
                TypedIntrinsic::Capability(super::capabilities::CapabilityOperation::Writable(_))
            )
        })
        || hir.functions.iter().any(|f| {
            f.params
                .iter()
                .any(|p| super::streams::web::Kind::of(&p.ty).is_some())
        });
    if has_stream {
        for method in [
            super::streams::web::Method::Read,
            super::streams::web::Method::ReadInto,
            super::streams::web::Method::Cancel,
            super::streams::web::Method::Write,
            super::streams::web::Method::Close,
        ] {
            candidates.insert(
                TaskTarget::WebStream(method),
                TaskPlan {
                    symbol: format!("__perry.stream.{method:?}"),
                    arguments: if method == super::streams::web::Method::ReadInto {
                        TaskArguments::ByobRead
                    } else {
                        TaskArguments::WebStream
                    },
                    result: method.result(),
                },
            );
        }
        for method in super::http::body::BodyMethod::ALL {
            candidates.insert(
                TaskTarget::HttpBody(method),
                TaskPlan {
                    symbol: format!("__perry.body.{method:?}"),
                    arguments: TaskArguments::HttpBody,
                    result: method.result(),
                },
            );
        }
    }
    let mut calls = 0;
    let mut direct_awaits = 0;
    let mut referenced = BTreeSet::new();
    for function in &hir.functions {
        visit_function_expressions(function, &mut |expression| {
            let (expression, awaited) = match expression {
                Expr::Await(inner) => (inner.as_ref(), true),
                expression => (expression, false),
            };
            if let Expr::Call { callee, args, .. } = expression
                && let Some(target) = TaskTarget::from_callee(callee)
                    .or_else(|| has_body.then(|| TaskTarget::http_body(callee)).flatten())
                    .or_else(|| {
                        has_stream
                            .then(|| TaskTarget::web_stream(callee, args))
                            .flatten()
                    })
                && candidates.contains_key(&target)
            {
                if awaited {
                    direct_awaits += 1;
                } else {
                    calls += 1;
                }
                referenced.insert(target);
            }
        });
    }
    let combinators = intrinsics.values().any(|intrinsic| {
        matches!(
            intrinsic,
            TypedIntrinsic::Capability(super::capabilities::CapabilityOperation::Promise(_))
        )
    });
    candidates.retain(|target, _| referenced.contains(target));
    let mut tasks = candidates;
    if has_fetch {
        tasks.insert(
            TaskTarget::FetchUpload,
            TaskPlan {
                symbol: "__perry.fetch.upload".into(),
                arguments: TaskArguments::FetchUpload,
                result: HirType::Number,
            },
        );
    }
    let uses_body = tasks
        .keys()
        .any(|target| matches!(target, TaskTarget::HttpBody(_) | TaskTarget::WebStream(_)));
    let uses_filesystem = tasks
        .values()
        .any(|task| matches!(task.arguments, TaskArguments::Filesystem(_)));
    if calls == direct_awaits && !combinators && !has_fetch && !uses_body && !uses_filesystem {
        return Ok(None);
    }
    for task in tasks.values() {
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
            matches!(task.arguments, TaskArguments::TimerValue) || is_task_outcome(&task.result),
            "Stored async task results require supported scalar, text, byte, object, or list values"
        );
    }
    Ok(Some(PromisePlan { tasks, combinators }))
}

/// Retained outcomes whose value and ownership fit the completion record.
pub(crate) fn is_task_outcome(ty: &HirType) -> bool {
    if super::values::is_string_type(ty) {
        return true;
    }
    matches!(
        ty,
        HirType::Number
            | HirType::Boolean
            | HirType::BigInt
            | HirType::String
            | HirType::Void
            | HirType::Null
    ) || super::bytes::is_byte_storage(ty)
        || super::text_or_bytes::is_text_or_bytes(ty)
        || super::filesystem::is_stats(ty)
        || super::objects::is_object(ty)
        || super::date::is_date(ty)
        || super::time::is_time(ty)
        || super::values::is_dynamic(ty)
        || super::values::is_boxed_union(ty)
        || super::values::sentinel_inner(ty).is_some()
        || super::structured::is_string_array(ty)
        || matches!(ty, HirType::Array(_) | HirType::Tuple(_))
        || super::http::fetch::is_response(ty)
        || super::http::headers::is_headers(ty)
        || super::http::request::is_request(ty)
        || super::streams::web::Kind::of(ty).is_some()
}
