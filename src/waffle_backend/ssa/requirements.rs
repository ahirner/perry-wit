//! String runtime dependencies from function signatures and expressions.

use crate::waffle_backend::{strings::RequiredStringHelpers, visit};
use perry_hir::{
    ir::{Expr, Module as HirModule, Stmt},
    types::Type as HirType,
};

pub(super) fn scan_module_string_requirements(hir: &HirModule) -> RequiredStringHelpers {
    let mut reqs = RequiredStringHelpers::default();

    for func in &hir.functions {
        reqs.objects |= crate::waffle_backend::objects::contains_object(&func.return_type)
            || func
                .params
                .iter()
                .any(|param| crate::waffle_backend::objects::contains_object(&param.ty));
        for param in &func.params {
            if super::super::decoder::is_decoder(&param.ty) {
                reqs.needs_strings = true;
                reqs.decoder = true;
            }
            if type_has_string(&param.ty) {
                reqs.needs_strings = true;
            }
        }
        if type_has_string(&func.return_type) {
            reqs.needs_strings = true;
        }
        visit::visit_statement_nodes(&func.body, &mut |statement| {
            if let Stmt::Let { ty, .. } = statement {
                reqs.needs_strings |= type_has_string(ty);
            }
        });
        visit::visit_function_expressions(func, &mut |expr| {
            scan_expr_requirements(expr, &mut reqs)
        });
    }
    visit::visit_statements(&hir.init, &mut |expr| {
        scan_expr_requirements(expr, &mut reqs)
    });
    reqs.needs_strings |= reqs.objects;
    reqs
}

fn scan_expr_requirements(expr: &Expr, reqs: &mut RequiredStringHelpers) {
    match expr {
        Expr::Object(_) => reqs.objects = true,
        Expr::New { class_name, .. } if class_name.starts_with("__AnonShape_") => {
            reqs.objects = true
        }
        Expr::TextDecoderNew { .. }
        | Expr::TextDecoderDecode { .. }
        | Expr::TextDecoderEncoding(_)
        | Expr::TextDecoderFatal(_)
        | Expr::TextDecoderIgnoreBom(_) => {
            reqs.needs_strings = true;
            reqs.decoder = true;
        }
        Expr::TypeOf(_) | Expr::String(_) | Expr::ForOfToArray(_) | Expr::RegExp { .. } => {
            reqs.needs_strings = true;
        }
        Expr::StringFromCodePoint(_) => {
            reqs.needs_strings = true;
            reqs.from_code_point = true;
        }
        Expr::TemplateStringCoerce(_) => {
            reqs.needs_strings = true;
        }
        Expr::ArrayJoin { .. } => {
            reqs.needs_strings = true;
            reqs.join = true;
        }
        Expr::Call { callee, .. } => {
            if matches!(callee.as_ref(), Expr::ExternFuncRef { return_type, .. } if super::super::decoder::is_decoder(return_type))
            {
                reqs.needs_strings = true;
                reqs.decoder = true;
            }
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
        HirType::Object(_) => true,
        _ => false,
    }
}
