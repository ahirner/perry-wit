//! AST/HIR binding resolution and typed operation identity.
//!
//! Resolves imports, aliases, built-ins, and shadowing before capability
//! classification loses binding identity. Carries typed operation identity into
//! lowering, preserving receiver and argument evaluation order.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail, ensure};
use perry_hir::ir::{Expr, Function, Module as HirModule, Stmt};
use perry_hir::types::{FuncId, Type as HirType};
use waffle::Type as WaffleType;

use super::capabilities::{
    CapabilityImplementation, CapabilityOperation, ClockOperation, ContextOperation,
    LowerCapability, RandomOperation, StdioOperation,
};
use super::visit::visit_function_expressions;

/// The nature of input accepted by the module entry point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvedInputKind {
    Wit,
    Number,
    Boolean,
    String,
    ByteStream,
    Bytes,
    TextOrBytes,
    Stats,
    StringArray,
}

/// Known typed intrinsics with explicit signatures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TypedIntrinsic {
    WitImport {
        name: String,
        key: String,
        params: Vec<WaffleType>,
        is_async: bool,
    },
    Capability(CapabilityOperation),
    HostDouble,
    ReadChunk,
    ReadInto,
    ByteAt,
    DecoderNew,
    DateNew,
    Temporal(super::time::TimeConstructor),
    Custom {
        name: String,
        params: Vec<WaffleType>,
        returns: Vec<WaffleType>,
        is_async: bool,
    },
}

impl TypedIntrinsic {
    pub(crate) fn has_completion(&self) -> bool {
        matches!(
            self,
            Self::WitImport { .. }
                | Self::Capability(
                    CapabilityOperation::Stdio(_)
                        | CapabilityOperation::Filesystem(_)
                        | CapabilityOperation::HttpGet
                        | CapabilityOperation::Random(RandomOperation::Fill)
                )
                | Self::DecoderNew
                | Self::Temporal(_)
        )
    }

    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Capability(operation) => operation.name(),
            Self::HostDouble => "hostDouble",
            Self::ReadChunk => "readChunk",
            Self::ReadInto => "readInto",
            Self::ByteAt => "byteAt",
            Self::DecoderNew => "TextDecoder",
            Self::DateNew => "Date",
            Self::Temporal(operation) => operation.name(),
            Self::Custom { name, .. } | Self::WitImport { name, .. } => name.as_str(),
        }
    }

    pub(crate) fn is_async(&self) -> bool {
        match self {
            Self::Capability(operation) => matches!(operation.lower().result, HirType::Promise(_)),
            Self::HostDouble | Self::ReadChunk | Self::ReadInto => true,
            Self::WitImport { is_async, .. } => *is_async,
            Self::ByteAt | Self::DecoderNew | Self::DateNew | Self::Temporal(_) => false,
            Self::Custom { is_async, .. } => *is_async,
        }
    }

    pub(crate) fn core_signature(&self) -> Result<waffle::SignatureData> {
        let (params, returns) = match self {
            Self::WitImport { params, .. } => {
                (params.clone(), vec![WaffleType::I32, WaffleType::F64])
            }
            Self::Capability(operation) => {
                let plan = operation.lower();
                let result = match &plan.result {
                    HirType::Promise(inner) => inner.as_ref(),
                    result => result,
                };
                let returns = if self.has_completion() {
                    vec![WaffleType::I32, WaffleType::F64]
                } else if matches!(result, HirType::Void) {
                    vec![]
                } else {
                    vec![map_hir_type_to_waffle(result)?]
                };
                (
                    plan.params
                        .iter()
                        .map(map_hir_type_to_waffle)
                        .collect::<Result<_>>()?,
                    returns,
                )
            }
            Self::HostDouble | Self::ByteAt => (vec![WaffleType::F64], vec![WaffleType::F64]),
            Self::ReadChunk => (vec![WaffleType::I32], vec![WaffleType::F64]),
            Self::ReadInto => (vec![WaffleType::I32; 2], vec![WaffleType::F64]),
            Self::DateNew => (vec![WaffleType::F64], vec![WaffleType::I32]),
            Self::Temporal(operation) => (
                vec![if operation.argument_type() == HirType::Number {
                    WaffleType::F64
                } else {
                    WaffleType::I32
                }],
                vec![WaffleType::I32, WaffleType::F64],
            ),
            Self::DecoderNew => (
                vec![WaffleType::I32; 3],
                vec![WaffleType::I32, WaffleType::F64],
            ),
            Self::Custom {
                params, returns, ..
            } => (params.clone(), returns.clone()),
        };
        Ok(waffle::SignatureData { params, returns })
    }
}

/// Validated contract containing typed operations and module signatures.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedContract {
    pub(crate) http_handler: Option<super::HttpHandlerOptions>,
    pub(crate) wit: Option<super::wit::WitWorld>,
    pub(crate) literal_shapes: BTreeMap<String, Vec<String>>,
    pub(crate) promises: Option<super::promises::PromisePlan>,
    pub(crate) input_kind: ResolvedInputKind,
    pub(crate) uses_p3_clocks: bool,
    pub(crate) intrinsics: BTreeMap<String, TypedIntrinsic>,
    pub(crate) functions_by_name: BTreeMap<String, FuncId>,
    pub(crate) entry_func_id: FuncId,
    pub(crate) entry_params: Vec<HirType>,
    pub(crate) entry_return_type: HirType,
}

impl ResolvedContract {
    pub(crate) fn has_http(&self) -> bool {
        self.intrinsics.values().any(|intrinsic| {
            matches!(
                intrinsic,
                TypedIntrinsic::Capability(CapabilityOperation::HttpGet)
            )
        })
    }
    pub(crate) fn context_operations(&self) -> BTreeSet<ContextOperation> {
        self.intrinsics
            .values()
            .filter_map(|intrinsic| match intrinsic {
                TypedIntrinsic::Capability(CapabilityOperation::Context(operation)) => {
                    Some(*operation)
                }
                _ => None,
            })
            .collect()
    }
    pub(crate) fn clock_operations(&self) -> BTreeSet<ClockOperation> {
        self.intrinsics
            .values()
            .filter_map(|intrinsic| match intrinsic {
                TypedIntrinsic::Capability(CapabilityOperation::Clock(operation)) => {
                    Some(*operation)
                }
                _ => None,
            })
            .collect()
    }
    pub(crate) fn random_operations(&self) -> BTreeSet<RandomOperation> {
        self.intrinsics
            .values()
            .filter_map(|intrinsic| match intrinsic {
                TypedIntrinsic::Capability(CapabilityOperation::Random(operation)) => {
                    Some(*operation)
                }
                _ => None,
            })
            .collect()
    }

    pub(crate) fn has_filesystem(&self) -> bool {
        self.intrinsics.values().any(|intrinsic| {
            matches!(
                intrinsic,
                TypedIntrinsic::Capability(CapabilityOperation::Filesystem(_))
            )
        })
    }

    pub(crate) fn output_operations(&self) -> BTreeSet<StdioOperation> {
        self.intrinsics
            .values()
            .filter_map(|intrinsic| {
                if let TypedIntrinsic::Capability(operation) = intrinsic
                    && let CapabilityImplementation::Stdio(operation) =
                        operation.lower().implementation
                {
                    Some(operation)
                } else {
                    None
                }
            })
            .collect()
    }

    pub(crate) fn has_stream_input(&self) -> bool {
        self.entry_params
            .iter()
            .any(|ty| matches!(ty, HirType::Named(name) if name == "ByteStream"))
    }
    /// The canonical result type after unwrapping asynchronous transport.
    pub(crate) fn entry_result_type(&self) -> &HirType {
        let mut ty = &self.entry_return_type;
        while let HirType::Promise(inner) = ty {
            ty = inner;
        }
        ty
    }

    pub(crate) fn entry_returns_wit_result(&self) -> bool {
        matches!(self.entry_result_type(), HirType::Generic { base, .. } if base == "Result")
    }
}

/// Resolves module bindings, shadowing, and contracts for WAFFLE lowering.
pub(crate) fn resolve_contract(
    hir: &HirModule,
    bindings: &super::source::SourceBindings,
    wit: Option<super::wit::WitWorld>,
) -> Result<ResolvedContract> {
    ensure!(
        hir.init.is_empty(),
        "Module initialization is unsupported by the WAFFLE backend"
    );
    ensure!(
        !hir.functions.is_empty(),
        "Module must declare at least one function"
    );
    ensure!(
        hir.classes.iter().all(|class| class.is_literal_shape()),
        "Unsupported class initialization in the WAFFLE backend"
    );

    let mut functions_by_name = BTreeMap::new();
    for func in &hir.functions {
        ensure!(
            func.params
                .iter()
                .all(|param| param.default.is_none() && !param.is_rest),
            "Unsupported default or rest parameters in the WAFFLE backend"
        );
        ensure!(
            functions_by_name
                .insert(func.name.clone(), func.id)
                .is_none(),
            "Duplicate function definition: {}",
            func.name
        );
    }

    // Resolve extern functions into typed intrinsics
    let mut intrinsics = BTreeMap::new();

    for (name, params, ret) in &hir.extern_funcs {
        if let Some(key) = bindings.wit_imports.get(name) {
            intrinsics.insert(
                name.clone(),
                TypedIntrinsic::WitImport {
                    name: name.clone(),
                    key: key.clone(),
                    is_async: matches!(ret, HirType::Promise(_)),
                    params: params
                        .iter()
                        .map(super::registry::map_type_to_waffle)
                        .collect::<Result<_>>()?,
                },
            );
            continue;
        }
        if let Some(operation) = bindings.time_constructors.get(name) {
            intrinsics.insert(name.clone(), TypedIntrinsic::Temporal(*operation));
            continue;
        }
        if bindings.date_constructor.as_ref() == Some(name) {
            intrinsics.insert(name.clone(), TypedIntrinsic::DateNew);
            continue;
        }
        if bindings.decoder_constructor.as_ref() == Some(name) {
            intrinsics.insert(name.clone(), TypedIntrinsic::DecoderNew);
            continue;
        }
        if let Some(operation) = bindings
            .capabilities
            .get(name)
            .copied()
            .or_else(|| CapabilityOperation::from_declaration(name))
        {
            let plan = operation.lower();
            ensure!(
                *params == plan.params && *ret == plan.result,
                "Invalid capability declaration '{name}': expected {:?} -> {:?}",
                plan.params,
                plan.result
            );
            intrinsics.insert(name.clone(), TypedIntrinsic::Capability(operation));
            continue;
        }
        match name.as_str() {
            "hostDouble" => {
                ensure!(
                    params.len() == 1 && matches!(params[0], HirType::Number),
                    "hostDouble signature must be (value: number) => Promise<number>"
                );
                ensure!(
                    matches!(ret, HirType::Promise(inner) if matches!(**inner, HirType::Number)),
                    "hostDouble must return Promise<number>"
                );
                intrinsics.insert(name.clone(), TypedIntrinsic::HostDouble);
            }
            "readChunk" => {
                ensure!(
                    params.len() == 1
                        && matches!(&params[0], HirType::Named(n) if n == "ByteStream"),
                    "readChunk signature must be (stream: ByteStream) => Promise<number>"
                );
                ensure!(
                    matches!(ret, HirType::Promise(inner) if matches!(**inner, HirType::Number)),
                    "readChunk must return Promise<number>"
                );
                intrinsics.insert(name.clone(), TypedIntrinsic::ReadChunk);
            }
            "readInto" => {
                ensure!(
                    params.len() == 2
                        && matches!(&params[0], HirType::Named(n) if n == "ByteStream")
                        && super::bytes::is_byte_view(&params[1]),
                    "readInto signature must be (stream: ByteStream, destination: Uint8Array) => Promise<number>"
                );
                ensure!(
                    matches!(ret, HirType::Promise(inner) if matches!(**inner, HirType::Number)),
                    "readInto must return Promise<number>"
                );
                intrinsics.insert(name.clone(), TypedIntrinsic::ReadInto);
            }
            "byteAt" => {
                ensure!(
                    params.len() == 1 && matches!(params[0], HirType::Number),
                    "byteAt signature must be (index: number) => number"
                );
                ensure!(matches!(ret, HirType::Number), "byteAt must return number");
                intrinsics.insert(name.clone(), TypedIntrinsic::ByteAt);
            }
            other => {
                // Generic custom typed intrinsic
                let waffle_params = params
                    .iter()
                    .map(map_hir_type_to_waffle)
                    .collect::<Result<Vec<_>>>()?;
                let (waffle_returns, is_async) = match ret {
                    HirType::Void => (vec![], false),
                    HirType::Promise(inner) => {
                        let ret_ty = if matches!(**inner, HirType::Void) {
                            vec![]
                        } else {
                            vec![map_hir_type_to_waffle(inner)?]
                        };
                        (ret_ty, true)
                    }
                    other_ty => (vec![map_hir_type_to_waffle(other_ty)?], false),
                };
                intrinsics.insert(
                    other.to_string(),
                    TypedIntrinsic::Custom {
                        name: other.to_string(),
                        params: waffle_params,
                        returns: waffle_returns,
                        is_async,
                    },
                );
            }
        }
    }

    // Determine primary exported entry point (defaults to first function if not explicitly marked)
    let entry_func = hir
        .functions
        .iter()
        .find(|f| f.is_exported)
        .unwrap_or(&hir.functions[0]);

    let input_kind = if wit.is_some() {
        ResolvedInputKind::Wit
    } else if let Some(first_param) = entry_func.params.first() {
        match &first_param.ty {
            HirType::Named(name) if name == "ByteStream" => ResolvedInputKind::ByteStream,
            ty if super::values::is_dynamic(ty) => ResolvedInputKind::Number,
            HirType::Number | HirType::Any => ResolvedInputKind::Number,
            HirType::Boolean => ResolvedInputKind::Boolean,
            HirType::String => ResolvedInputKind::String,
            ty if super::filesystem::is_stats(ty) => ResolvedInputKind::Stats,
            ty if super::structured::is_string_array(ty) => ResolvedInputKind::StringArray,
            ty if super::bytes::is_byte_view(ty) => ResolvedInputKind::Bytes,
            ty if super::text_or_bytes::is_text_or_bytes(ty) => ResolvedInputKind::TextOrBytes,
            other => bail!("Unsupported entry function parameter type: {other:?}"),
        }
    } else {
        ResolvedInputKind::Number
    };

    // Audit function bodies for shadowing of intrinsics by local variable declarations
    for func in &hir.functions {
        audit_scope_and_shadowing(func, &intrinsics)?;
    }

    let mut referenced = BTreeSet::new();
    for function in &hir.functions {
        visit_function_expressions(function, &mut |expression| {
            if let Expr::ExternFuncRef { name, .. } = expression {
                referenced.insert(name.clone());
            }
        });
    }
    intrinsics.retain(|name, _| referenced.contains(name));
    let uses_p3_clocks = intrinsics.values().any(|intrinsic| {
        matches!(
            intrinsic,
            TypedIntrinsic::Capability(CapabilityOperation::Clock(_))
        )
    });

    let promises = super::promises::plan_promises(hir, &intrinsics)?;
    let stream_inputs = entry_func
        .params
        .iter()
        .filter(|param| matches!(&param.ty, HirType::Named(name) if name == "ByteStream"))
        .count();
    ensure!(
        stream_inputs <= 1,
        "Only one ByteStream input is supported per entry invocation"
    );
    ensure!(
        stream_inputs == 0 || promises.is_none(),
        "ByteStream inputs cannot be combined with stored async tasks until each transfer has its own buffer owner"
    );
    ensure!(
        stream_inputs == 1
            || !intrinsics.values().any(|intrinsic| matches!(
                intrinsic,
                TypedIntrinsic::ReadChunk | TypedIntrinsic::ReadInto | TypedIntrinsic::ByteAt
            )),
        "Stream operations require a ByteStream entry input"
    );
    Ok(ResolvedContract {
        http_handler: None,
        wit,
        literal_shapes: hir
            .classes
            .iter()
            .filter(|class| class.name.starts_with("__AnonShape_"))
            .map(|class| {
                (
                    class.name.clone(),
                    class
                        .fields
                        .iter()
                        .map(|field| field.name.clone())
                        .collect(),
                )
            })
            .collect(),
        promises,
        input_kind,
        uses_p3_clocks,
        intrinsics,
        functions_by_name,
        entry_func_id: entry_func.id,
        entry_params: entry_func
            .params
            .iter()
            .map(|param| param.ty.clone())
            .collect(),
        entry_return_type: entry_func.return_type.clone(),
    })
}

/// Audit function AST/HIR to verify that local variables do not shadow intrinsics in an illegal manner.
fn audit_scope_and_shadowing(
    func: &Function,
    intrinsics: &BTreeMap<String, TypedIntrinsic>,
) -> Result<()> {
    for param in &func.params {
        if intrinsics.contains_key(&param.name) {
            bail!(
                "Parameter '{}' in function '{}' shadows declared intrinsic of the same name",
                param.name,
                func.name
            );
        }
    }

    check_stmts_shadowing(&func.body, intrinsics)?;
    Ok(())
}

fn check_stmts_shadowing(
    stmts: &[Stmt],
    intrinsics: &BTreeMap<String, TypedIntrinsic>,
) -> Result<()> {
    for stmt in stmts {
        match stmt {
            Stmt::Let { name, .. } => {
                if intrinsics.contains_key(name) {
                    bail!("Local variable '{name}' illegally shadows declared intrinsic");
                }
            }
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                check_stmts_shadowing(then_branch, intrinsics)?;
                if let Some(else_b) = else_branch {
                    check_stmts_shadowing(else_b, intrinsics)?;
                }
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
                check_stmts_shadowing(body, intrinsics)?;
            }
            Stmt::For { init, body, .. } => {
                if let Some(init_stmt) = init
                    && let Stmt::Let { name, .. } = init_stmt.as_ref()
                    && intrinsics.contains_key(name)
                {
                    bail!("For loop local '{name}' illegally shadows declared intrinsic");
                }
                check_stmts_shadowing(body, intrinsics)?;
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                check_stmts_shadowing(body, intrinsics)?;
                if let Some(c) = catch {
                    if let Some((_, param_name)) = &c.param
                        && intrinsics.contains_key(param_name)
                    {
                        bail!(
                            "Catch parameter '{param_name}' illegally shadows declared intrinsic"
                        );
                    }
                    check_stmts_shadowing(&c.body, intrinsics)?;
                }
                if let Some(f) = finally {
                    check_stmts_shadowing(f, intrinsics)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn map_hir_type_to_waffle(ty: &HirType) -> Result<WaffleType> {
    match ty {
        HirType::Number => Ok(WaffleType::F64),
        HirType::Boolean => Ok(WaffleType::I32),
        HirType::Named(name) if name == "ByteStream" => Ok(WaffleType::I32),
        _ => bail!("Type {ty:?} cannot be directly mapped to a primitive WAFFLE type"),
    }
}
