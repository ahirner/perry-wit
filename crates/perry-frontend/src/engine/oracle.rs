//! Test-only oracle comparison between the legacy inference subsystem and the new engine.
//!
//! Runs both engines on identical lowered HIR modules and diffs binding, parameter, return,
//! field, and element types.
use super::boundary::{self, ModuleIndex};
use super::types::TypeTable;
use crate::{hir, hir_compat, infer, modules};
use perry_hir::{
    ir::{Expr, Function, Module as HirModule, Stmt},
    types::Type as HirType,
    walker::{stmt_any_expr, walk_expr_children},
};
use pliron::context::Context;
use std::collections::{BTreeMap, HashMap};

/// Normalized type summary of a typed HIR module for oracle diffing.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct ModuleSummary {
    pub(crate) bindings: BTreeMap<String, String>,
    pub(crate) params: BTreeMap<String, String>,
    pub(crate) returns: BTreeMap<String, String>,
    pub(crate) fields: BTreeMap<String, String>,
    pub(crate) elements: BTreeMap<String, String>,
}

/// One typed difference between the legacy oracle and the new engine.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OracleDiff {
    pub(crate) category: &'static str,
    pub(crate) key: String,
    pub(crate) old: Option<String>,
    pub(crate) new: Option<String>,
}

/// Canonical string representation of a `HirType` (sorts union members and object keys).
pub(crate) fn canonical_hir_type(ty: &HirType) -> String {
    match ty {
        HirType::Any => "any".into(),
        HirType::Unknown => "unknown".into(),
        HirType::Void => "undefined".into(),
        HirType::Never => "never".into(),
        HirType::Null => "null".into(),
        HirType::Boolean => "boolean".into(),
        HirType::Number => "number".into(),
        HirType::Int32 => "i32".into(),
        HirType::BigInt => "bigint".into(),
        HirType::String => "string".into(),
        HirType::Symbol => "symbol".into(),
        HirType::StringLiteral(s) => format!("{s:?}"),
        HirType::Named(s) => s.clone(),
        HirType::TypeVar(s) => format!("${s}"),
        HirType::Array(inner) => format!("{}[]", canonical_hir_type(inner)),
        HirType::Promise(inner) => format!("Promise<{}>", canonical_hir_type(inner)),
        HirType::Tuple(items) => {
            let parts: Vec<_> = items.iter().map(canonical_hir_type).collect();
            format!("[{}]", parts.join(", "))
        }
        HirType::Union(items) => {
            let mut parts: Vec<String> = Vec::new();
            flatten_union_strings(items, &mut parts);
            parts.sort_unstable();
            parts.dedup();
            format!("({})", parts.join(" | "))
        }
        HirType::Generic { base, type_args } => {
            let args: Vec<_> = type_args.iter().map(canonical_hir_type).collect();
            format!("{base}<{}>", args.join(", "))
        }
        HirType::Object(obj) => {
            let mut props: Vec<_> = obj
                .properties
                .iter()
                .map(|(k, v)| {
                    let opt = if v.optional { "?" } else { "" };
                    format!("{k}{opt}: {}", canonical_hir_type(&v.ty))
                })
                .collect();
            props.sort_unstable();
            if let Some(idx) = &obj.index_signature {
                props.push(format!("[string]: {}", canonical_hir_type(idx)));
            }
            format!("{{ {} }}", props.join("; "))
        }
        HirType::Function(f) => {
            let params: Vec<_> = f
                .params
                .iter()
                .map(|(_, ty, opt)| {
                    let q = if *opt { "?" } else { "" };
                    format!("{}{q}", canonical_hir_type(ty))
                })
                .collect();
            format!(
                "({}) => {}",
                params.join(", "),
                canonical_hir_type(&f.return_type)
            )
        }
    }
}

fn flatten_union_strings(items: &[HirType], out: &mut Vec<String>) {
    for item in items {
        if let HirType::Union(nested) = item {
            flatten_union_strings(nested, out);
        } else {
            out.push(canonical_hir_type(item));
        }
    }
}

/// Extract bindings, parameters, returns, class fields, and array/dict element types from HIR.
pub(crate) fn summarize_hir(hir: &HirModule) -> ModuleSummary {
    let mut summary = ModuleSummary::default();
    collect_stmts("init", &hir.init, &mut summary);

    for function in &hir.functions {
        collect_function(&function.name, function, &mut summary);
    }
    for class in &hir.classes {
        if hir_compat::is_object_literal(&class.name) {
            continue;
        }
        for field in &class.fields {
            let key = format!("{}.{}", class.name, field.name);
            record_type(&mut summary.fields, &mut summary.elements, key, &field.ty);
        }
        for field in &class.static_fields {
            let key = format!("{}::{}", class.name, field.name);
            record_type(&mut summary.fields, &mut summary.elements, key, &field.ty);
        }
        if let Some(ctor) = &class.constructor {
            let scope = format!("{}::constructor", class.name);
            collect_function(&scope, ctor, &mut summary);
        }
        for method in &class.methods {
            let scope = format!("{}::{}", class.name, method.name);
            collect_function(&scope, method, &mut summary);
        }
        for method in &class.static_methods {
            let scope = format!("{}::static_{}", class.name, method.name);
            collect_function(&scope, method, &mut summary);
        }
        for (name, getter) in &class.getters {
            let scope = format!("{}::get_{name}", class.name);
            collect_function(&scope, getter, &mut summary);
        }
        for (name, setter) in &class.setters {
            let scope = format!("{}::set_{name}", class.name);
            collect_function(&scope, setter, &mut summary);
        }
    }
    summary
}

fn record_type(
    target: &mut BTreeMap<String, String>,
    elements: &mut BTreeMap<String, String>,
    key: String,
    ty: &HirType,
) {
    if let HirType::Array(elem) = ty {
        elements.insert(key.clone(), canonical_hir_type(elem));
    } else if let HirType::Object(obj) = ty
        && let Some(idx) = &obj.index_signature
    {
        elements.insert(key.clone(), canonical_hir_type(idx));
    }
    target.insert(key, canonical_hir_type(ty));
}

fn collect_function(scope: &str, function: &Function, summary: &mut ModuleSummary) {
    summary
        .returns
        .insert(scope.to_owned(), canonical_hir_type(&function.return_type));
    for (i, param) in function.params.iter().enumerate() {
        let key = format!("{scope}#{i}:{}", param.name);
        record_type(&mut summary.params, &mut summary.elements, key, &param.ty);
        if let Some(default) = &param.default {
            collect_expr(scope, default, summary);
        }
    }
    collect_stmts(scope, &function.body, summary);
}

fn collect_stmts(scope: &str, stmts: &[Stmt], summary: &mut ModuleSummary) {
    for stmt in stmts {
        if let Stmt::Let {
            id, name, ty, init, ..
        } = stmt
        {
            let key = format!("{scope}::let:{name}@{id}");
            record_type(&mut summary.bindings, &mut summary.elements, key, ty);
            if let Some(init) = init {
                collect_expr(scope, init, summary);
            }
            continue;
        }
        match stmt {
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
                collect_expr(scope, condition, summary);
                collect_stmts(scope, then_branch, summary);
                if let Some(else_branch) = else_branch {
                    collect_stmts(scope, else_branch, summary);
                }
            }
            Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                collect_expr(scope, condition, summary);
                collect_stmts(scope, body, summary);
            }
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => {
                if let Some(init) = init {
                    collect_stmts(scope, std::slice::from_ref(init), summary);
                }
                if let Some(condition) = condition {
                    collect_expr(scope, condition, summary);
                }
                if let Some(update) = update {
                    collect_expr(scope, update, summary);
                }
                collect_stmts(scope, body, summary);
            }
            Stmt::Try {
                body,
                catch,
                finally,
                ..
            } => {
                collect_stmts(scope, body, summary);
                if let Some(catch) = catch {
                    collect_stmts(scope, &catch.body, summary);
                }
                if let Some(finally) = finally {
                    collect_stmts(scope, finally, summary);
                }
            }
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                collect_expr(scope, discriminant, summary);
                for case in cases {
                    if let Some(test) = &case.test {
                        collect_expr(scope, test, summary);
                    }
                    collect_stmts(scope, &case.body, summary);
                }
            }
            Stmt::Labeled { body, .. } => {
                collect_stmts(scope, std::slice::from_ref(body), summary);
            }
            other => {
                stmt_any_expr(other, &mut |expr| {
                    collect_expr(scope, expr, summary);
                    false
                });
            }
        }
    }
}

fn collect_expr(scope: &str, expr: &Expr, summary: &mut ModuleSummary) {
    if let Expr::Closure {
        func_id,
        params,
        return_type,
        body,
        ..
    } = expr
    {
        let closure_scope = format!("{scope}::closure@{func_id}");
        summary
            .returns
            .insert(closure_scope.clone(), canonical_hir_type(return_type));
        for (i, param) in params.iter().enumerate() {
            let key = format!("{closure_scope}#{i}:{}", param.name);
            record_type(&mut summary.params, &mut summary.elements, key, &param.ty);
        }
        collect_stmts(&closure_scope, body, summary);
        return;
    }
    walk_expr_children(expr, &mut |child| collect_expr(scope, child, summary));
}

pub(crate) fn diff_summaries(old: &ModuleSummary, new: &ModuleSummary) -> Vec<OracleDiff> {
    let mut diffs = Vec::new();
    diff_map("binding", &old.bindings, &new.bindings, &mut diffs);
    diff_map("param", &old.params, &new.params, &mut diffs);
    diff_map("return", &old.returns, &new.returns, &mut diffs);
    diff_map("field", &old.fields, &new.fields, &mut diffs);
    diff_map("element", &old.elements, &new.elements, &mut diffs);
    diffs
}

fn diff_map(
    category: &'static str,
    old: &BTreeMap<String, String>,
    new: &BTreeMap<String, String>,
    out: &mut Vec<OracleDiff>,
) {
    for (k, v_old) in old {
        match new.get(k) {
            Some(v_new) if v_new == v_old => {}
            other => out.push(OracleDiff {
                category,
                key: k.clone(),
                old: Some(v_old.clone()),
                new: other.cloned(),
            }),
        }
    }
    for (k, v_new) in new {
        if !old.contains_key(k) {
            out.push(OracleDiff {
                category,
                key: k.clone(),
                old: None,
                new: Some(v_new.clone()),
            });
        }
    }
}

/// Result of running both the legacy oracle and the new engine on one source module.
pub(crate) struct OracleReport {
    pub(crate) old_err: Option<String>,
    pub(crate) new_err: Option<String>,
    pub(crate) template_count: usize,
    pub(crate) local_count: usize,
    pub(crate) location_count: usize,
    pub(crate) diffs: Vec<OracleDiff>,
}

/// Run both the legacy inference subsystem and the new engine on `source` and diff them.
pub(crate) fn compare_source(
    source: &str,
    identity: &str,
    sources: &BTreeMap<String, String>,
    foreign: &HashMap<String, hir::Foreign>,
) -> OracleReport {
    let mut ctx = Context::new();

    // Parse & collect AST source facts for the new engine's boundary pass.
    let loaded_new = match modules::load(source, identity, sources, foreign) {
        Ok(loaded) => loaded,
        Err(e) => {
            return OracleReport {
                old_err: Some(e.to_string()),
                new_err: Some(e.to_string()),
                template_count: 0,
                local_count: 0,
                location_count: 0,
                diffs: Vec::new(),
            };
        }
    };
    let facts = match boundary::scan_ast(&loaded_new.module, &loaded_new.locations) {
        Ok(facts) => facts,
        Err(e) => {
            return OracleReport {
                old_err: Some(e.to_string()),
                new_err: Some(e.to_string()),
                template_count: 0,
                local_count: 0,
                location_count: 0,
                diffs: Vec::new(),
            };
        }
    };
    let mut lowered_new = match hir_compat::lower(loaded_new, identity) {
        Ok(l) => l,
        Err(e) => {
            return OracleReport {
                old_err: Some(e.to_string()),
                new_err: Some(e.to_string()),
                template_count: 0,
                local_count: 0,
                location_count: 0,
                diffs: Vec::new(),
            };
        }
    };

    // Clone HIR before either engine mutates it so both start from the exact same HIR.
    let mut types = TypeTable::new();
    let index = ModuleIndex::build(
        &ctx,
        &lowered_new.module,
        &facts,
        &lowered_new.metadata.locations,
        &mut types,
    );
    let template_count = index.template_count();
    let local_count = index.local_count();
    let location_count = index.location_count();

    // Run legacy engine on a fresh lowering so metadata & callables match production exactly.
    let old_summary = {
        let loaded_old = modules::load(source, identity, sources, foreign);
        match loaded_old.and_then(|l| hir_compat::lower(l, identity)) {
            Ok(mut lowered_old) => {
                match infer::specialize_functions(
                    &ctx,
                    &mut lowered_old.module,
                    &mut lowered_old.metadata,
                    foreign,
                    lowered_old.callables,
                ) {
                    Ok(()) => {
                        infer::unresolved_types(
                            &ctx,
                            &mut lowered_old.module,
                            &lowered_old.metadata,
                            foreign,
                        );
                        Ok(summarize_hir(&lowered_old.module))
                    }
                    Err(e) => Err(e.to_string()),
                }
            }
            Err(e) => Err(e.to_string()),
        }
    };

    // Run new engine (in Milestone 1, boundary + types are live; solver grows in Milestones 2-5).
    let new_summary = super::run_engine(
        &mut ctx,
        &mut lowered_new.module,
        &mut lowered_new.metadata,
        &facts,
        foreign,
    )
    .map(|_| summarize_hir(&lowered_new.module))
    .map_err(|e| e.to_string());

    match (old_summary, new_summary) {
        (Ok(old_s), Ok(new_s)) => OracleReport {
            old_err: None,
            new_err: None,
            template_count,
            local_count,
            location_count,
            diffs: diff_summaries(&old_s, &new_s),
        },
        (old_res, new_res) => OracleReport {
            old_err: old_res.err(),
            new_err: new_res.err(),
            template_count,
            local_count,
            location_count,
            diffs: Vec::new(),
        },
    }
}
