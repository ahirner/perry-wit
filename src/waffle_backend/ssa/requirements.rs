//! String runtime dependencies from function signatures and expressions.

use crate::waffle_backend::strings::RequiredStringHelpers;
use perry_hir::{
    ir::{Expr, Module as HirModule, Stmt},
    types::Type as HirType,
};

pub(super) fn scan_module_string_requirements(hir: &HirModule) -> RequiredStringHelpers {
    let mut reqs = RequiredStringHelpers::default();

    for func in &hir.functions {
        for param in &func.params {
            if type_has_string(&param.ty) {
                reqs.needs_strings = true;
            }
        }
        if type_has_string(&func.return_type) {
            reqs.needs_strings = true;
        }
        for stmt in &func.body {
            scan_stmt_requirements(stmt, &mut reqs);
        }
    }
    for stmt in &hir.init {
        scan_stmt_requirements(stmt, &mut reqs);
    }

    reqs
}

fn scan_stmt_requirements(stmt: &Stmt, reqs: &mut RequiredStringHelpers) {
    match stmt {
        Stmt::Expr(expr) | Stmt::Throw(expr) => scan_expr_requirements(expr, reqs),
        Stmt::Return(Some(expr)) => scan_expr_requirements(expr, reqs),
        Stmt::Let {
            init: Some(expr), ..
        } => scan_expr_requirements(expr, reqs),
        Stmt::If {
            condition,
            then_branch,
            else_branch,
        } => {
            scan_expr_requirements(condition, reqs);
            for s in then_branch {
                scan_stmt_requirements(s, reqs);
            }
            if let Some(eb) = else_branch {
                for s in eb {
                    scan_stmt_requirements(s, reqs);
                }
            }
        }
        Stmt::While { condition, body } => {
            scan_expr_requirements(condition, reqs);
            for s in body {
                scan_stmt_requirements(s, reqs);
            }
        }
        Stmt::Try {
            body,
            catch,
            finally,
        } => {
            for s in body {
                scan_stmt_requirements(s, reqs);
            }
            if let Some(c) = catch {
                for s in &c.body {
                    scan_stmt_requirements(s, reqs);
                }
            }
            if let Some(f) = finally {
                for s in f {
                    scan_stmt_requirements(s, reqs);
                }
            }
        }
        _ => {}
    }
}

fn scan_expr_requirements(expr: &Expr, reqs: &mut RequiredStringHelpers) {
    match expr {
        Expr::String(_) => {
            reqs.needs_strings = true;
        }
        Expr::StringFromCodePoint(arg) => {
            reqs.needs_strings = true;
            reqs.from_code_point = true;
            scan_expr_requirements(arg, reqs);
        }
        Expr::TemplateStringCoerce(arg) => {
            reqs.needs_strings = true;
            scan_expr_requirements(arg, reqs);
        }
        Expr::Call { callee, args, .. } => {
            if let Expr::PropertyGet { property, .. } = callee.as_ref() {
                match property.as_str() {
                    "indexOf" => {
                        reqs.needs_strings = true;
                        reqs.find_substring = true;
                    }
                    "codePointAt" => {
                        reqs.needs_strings = true;
                        reqs.code_point_at = true;
                    }
                    "toLowerCase" | "toUpperCase" => {
                        reqs.needs_strings = true;
                        reqs.case_convert = true;
                    }
                    "split" => {
                        reqs.needs_strings = true;
                        reqs.split = true;
                    }
                    "join" => {
                        reqs.needs_strings = true;
                        reqs.join = true;
                    }
                    "slice" | "charAt" | "charCodeAt" => {
                        reqs.needs_strings = true;
                    }
                    _ => {}
                }
            }
            scan_expr_requirements(callee, reqs);
            for arg in args {
                scan_expr_requirements(arg, reqs);
            }
        }
        Expr::Binary { left, right, .. } | Expr::Compare { left, right, .. } => {
            scan_expr_requirements(left, reqs);
            scan_expr_requirements(right, reqs);
        }
        Expr::Unary { operand, .. } => {
            scan_expr_requirements(operand, reqs);
        }
        Expr::Await(inner) => {
            scan_expr_requirements(inner, reqs);
        }
        Expr::PropertyGet {
            object, property, ..
        } => {
            if property == "length" {
                reqs.needs_strings = true;
            }
            scan_expr_requirements(object, reqs);
        }
        Expr::IndexGet { object, index, .. } => {
            scan_expr_requirements(object, reqs);
            scan_expr_requirements(index, reqs);
        }
        Expr::LocalSet(_, inner) => {
            scan_expr_requirements(inner, reqs);
        }
        _ => {}
    }
}

fn type_has_string(ty: &HirType) -> bool {
    match ty {
        HirType::String => true,
        HirType::Promise(inner) => type_has_string(inner),
        HirType::Array(inner) => type_has_string(inner),
        HirType::Generic { base, type_args } if base == "Result" => {
            type_args.iter().any(type_has_string)
        }
        HirType::Union(types) => types.iter().any(type_has_string),
        _ => false,
    }
}
