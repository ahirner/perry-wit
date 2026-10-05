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
        ty if crate::waffle_backend::objects::is_object(ty) => Some("object"),
        HirType::Promise(_) => Some("Promise"),
        ty if crate::waffle_backend::bytes::is_byte_view(ty) => Some("Uint8Array"),
        ty if crate::waffle_backend::bytes::is_array_buffer(ty) => Some("ArrayBuffer"),
        ty if crate::waffle_backend::decoder::is_decoder(ty) => Some("TextDecoder"),
        ty if crate::waffle_backend::http::fetch::is_response(ty) => Some("Response"),
        ty if crate::waffle_backend::http::headers::is_headers(ty) => Some("Headers"),
        ty if crate::waffle_backend::http::request::is_request(ty) => Some("Request"),
        ty if crate::waffle_backend::http::is_response(ty) => Some("HttpResponse"),
        ty if crate::waffle_backend::date::is_date(ty) => Some("Date"),
        ty if crate::waffle_backend::time::is_time(ty) => {
            crate::waffle_backend::time::TimeKind::of(ty).map(|kind| match kind {
                crate::waffle_backend::time::TimeKind::Instant => "Temporal.Instant",
                crate::waffle_backend::time::TimeKind::PlainDateTime => "Temporal.PlainDateTime",
            })
        }
        ty if crate::waffle_backend::filesystem::is_stats(ty) => Some("Stats"),
        HirType::Array(inner) if **inner == HirType::String => Some("string[]"),
        HirType::Array(_) => Some("array"),
        HirType::Named(name) if name == "ByteStream" => Some("ByteStream"),
        _ => None,
    }
}

pub(crate) fn is_reference(ty: &HirType) -> bool {
    match ty {
        ty if crate::waffle_backend::nullable::inner(ty).is_some() => true,
        ty if crate::waffle_backend::values::is_boxed(ty) => true,
        ty if crate::waffle_backend::objects::is_object(ty) => true,
        ty if crate::waffle_backend::decoder::is_decoder(ty)
            || crate::waffle_backend::date::is_date(ty)
            || crate::waffle_backend::time::is_time(ty)
            || crate::waffle_backend::http::is_response(ty)
            || crate::waffle_backend::http::headers::is_headers(ty)
            || crate::waffle_backend::http::request::is_request(ty) =>
        {
            true
        }
        ty if crate::waffle_backend::filesystem::is_stats(ty) => true,
        ty if crate::waffle_backend::values::is_string_type(ty) => true,
        HirType::Tuple(_) | HirType::Promise(_) | HirType::BigInt => true,
        HirType::Array(_) => true,
        HirType::Named(name) => {
            name == SCALAR_ITERATION
                || name == "Uint8Array"
                || name == "ArrayBuffer"
                || name == crate::waffle_backend::values::ARRAY_TYPE
        }
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
            ty if crate::waffle_backend::values::is_string_type(ty) => Some(Self::Present),
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
            Expr::ArrayPush { .. } => HirType::Number,
            Expr::IndexGet { object, .. } if matches!(self.infer_expr_type(object),HirType::Array(inner) if *inner!=HirType::String) =>
            {
                let HirType::Array(inner) = self.infer_expr_type(object) else {
                    unreachable!()
                };
                *inner
            }
            Expr::IndexGet { object, index }
                if matches!(self.infer_expr_type(object), HirType::Tuple(_)) =>
            {
                let HirType::Tuple(types) = self.infer_expr_type(object) else {
                    unreachable!()
                };
                super::tuples::element_type(&types, index).unwrap_or(HirType::Unknown)
            }
            Expr::PropertyGet {
                object, property, ..
            } if crate::waffle_backend::http::request::is_request(
                &self.infer_expr_type(object),
            ) =>
            {
                match property.as_str() {
                    "headers" => {
                        HirType::Named(crate::waffle_backend::http::headers::HEADERS_TYPE.into())
                    }
                    "bodyUsed" => HirType::Boolean,
                    _ => HirType::String,
                }
            }
            Expr::PropertyGet {
                object, property, ..
            } if crate::waffle_backend::http::fetch::is_response(&self.infer_expr_type(object)) => {
                match property.as_str() {
                    "url" | "statusText" => HirType::String,
                    "headers" => {
                        HirType::Named(crate::waffle_backend::http::headers::HEADERS_TYPE.into())
                    }
                    "ok" | "bodyUsed" | "redirected" => HirType::Boolean,
                    _ => HirType::Number,
                }
            }
            Expr::PropertyGet {
                object, property, ..
            } if crate::waffle_backend::http::is_response(&self.infer_expr_type(object)) => {
                if property == "body" {
                    HirType::Named("Uint8Array".into())
                } else {
                    HirType::Number
                }
            }
            Expr::Logical { left, right, .. }
                if self.infer_expr_type(left) == HirType::Boolean
                    && self.infer_expr_type(right) == HirType::Boolean =>
            {
                HirType::Boolean
            }
            Expr::PropertyGet { object, .. }
                if crate::waffle_backend::time::is_time(&self.infer_expr_type(object)) =>
            {
                HirType::Number
            }
            Expr::Array(_) => HirType::Named(crate::waffle_backend::values::ARRAY_TYPE.into()),
            Expr::PropertyGet { object, .. } | Expr::IndexGet { object, .. }
                if crate::waffle_backend::values::has_dynamic_properties(
                    &self.infer_expr_type(object),
                ) =>
            {
                crate::waffle_backend::values::value_type()
            }
            Expr::JsonParse(_)
            | Expr::JsonParseTyped { .. }
            | Expr::JsonParseWithReviver(..)
            | Expr::JsonParseReviver { .. } => crate::waffle_backend::values::value_type(),
            Expr::JsonStringify(_) | Expr::JsonStringifyFull(..) => {
                HirType::Union(vec![HirType::String, HirType::Void])
            }
            Expr::ObjectAssign { target, .. } => self.infer_expr_type(target),
            Expr::ObjectKeys(_) | Expr::ObjectValues(_) => {
                HirType::Array(Box::new(HirType::String))
            }
            Expr::In { .. } | Expr::ArrayIsArray(_) => HirType::Boolean,
            Expr::Null => HirType::Null,
            Expr::Delete(_) => HirType::Boolean,
            Expr::Object(_) => self.object_literal_type(expr),
            Expr::New { class_name, .. }
                if self.contract.literal_shapes.contains_key(class_name) =>
            {
                self.object_literal_type(expr)
            }
            Expr::PropertyGet {
                object, property, ..
            } if crate::waffle_backend::objects::is_object(&self.infer_expr_type(object)) => {
                self.object_property_type(object, &Expr::String(property.clone()))
            }
            Expr::IndexGet { object, index, .. }
                if crate::waffle_backend::objects::is_object(&self.infer_expr_type(object)) =>
            {
                self.object_property_type(object, index)
            }
            Expr::PropertySet { value, .. } | Expr::IndexSet { value, .. } => {
                self.infer_expr_type(value)
            }
            Expr::PropertyGet {
                object, property, ..
            } if crate::waffle_backend::filesystem::is_stats(&self.infer_expr_type(object))
                && matches!(property.as_str(), "size" | "mtimeMs") =>
            {
                HirType::Number
            }
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
                    || matches!(
                        self.infer_expr_type(object),
                        HirType::Array(_) | HirType::Tuple(_)
                    )) =>
            {
                HirType::Number
            }
            Expr::PropertyGet {
                object, property, ..
            } if crate::waffle_backend::bytes::is_byte_storage(&self.infer_expr_type(object))
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
                .or_else(|| {
                    self.registry
                        .module_state
                        .as_ref()?
                        .bindings
                        .get(id)
                        .map(|binding| &binding.ty)
                })
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
            Expr::Call { callee, args, .. } => {
                if self.timer_value(callee).is_some() {
                    return self
                        .timer_value_type(args)
                        .map(|ty| HirType::Promise(Box::new(ty)))
                        .unwrap_or(HirType::Any);
                }
                if let Some(operation) = self.combinator(callee) {
                    return self
                        .combinator_type(operation, args)
                        .unwrap_or(HirType::Any);
                }
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
                    if crate::waffle_backend::http::headers::is_headers(
                        &self.infer_expr_type(object),
                    ) {
                        return match property.as_str() {
                            "has" => HirType::Boolean,
                            "get" => HirType::Union(vec![HirType::String, HirType::Null]),
                            _ => HirType::Void,
                        };
                    }
                    if (crate::waffle_backend::http::fetch::is_response(
                        &self.infer_expr_type(object),
                    ) || crate::waffle_backend::http::request::is_request(
                        &self.infer_expr_type(object),
                    )) && let Some(method) =
                        crate::waffle_backend::http::body::BodyMethod::named(property)
                    {
                        return HirType::Promise(Box::new(method.result()));
                    }
                    if crate::waffle_backend::http::is_response(&self.infer_expr_type(object)) {
                        return if property == "headerName" {
                            HirType::String
                        } else {
                            HirType::Named("Uint8Array".into())
                        };
                    }
                    if crate::waffle_backend::time::is_time(&self.infer_expr_type(object)) {
                        return if property == "toString" {
                            HirType::String
                        } else {
                            self.infer_expr_type(object)
                        };
                    }
                    if crate::waffle_backend::date::is_date(&self.infer_expr_type(object)) {
                        return if property == "toISOString" {
                            HirType::String
                        } else {
                            HirType::Number
                        };
                    }
                    if crate::waffle_backend::filesystem::is_stats(&self.infer_expr_type(object))
                        && matches!(property.as_str(), "isFile" | "isDirectory")
                    {
                        return HirType::Boolean;
                    }
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
            Expr::Conditional {
                then_expr,
                else_expr,
                ..
            } => {
                let left = self.infer_expr_type(then_expr);
                if crate::waffle_backend::wit::same_type(&left, &self.infer_expr_type(else_expr)) {
                    left
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
