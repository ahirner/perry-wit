//! Logical expression types retained independently of primitive SSA representations.

use super::FunctionLowerer;
use perry_hir::{
    ir::{BinaryOp, Expr},
    types::Type as HirType,
};

/// Perry's private for-of holder keeps a string snapshot with scalar indexing.
/// It cannot be named by a TypeScript type annotation or cross a call boundary.
const SCALAR_ITERATION: &str = "perry:scalar-iteration";

pub(super) fn identity_kind(ty: &HirType) -> Option<&'static str> {
    match ty {
        HirType::Promise(_) => Some("Promise"),
        ty if crate::waffle_backend::bytes::is_byte_view(ty) => Some("Uint8Array"),
        ty if crate::waffle_backend::decoder::is_decoder(ty) => Some("TextDecoder"),
        HirType::Named(name) if name == "ByteStream" => Some("ByteStream"),
        _ => None,
    }
}

pub(super) fn is_reference(ty: &HirType) -> bool {
    match ty {
        ty if crate::waffle_backend::decoder::is_decoder(ty) => true,
        HirType::String | HirType::Promise(_) => true,
        HirType::Array(inner) => **inner == HirType::String,
        HirType::Named(name) => name == SCALAR_ITERATION || name == "Uint8Array",
        HirType::Union(types) => types.iter().any(is_reference),
        _ => false,
    }
}

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
            Expr::TextDecoderNew { .. } => {
                HirType::Named(crate::waffle_backend::decoder::DECODER_TYPE.into())
            }
            Expr::TextDecoderDecode { .. } | Expr::TextDecoderEncoding(_) => HirType::String,
            Expr::TextDecoderFatal(_) | Expr::TextDecoderIgnoreBom(_) => HirType::Boolean,
            Expr::PropertyGet {
                object, property, ..
            } if crate::waffle_backend::decoder::is_decoder(&self.infer_expr_type(object)) => {
                if property == "encoding" {
                    HirType::String
                } else {
                    HirType::Boolean
                }
            }
            Expr::Uint8ArrayNew(_) => HirType::Named("Uint8Array".into()),
            Expr::Uint8ArrayLength(_) => HirType::Number,
            Expr::PropertyGet {
                object, property, ..
            } if property == "length"
                && (self.is_string(object)
                    || crate::waffle_backend::text_or_bytes::is_text_or_bytes(
                        &self.infer_expr_type(object),
                    )
                    || self.is_scalar_iteration(object)
                    || matches!(self.infer_expr_type(object), HirType::Array(_))) =>
            {
                HirType::Number
            }
            Expr::PropertyGet {
                object, property, ..
            } if crate::waffle_backend::bytes::is_byte_view(&self.infer_expr_type(object))
                && matches!(property.as_str(), "length" | "byteLength" | "byteOffset") =>
            {
                HirType::Number
            }
            Expr::Uint8ArrayGet { .. } => HirType::Union(vec![HirType::Number, HirType::Void]),
            Expr::Uint8ArraySet { value, .. } => self.infer_expr_type(value),
            Expr::PutValueSet { value, .. } => self.infer_expr_type(value),
            Expr::LocalSet(_, value) => self.infer_expr_type(value),
            Expr::ForOfToArray(_) => HirType::Named(SCALAR_ITERATION.into()),
            Expr::TypeOf(_)
            | Expr::String(_)
            | Expr::TemplateStringCoerce(_)
            | Expr::StringCoerce(_)
            | Expr::StringFromCodePoint(_)
            | Expr::ArrayJoin { .. } => HirType::String,
            Expr::Await(inner) => match self.infer_expr_type(inner) {
                HirType::Promise(result) => *result,
                result => result,
            },
            Expr::Number(_) | Expr::Integer(_) | Expr::Update { .. } => HirType::Number,
            Expr::Bool(_)
            | Expr::Compare { .. }
            | Expr::Unary {
                op: perry_hir::ir::UnaryOp::Not,
                ..
            } => HirType::Boolean,
            Expr::LocalGet(id) => self
                .narrowings
                .get(id)
                .or_else(|| self.local_types.get(id))
                .cloned()
                .unwrap_or(HirType::Any),
            Expr::IndexGet { object, .. } if self.is_scalar_iteration(object) => HirType::String,
            Expr::IndexGet { object, .. }
                if crate::waffle_backend::bytes::is_byte_view(&self.infer_expr_type(object)) =>
            {
                HirType::Union(vec![HirType::Number, HirType::Void])
            }
            Expr::IndexGet { .. } => HirType::Union(vec![HirType::String, HirType::Void]),
            Expr::Undefined => HirType::Void,
            Expr::Call { callee, .. } => {
                if let Some(plan) = &self.contract.promises
                    && let Some(target) =
                        crate::waffle_backend::promises::TaskTarget::from_callee(callee)
                    && let Some(task) = plan.tasks.get(&target)
                {
                    return HirType::Promise(Box::new(task.result.clone()));
                }
                if let Expr::PropertyGet {
                    object, property, ..
                } = callee.as_ref()
                {
                    if crate::waffle_backend::decoder::is_decoder(&self.infer_expr_type(object))
                        && property == "decode"
                    {
                        return HirType::String;
                    }
                    if crate::waffle_backend::bytes::is_byte_view(&self.infer_expr_type(object))
                        && matches!(property.as_str(), "subarray" | "slice")
                    {
                        return HirType::Named("Uint8Array".into());
                    }
                    if property == "slice"
                        || property == "charAt"
                        || property == "toLowerCase"
                        || property == "toUpperCase"
                        || property == "join"
                    {
                        return HirType::String;
                    } else if property == "codePointAt" {
                        return HirType::Union(vec![HirType::Number, HirType::Void]);
                    } else if property == "indexOf" || property == "search" {
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

    pub(super) fn is_scalar_iteration(&self, expr: &Expr) -> bool {
        self.infer_expr_type(expr) == HirType::Named(SCALAR_ITERATION.into())
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
