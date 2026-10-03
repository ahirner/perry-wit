//! Logical expression types retained independently of primitive SSA representations.

use super::FunctionLowerer;
use perry_hir::{
    ir::{BinaryOp, Expr},
    types::Type as HirType,
};

impl FunctionLowerer<'_> {
    pub(super) fn infer_expr_type(&self, expr: &Expr) -> HirType {
        match expr {
            Expr::String(_) | Expr::TemplateStringCoerce(_) | Expr::StringCoerce(_) => {
                HirType::String
            }
            Expr::Await(inner) => match self.infer_expr_type(inner) {
                HirType::Promise(result) => *result,
                result => result,
            },
            Expr::Number(_) | Expr::Integer(_) => HirType::Number,
            Expr::Bool(_) | Expr::Compare { .. } => HirType::Boolean,
            Expr::LocalGet(id) => self.local_types.get(id).cloned().unwrap_or(HirType::Any),
            Expr::IndexGet { .. } => HirType::String,
            Expr::Call { callee, .. } => {
                if let Expr::PropertyGet { property, .. } = callee.as_ref() {
                    if property == "slice" || property == "charAt" {
                        return HirType::String;
                    } else if property == "indexOf" {
                        return HirType::Number;
                    }
                } else if let Expr::FuncRef(fid) = callee.as_ref()
                    && let Some(info) = self.registry.functions.get(fid)
                {
                    return info.success_type().clone();
                }
                if let Expr::ExternFuncRef { return_type, .. } = callee.as_ref() {
                    return_type.clone()
                } else {
                    HirType::Any
                }
            }
            Expr::Binary { op, left, right } => {
                if *op == BinaryOp::Add && (self.is_string(left) || self.is_string(right)) {
                    HirType::String
                } else {
                    HirType::Number
                }
            }
            _ => HirType::Any,
        }
    }

    pub(super) fn is_string(&self, expr: &Expr) -> bool {
        matches!(self.infer_expr_type(expr), HirType::String)
    }
}
