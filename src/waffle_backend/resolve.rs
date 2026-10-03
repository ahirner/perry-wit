//! AST/HIR binding resolution and typed operation identity.
//!
//! Resolves imports, aliases, built-ins, and shadowing before capability
//! classification loses binding identity. Carries typed operation identity into
//! lowering, preserving receiver and argument evaluation order.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail, ensure};
use perry_hir::ir::{Function, Module as HirModule, Stmt};
use perry_hir::types::{FuncId, Type as HirType};
use waffle::Type as WaffleType;

/// The nature of input accepted by the module entry point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvedInputKind {
    Number,
    ByteStream,
}

/// Known typed intrinsics with explicit signatures.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum TypedIntrinsic {
    WaitFor,
    HostDouble,
    ReadChunk,
    ByteAt,
    StreamDrop,
    StreamReset,
    Custom {
        name: String,
        params: Vec<WaffleType>,
        returns: Vec<WaffleType>,
        is_async: bool,
    },
}

#[allow(dead_code)]
impl TypedIntrinsic {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::WaitFor => "waitFor",
            Self::HostDouble => "hostDouble",
            Self::ReadChunk => "readChunk",
            Self::ByteAt => "byteAt",
            Self::StreamDrop => "drop",
            Self::StreamReset => "reset",
            Self::Custom { name, .. } => name.as_str(),
        }
    }

    pub(crate) fn is_async(&self) -> bool {
        match self {
            Self::WaitFor | Self::HostDouble | Self::ReadChunk => true,
            Self::ByteAt | Self::StreamDrop | Self::StreamReset => false,
            Self::Custom { is_async, .. } => *is_async,
        }
    }
}

/// Validated contract containing typed operations and module signatures.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedContract {
    pub(crate) input_kind: ResolvedInputKind,
    pub(crate) uses_p3_clocks: bool,
    pub(crate) intrinsics: BTreeMap<String, TypedIntrinsic>,
    #[allow(dead_code)]
    pub(crate) functions_by_name: BTreeMap<String, FuncId>,
    pub(crate) entry_func_id: FuncId,
    pub(crate) entry_params: Vec<HirType>,
    pub(crate) entry_return_type: HirType,
}

impl ResolvedContract {
    pub(crate) fn entry_returns_wit_result(&self) -> bool {
        let mut ret = &self.entry_return_type;
        while let HirType::Promise(inner) = ret {
            ret = inner;
        }
        matches!(ret, HirType::Generic { base, .. } if base == "Result")
    }
}

/// Resolves module bindings, shadowing, and contracts for WAFFLE lowering.
pub(crate) fn resolve_contract(hir: &HirModule) -> Result<ResolvedContract> {
    ensure!(
        hir.init.is_empty(),
        "Module initialization is unsupported by the WAFFLE backend"
    );
    ensure!(
        !hir.functions.is_empty(),
        "Module must declare at least one function"
    );

    let mut functions_by_name = BTreeMap::new();
    for func in &hir.functions {
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
    let mut uses_p3_clocks = false;

    for (name, params, ret) in &hir.extern_funcs {
        match name.as_str() {
            "waitFor" => {
                ensure!(
                    params.len() == 1 && matches!(params[0], HirType::Number),
                    "waitFor signature must be (milliseconds: number) => Promise<void>"
                );
                ensure!(
                    matches!(ret, HirType::Promise(inner) if matches!(**inner, HirType::Void)),
                    "waitFor must return Promise<void>"
                );
                uses_p3_clocks = true;
                intrinsics.insert(name.clone(), TypedIntrinsic::WaitFor);
            }
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

    let input_kind = if let Some(first_param) = entry_func.params.first() {
        match &first_param.ty {
            HirType::Named(name) if name == "ByteStream" => ResolvedInputKind::ByteStream,
            HirType::Number | HirType::Any => ResolvedInputKind::Number,
            other => bail!("Unsupported entry function parameter type: {other:?}"),
        }
    } else {
        ResolvedInputKind::Number
    };

    // Audit function bodies for shadowing of intrinsics by local variable declarations
    for func in &hir.functions {
        audit_scope_and_shadowing(func, &intrinsics)?;
    }

    Ok(ResolvedContract {
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
    let mut declared_names = BTreeSet::new();
    for param in &func.params {
        if intrinsics.contains_key(&param.name) {
            bail!(
                "Parameter '{}' in function '{}' shadows declared intrinsic of the same name",
                param.name,
                func.name
            );
        }
        declared_names.insert(param.name.clone());
    }

    check_stmts_shadowing(&func.body, &declared_names, intrinsics)?;
    Ok(())
}

fn check_stmts_shadowing(
    stmts: &[Stmt],
    outer_names: &BTreeSet<String>,
    intrinsics: &BTreeMap<String, TypedIntrinsic>,
) -> Result<()> {
    let mut current_names = outer_names.clone();
    for stmt in stmts {
        match stmt {
            Stmt::Let { name, .. } => {
                if intrinsics.contains_key(name) {
                    bail!("Local variable '{name}' illegally shadows declared intrinsic");
                }
                current_names.insert(name.clone());
            }
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                check_stmts_shadowing(then_branch, &current_names, intrinsics)?;
                if let Some(else_b) = else_branch {
                    check_stmts_shadowing(else_b, &current_names, intrinsics)?;
                }
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
                check_stmts_shadowing(body, &current_names, intrinsics)?;
            }
            Stmt::For { init, body, .. } => {
                let mut for_names = current_names.clone();
                if let Some(init_stmt) = init
                    && let Stmt::Let { name, .. } = init_stmt.as_ref()
                {
                    if intrinsics.contains_key(name) {
                        bail!("For loop local '{name}' illegally shadows declared intrinsic");
                    }
                    for_names.insert(name.clone());
                }
                check_stmts_shadowing(body, &for_names, intrinsics)?;
            }
            Stmt::Try { body, catch, finally } => {
                check_stmts_shadowing(body, &current_names, intrinsics)?;
                if let Some(c) = catch {
                    let mut catch_names = current_names.clone();
                    if let Some((_, param_name)) = &c.param {
                        if intrinsics.contains_key(param_name) {
                            bail!("Catch parameter '{param_name}' illegally shadows declared intrinsic");
                        }
                        catch_names.insert(param_name.clone());
                    }
                    check_stmts_shadowing(&c.body, &catch_names, intrinsics)?;
                }
                if let Some(f) = finally {
                    check_stmts_shadowing(f, &current_names, intrinsics)?;
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
