//! Logical JSON shapes shared by decoding, serialization and copy normalization.
use pliron::{
    builtin::types::{FP64Type, IntegerType},
    context::Context,
    r#type::{TypeHandle, TypedHandle},
};
use std::collections::HashSet;

use crate::{
    ArrayType, JsonType, NumberStringType, OptionalType, RecordKind, RecordType, StringType,
    VariantType,
};

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
    NumberString,
    Optional(TypeHandle),
    Record(TypedHandle<RecordType>),
    Array(TypeHandle),
    Dictionary {
        entry: TypedHandle<RecordType>,
        key: TypeHandle,
        value: TypeHandle,
    },
    Variant(TypedHandle<VariantType>),
}

impl JsonSchema {
    pub fn supports_descriptor(ctx: &Context, root: TypeHandle) -> bool {
        Self::supports_input(ctx, root, false)
    }

    fn supports_input(ctx: &Context, root: TypeHandle, cursor: bool) -> bool {
        let mut pending = vec![root];
        let mut visited = HashSet::new();
        while let Some(ty) = pending.pop() {
            if !visited.insert(ty) {
                continue;
            }
            if ty.deref(ctx).is::<crate::NumericType>() {
                continue;
            }
            match Self::of(ctx, ty) {
                Some(
                    JsonSchema::Value
                    | JsonSchema::Number
                    | JsonSchema::Boolean
                    | JsonSchema::String
                    | JsonSchema::NumberString
                    | JsonSchema::Integer { width: 64, .. },
                ) => {}
                Some(JsonSchema::Array(element) | JsonSchema::Optional(element)) => {
                    pending.push(element)
                }
                Some(JsonSchema::Record(record)) => {
                    let record = record.deref(ctx);
                    if cursor && !matches!(record.kind(), RecordKind::Object | RecordKind::Tuple) {
                        return false;
                    }
                    let Ok(fields) = record.fields() else {
                        return false;
                    };
                    pending.extend(fields.iter().map(|(_, ty)| *ty));
                }
                Some(JsonSchema::Dictionary { entry, .. }) if !cursor => pending.push(entry.into()),
                Some(JsonSchema::Variant(variant)) => {
                    for (_, payload) in variant.deref(ctx).cases() {
                        if let Some(payload) = payload {
                            // Variant payload storage has one scalar slot, unlike record/array slots.
                            if crate::present_type(ctx, *payload)
                                .deref(ctx)
                                .is::<crate::NumericType>()
                            {
                                return false;
                            }
                            pending.push(*payload);
                        }
                    }
                }
                _ => return false,
            }
        }
        true
    }

    /// Classify logical types without relying on generated record names.
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
        } else if definition.is::<NumberStringType>() {
            Self::NumberString
        } else if let Some(optional) = definition.downcast_ref::<OptionalType>() {
            Self::Optional(optional.value())
        } else if let Ok(record) = TypedHandle::<RecordType>::from_handle(ty, ctx) {
            Self::Record(record)
        } else if let Some(array) = definition.downcast_ref::<ArrayType>() {
            let element = array.element();
            match TypedHandle::<RecordType>::from_handle(element, ctx) {
                Ok(record) if record.deref(ctx).kind() == RecordKind::DictionaryEntry => {
                    let fields = record.deref(ctx);
                    Self::Dictionary {
                        entry: record,
                        key: fields.field("_0")?.1,
                        value: fields.field("_1")?.1,
                    }
                }
                _ => Self::Array(element),
            }
        } else if let Ok(variant) = TypedHandle::<VariantType>::from_handle(ty, ctx) {
            Self::Variant(variant)
        } else {
            return None;
        })
    }

    /// Dictionaries use an array of entries as native storage, despite encoding as objects.
    pub(crate) fn storage_element(self) -> Option<TypeHandle> {
        match self {
            Self::Array(element) => Some(element),
            Self::Dictionary { entry, .. } => Some(entry.into()),
            _ => None,
        }
    }

    /// Whether every reachable type can be built directly from the JSON lexer.
    pub fn supports_cursor(ctx: &Context, root: TypeHandle) -> bool {
        Self::supports_input(ctx, root, true)
    }
}
