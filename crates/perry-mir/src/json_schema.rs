//! Logical JSON shapes shared by decoding, serialization and copy normalization.
use pliron::{
    builtin::types::{FP64Type, IntegerType},
    context::Context,
    r#type::{TypeHandle, TypedHandle},
};

use crate::{ArrayType, JsonType, OptionalType, RecordKind, RecordType, StringType, VariantType};

/// Handles keep schema classification independent of mutable IR construction.
#[derive(Clone, Copy)]
pub enum JsonSchema {
    Value,
    Number,
    Integer {
        width: u32,
        signed: bool,
    },
    Boolean,
    String,
    Optional(TypeHandle),
    Record(TypedHandle<RecordType>),
    Array(TypeHandle),
    Dictionary {
        entry: TypedHandle<RecordType>,
        value: TypeHandle,
    },
    Variant(TypedHandle<VariantType>),
    ValueUnion(TypedHandle<VariantType>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Usage {
    Box,
    Graph,
    Cursor,
    Reflection,
}

impl JsonSchema {
    pub fn supports_descriptor(ctx: &Context, root: TypeHandle) -> bool {
        Self::supports_input(ctx, root, Usage::Graph)
    }

    pub fn supports_boxing(ctx: &Context, root: TypeHandle) -> bool {
        Self::supports_input(ctx, root, Usage::Box)
    }

    pub fn scalar_descriptor(ctx: &Context, ty: TypeHandle) -> Option<u32> {
        use perry_abi::layout::descriptor::{
            self, BOOLEAN, EXCEPTION, JSON_DATE, JSON_NUMERIC, JSON_VALUE, NUMBER, SIGNED_INT,
            STRING, UNSIGNED_INT,
        };
        Some(if ty.deref(ctx).is::<crate::NumericType>() {
            JSON_NUMERIC
        } else if ty.deref(ctx).is::<JsonType>() {
            JSON_VALUE
        } else if ty.deref(ctx).is::<crate::BigIntType>() {
            descriptor::JSON_BIGINT
        } else if ty.deref(ctx).is::<crate::DateType>() {
            JSON_DATE
        } else if ty.deref(ctx).is::<FP64Type>() {
            NUMBER
        } else if let Some(int) = ty.deref(ctx).downcast_ref::<IntegerType>() {
            if int.width() == 1 {
                BOOLEAN
            } else if int.signedness() == pliron::builtin::types::Signedness::Unsigned {
                UNSIGNED_INT
            } else {
                SIGNED_INT
            }
        } else if ty.deref(ctx).is::<StringType>() {
            STRING
        } else if ty.deref(ctx).is::<crate::ExceptionType>() {
            EXCEPTION
        } else {
            return None;
        })
    }

    fn supports_input(ctx: &Context, root: TypeHandle, usage: Usage) -> bool {
        crate::nested_types(ctx, [root]).all(|ty| {
            if usage != Usage::Reflection && ty.deref(ctx).is::<crate::NumericType>() {
                return true;
            }
            let schema = if usage == Usage::Box {
                if Self::scalar_descriptor(ctx, ty).is_some() {
                    return true;
                }
                crate::typed::<RecordType>(ctx, ty)
                    .map(Self::Record)
                    .or_else(|| Self::of(ctx, ty))
            } else {
                Self::of(ctx, ty)
            };
            match schema {
                Some(
                    Self::Number
                    | Self::Boolean
                    | Self::String
                    | Self::Array(_)
                    | Self::Optional(_),
                ) => true,
                Some(Self::Value) => usage != Usage::Reflection,
                Some(Self::Integer { width, .. }) => width == 64 || usage == Usage::Reflection,
                Some(Self::Dictionary { .. }) => usage != Usage::Cursor,
                Some(Self::Record(record)) => {
                    let record = record.deref(ctx);
                    record.fields().is_ok()
                        && (usage != Usage::Cursor
                            || matches!(record.kind(), RecordKind::Object | RecordKind::Tuple))
                }
                Some(Self::ValueUnion(_)) => usage != Usage::Reflection,
                Some(Self::Variant(variant)) => {
                    // Variant payload storage has one scalar slot, unlike record/array slots.
                    usage != Usage::Reflection
                        && !variant.deref(ctx).cases().iter().any(|(_, payload)| {
                            payload.is_some_and(|p| {
                                crate::present_type(ctx, p)
                                    .deref(ctx)
                                    .is::<crate::NumericType>()
                            })
                        })
                }
                None => usage == Usage::Reflection && ty.deref(ctx).is::<crate::ExceptionType>(),
            }
        })
    }

    /// Classify a MIR type into its structural JSON schema kind.
    pub fn of(ctx: &Context, ty: TypeHandle) -> Option<Self> {
        let definition = ty.deref(ctx);
        Some(if definition.is::<JsonType>() {
            Self::Value
        } else if definition.is::<FP64Type>() {
            Self::Number
        } else if let Some(integer) = definition.downcast_ref::<IntegerType>() {
            if integer.width() == 1 {
                Self::Boolean
            } else {
                Self::Integer {
                    width: integer.width(),
                    signed: integer.is_signed(),
                }
            }
        } else if definition.is::<StringType>() {
            Self::String
        } else if let Some(optional) = definition.downcast_ref::<OptionalType>() {
            Self::Optional(optional.value())
        } else if let Some(record) = RecordType::data_record(ctx, ty) {
            Self::Record(record)
        } else if let Some(array) = definition.downcast_ref::<ArrayType>() {
            let element = array.element();
            match TypedHandle::<RecordType>::from_handle(element, ctx) {
                Ok(record) if record.deref(ctx).kind() == RecordKind::DictionaryEntry => {
                    let fields = record.deref(ctx);
                    fields.field("_0")?;
                    Self::Dictionary {
                        entry: record,
                        value: fields.field("_1")?.1,
                    }
                }
                _ => Self::Array(element),
            }
        } else if let Some(union) = definition.downcast_ref::<crate::ValueUnionType>() {
            Self::ValueUnion(crate::typed::<VariantType>(ctx, union.variants())?)
        } else {
            Self::Variant(crate::typed::<VariantType>(ctx, ty)?)
        })
    }

    /// Whether every reachable type can be built directly from the JSON lexer.
    pub fn supports_cursor(ctx: &Context, root: TypeHandle) -> bool {
        Self::supports_input(ctx, root, Usage::Cursor)
    }

    /// Whether native slots can be exposed through an Error cause's live JSON view.
    pub fn supports_reflection(ctx: &Context, root: TypeHandle) -> bool {
        Self::supports_input(ctx, root, Usage::Reflection)
    }
}
