//! Logical expression types retained independently of primitive SSA representations.

use super::FunctionLowerer;
use perry_hir::{
    ir::{BinaryOp, Expr},
    types::Type as HirType,
};

/// Descriptor zero represents undefined only within the string-or-undefined union.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum StringKind {
    Present,
    Optional,
    Undefined,
}

impl StringKind {
    pub(super) fn of(ty: &HirType) -> Option<Self> {
        match ty {
            HirType::String => Some(Self::Present),
            HirType::Void => Some(Self::Undefined),
            HirType::Union(types)
                if types.len() == 2
                    && types.contains(&HirType::String)
                    && types.contains(&HirType::Void) =>
            {
                Some(Self::Optional)
            }
            _ => None,
        }
    }
}

impl FunctionLowerer<'_> {
    pub(super) fn infer_expr_type(&self, expr: &Expr) -> HirType {
        match expr {
            Expr::String(_)
            | Expr::TemplateStringCoerce(_)
            | Expr::StringCoerce(_)
            | Expr::StringFromCodePoint(_) => HirType::String,
            Expr::Await(inner) => match self.infer_expr_type(inner) {
                HirType::Promise(result) => *result,
                result => result,
            },
            Expr::Number(_) | Expr::Integer(_) => HirType::Number,
            Expr::Bool(_) | Expr::Compare { .. } => HirType::Boolean,
            Expr::LocalGet(id) => self.local_types.get(id).cloned().unwrap_or(HirType::Any),
            Expr::IndexGet { .. } => HirType::Union(vec![HirType::String, HirType::Void]),
            Expr::Undefined => HirType::Void,
            Expr::Call { callee, .. } => {
                if let Expr::PropertyGet { property, .. } = callee.as_ref() {
                    if property == "slice"
                        || property == "charAt"
                        || property == "toLowerCase"
                        || property == "toUpperCase"
                        || property == "join"
                    {
                        return HirType::String;
                    } else if property == "codePointAt" {
                        return HirType::Union(vec![HirType::Number, HirType::Void]);
                    } else if property == "indexOf" {
                        return HirType::Number;
                    } else if property == "split" {
                        return HirType::Array(Box::new(HirType::String));
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
        matches!(
            StringKind::of(&self.infer_expr_type(expr)),
            Some(StringKind::Present | StringKind::Optional)
        )
    }

    pub(super) fn is_string_or_undefined(&self, expr: &Expr) -> bool {
        StringKind::of(&self.infer_expr_type(expr)).is_some()
    }

    pub(super) fn is_optional_number(&self, expr: &Expr) -> bool {
        self.infer_expr_type(expr) == HirType::Union(vec![HirType::Number, HirType::Void])
    }
}
