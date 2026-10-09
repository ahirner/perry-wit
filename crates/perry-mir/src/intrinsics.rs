//! Typed built-in capabilities. Signatures are verified here; native representations and
//! task scheduling belong to codegen and the runtime.
use crate::types::{
    ArrayType, BufferType, BytesType, JsonReplacerType, JsonReviverType, JsonType, OptionalType,
    StringType,
};
use pliron::{
    builtin::types::{FP64Type, IntegerType, Signedness},
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_attr, pliron_op},
    op::Op,
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::{TypeHandle, Typed},
    value::Value,
    verify_err,
};
use thiserror::Error;

#[pliron_attr(name = "perry.intrinsic_kind", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum Intrinsic {
    JsonParse,
    JsonReplacerStart,
    JsonReplacerNext,
    JsonReplacerKey,
    JsonReplacerValue,
    JsonReplacerApply,
    JsonReplacerResult,
    JsonReviverStart,
    JsonReviverNext,
    JsonReviverKey,
    JsonReviverValue,
    JsonReviverContext,
    JsonReviverApply,
    JsonReviverResult,
    JsonStringify,
    JsonStringifyOptions,
    JsonNull,
    JsonNumber,
    JsonBoolean,
    JsonString,
    JsonArray,
    JsonObject,
    JsonGet,
    JsonIndex,
    JsonAsNumber,
    JsonAsBoolean,
    JsonAsString,
    JsonAsArray,
    JsonTypeof,
    JsonEqual,
    JsonIsArray,
    JsonSet,
    JsonSetIndex,
    JsonKeys,
    JsonHasOwn,
    JsonDelete,
    JsonKey,

    Stdout,
    Stderr,
    EnvGet,
    EnvKeys,
    Arguments,
    MonotonicNow,
    DateNow,
    ExitCodeGet,
    ExitCodeSet,
    ExitCodeClear,
    ProcessExit,
    ProcessExitCurrent,
    BufferNew,
    BufferLength,
    BytesNew,
    BytesView,
    BytesFromArray,
    BytesCopy,
    BytesLength,
    BytesOffset,
    BytesBuffer,
    BytesGet,
    BytesPut,
    BytesSubarray,
    BytesSlice,
    BytesSet,
    BytesSetArray,
}

#[derive(Clone, Copy)]
enum ValueType {
    JsonReplacer,
    JsonReviver,
    Json,
    OptionalJson,
    Jsons,
    OptionalJsons,
    Boolean,
    String,
    OptionalString,
    OptionalNumber,
    Strings,
    Number,
    Numbers,
    Bytes,
    Buffer,
}

impl ValueType {
    fn get(self, ctx: &mut Context) -> TypeHandle {
        match self {
            Self::JsonReplacer => JsonReplacerType::get(ctx).into(),
            Self::JsonReviver => JsonReviverType::get(ctx).into(),
            Self::Json => JsonType::get(ctx).into(),
            Self::OptionalJson => {
                let json = JsonType::get(ctx).into();
                OptionalType::get(ctx, json).into()
            }
            Self::Jsons => {
                let json = JsonType::get(ctx).into();
                ArrayType::get(ctx, json).into()
            }
            Self::OptionalJsons => {
                let json = Self::OptionalJson.get(ctx);
                ArrayType::get(ctx, json).into()
            }
            Self::Boolean => IntegerType::get(ctx, 1, Signedness::Signless).into(),
            Self::Bytes => BytesType::get(ctx).into(),
            Self::Buffer => BufferType::get(ctx).into(),
            Self::Numbers => {
                let number = FP64Type::get(ctx).into();
                ArrayType::get(ctx, number).into()
            }
            Self::Number => FP64Type::get(ctx).into(),
            Self::String => StringType::get(ctx).into(),
            Self::OptionalString | Self::OptionalNumber => {
                let value = if matches!(self, Self::OptionalString) {
                    StringType::get(ctx).into()
                } else {
                    FP64Type::get(ctx).into()
                };
                OptionalType::get(ctx, value).into()
            }
            Self::Strings => {
                let string = StringType::get(ctx).into();
                ArrayType::get(ctx, string).into()
            }
        }
    }
    fn matches(self, ctx: &Context, ty: TypeHandle) -> bool {
        match self {
            Self::JsonReplacer => ty.deref(ctx).is::<JsonReplacerType>(),
            Self::JsonReviver => ty.deref(ctx).is::<JsonReviverType>(),
            Self::Json => ty.deref(ctx).is::<JsonType>(),
            Self::OptionalJson => ty
                .deref(ctx)
                .downcast_ref::<OptionalType>()
                .is_some_and(|ty| ty.value().deref(ctx).is::<JsonType>()),
            Self::Jsons => ty
                .deref(ctx)
                .downcast_ref::<ArrayType>()
                .is_some_and(|ty| ty.element().deref(ctx).is::<JsonType>()),
            Self::OptionalJsons => ty
                .deref(ctx)
                .downcast_ref::<ArrayType>()
                .is_some_and(|ty| Self::OptionalJson.matches(ctx, ty.element())),
            Self::Boolean => ty
                .deref(ctx)
                .downcast_ref::<IntegerType>()
                .is_some_and(|ty| ty.width() == 1),
            Self::Bytes => ty.deref(ctx).is::<BytesType>(),
            Self::Buffer => ty.deref(ctx).is::<BufferType>(),
            Self::Numbers => ty
                .deref(ctx)
                .downcast_ref::<ArrayType>()
                .is_some_and(|ty| ty.element().deref(ctx).is::<FP64Type>()),
            Self::Number => ty.deref(ctx).is::<FP64Type>(),
            Self::String => ty.deref(ctx).is::<StringType>(),
            Self::OptionalString => ty
                .deref(ctx)
                .downcast_ref::<OptionalType>()
                .is_some_and(|ty| ty.value().deref(ctx).is::<StringType>()),
            Self::OptionalNumber => ty
                .deref(ctx)
                .downcast_ref::<OptionalType>()
                .is_some_and(|ty| ty.value().deref(ctx).is::<FP64Type>()),
            Self::Strings => ty
                .deref(ctx)
                .downcast_ref::<ArrayType>()
                .is_some_and(|ty| ty.element().deref(ctx).is::<StringType>()),
        }
    }
}

impl Intrinsic {
    fn signature(self) -> (&'static [ValueType], &'static [ValueType]) {
        use ValueType::*;
        match self {
            Self::JsonParse => (&[String], &[Json]),
            Self::JsonReplacerStart => (&[OptionalJson, OptionalJson], &[JsonReplacer]),
            Self::JsonReplacerNext => (&[JsonReplacer], &[Boolean]),
            Self::JsonReplacerKey => (&[JsonReplacer], &[String]),
            Self::JsonReplacerValue => (&[JsonReplacer], &[OptionalJson]),
            Self::JsonReplacerApply => (&[JsonReplacer, OptionalJson], &[]),
            Self::JsonReplacerResult => (&[JsonReplacer], &[OptionalString]),
            Self::JsonReviverStart => (&[String], &[JsonReviver]),
            Self::JsonReviverContext => (&[JsonReviver], &[Json]),
            Self::JsonReviverNext => (&[JsonReviver], &[Boolean]),
            Self::JsonReviverKey => (&[JsonReviver], &[String]),
            Self::JsonReviverValue | Self::JsonReviverResult => (&[JsonReviver], &[OptionalJson]),
            Self::JsonReviverApply => (&[JsonReviver, OptionalJson], &[]),
            Self::JsonStringify => (&[OptionalJson], &[OptionalString]),
            Self::JsonStringifyOptions => (
                &[OptionalJson, OptionalJson, OptionalJson],
                &[OptionalString],
            ),
            Self::JsonNull => (&[], &[Json]),
            Self::JsonNumber => (&[Number], &[Json]),
            Self::JsonBoolean => (&[Boolean], &[Json]),
            Self::JsonString => (&[String], &[Json]),
            Self::JsonArray => (&[OptionalJsons], &[Json]),
            Self::JsonObject => (&[Strings, OptionalJsons], &[Json]),
            Self::JsonGet => (&[Json, String], &[OptionalJson]),
            Self::JsonIndex => (&[Json, Number], &[OptionalJson]),
            Self::JsonAsArray => (&[OptionalJson], &[Jsons]),
            Self::JsonAsNumber => (&[OptionalJson], &[Number]),
            Self::JsonAsBoolean => (&[OptionalJson], &[Boolean]),
            Self::JsonAsString | Self::JsonTypeof => (&[OptionalJson], &[String]),
            Self::JsonIsArray => (&[OptionalJson], &[Boolean]),
            Self::JsonEqual => (&[OptionalJson, OptionalJson], &[Boolean]),
            Self::JsonSet => (&[Json, String, OptionalJson], &[]),
            Self::JsonSetIndex => (&[Json, Number, OptionalJson], &[]),
            Self::JsonKeys => (&[OptionalJson], &[Strings]),
            Self::JsonHasOwn => (&[OptionalJson, String], &[Boolean]),
            Self::JsonDelete => (&[Json, String], &[Boolean]),
            Self::JsonKey => (&[OptionalJson], &[String]),
            Self::BufferNew => (&[Number], &[Buffer]),
            Self::BufferLength => (&[Buffer], &[Number]),
            Self::BytesNew => (&[Number], &[Bytes]),
            Self::BytesView => (&[Buffer, Number, OptionalNumber], &[Bytes]),
            Self::BytesFromArray => (&[Numbers], &[Bytes]),
            Self::BytesCopy => (&[Bytes], &[Bytes]),
            Self::BytesLength | Self::BytesOffset => (&[Bytes], &[Number]),
            Self::BytesBuffer => (&[Bytes], &[Buffer]),
            Self::BytesGet => (&[Bytes, Number], &[OptionalNumber]),
            Self::BytesPut => (&[Bytes, Number, Number], &[Number]),
            Self::BytesSubarray | Self::BytesSlice => (&[Bytes, Number, OptionalNumber], &[Bytes]),
            Self::BytesSet => (&[Bytes, Bytes, Number], &[]),
            Self::BytesSetArray => (&[Bytes, Numbers, Number], &[]),
            Self::ExitCodeGet => (&[], &[OptionalNumber]),
            Self::ExitCodeSet => (&[Number], &[Number]),
            Self::ExitCodeClear | Self::ProcessExitCurrent => (&[], &[]),
            Self::ProcessExit => (&[Number], &[]),
            Self::Stdout | Self::Stderr => (&[String], &[]),
            Self::EnvGet => (&[String], &[OptionalString]),
            Self::EnvKeys | Self::Arguments => (&[], &[Strings]),
            Self::MonotonicNow | Self::DateNow => (&[], &[Number]),
        }
    }
}

#[derive(Debug, Error)]
#[error("intrinsic {0:?} has incorrect operand or result types")]
struct SignatureError(Intrinsic);

#[pliron_op(
    name = "perry.intrinsic",
    format = "attr($perry_intrinsic, $Intrinsic) ` (` operands(CharSpace(`,`)) `) : ` types(CharSpace(`,`))",
    attributes = (perry_intrinsic: Intrinsic)
)]
pub struct IntrinsicOp;

impl IntrinsicOp {
    pub fn new(ctx: &mut Context, kind: Intrinsic, args: Vec<Value>) -> Self {
        let results = kind.signature().1.iter().map(|ty| ty.get(ctx)).collect();
        let op = Self {
            op: Operation::new(ctx, Self::get_concrete_op_info(), results, args, vec![], 0),
        };
        op.set_attr_perry_intrinsic(ctx, kind);
        op
    }
    pub fn kind(&self, ctx: &Context) -> Intrinsic {
        *self.get_attr_perry_intrinsic(ctx).expect("intrinsic kind")
    }
}

impl Verify for IntrinsicOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let kind = self.kind(ctx);
        let (params, results) = kind.signature();
        let op = self.get_operation().deref(ctx);
        if op.get_num_operands() != params.len()
            || op.get_num_results() != results.len()
            || !params
                .iter()
                .zip(op.operands())
                .all(|(ty, v)| ty.matches(ctx, v.get_type(ctx)))
            || !results
                .iter()
                .zip(op.results())
                .all(|(ty, v)| ty.matches(ctx, v.get_type(ctx)))
        {
            verify_err!(self.loc(ctx), SignatureError(kind))?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for IntrinsicOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        true
    }
}

#[op_interface_impl]
impl crate::exceptions::MayThrowInterface for IntrinsicOp {
    fn exception(&self, ctx: &Context) -> crate::attrs::ErrorKind {
        if matches!(
            self.kind(ctx),
            Intrinsic::BytesFromArray
                | Intrinsic::BytesSetArray
                | Intrinsic::JsonReplacerNext
                | Intrinsic::JsonReplacerApply
                | Intrinsic::JsonReviverStart
                | Intrinsic::JsonReviverNext
                | Intrinsic::JsonReviverApply
                | Intrinsic::JsonParse
                | Intrinsic::JsonObject
                | Intrinsic::JsonStringify
                | Intrinsic::JsonStringifyOptions
                | Intrinsic::JsonGet
                | Intrinsic::JsonIndex
                | Intrinsic::JsonAsNumber
                | Intrinsic::JsonAsBoolean
                | Intrinsic::JsonAsArray
                | Intrinsic::JsonAsString
                | Intrinsic::JsonSet
                | Intrinsic::JsonSetIndex
                | Intrinsic::JsonKeys
                | Intrinsic::JsonHasOwn
                | Intrinsic::JsonDelete
                | Intrinsic::JsonKey
        ) {
            crate::attrs::ErrorKind::Any
        } else {
            crate::attrs::ErrorKind::RangeError
        }
    }
    fn may_throw(&self, ctx: &Context) -> bool {
        matches!(
            self.kind(ctx),
            Intrinsic::BytesFromArray
                | Intrinsic::BytesSetArray
                | Intrinsic::JsonReplacerNext
                | Intrinsic::JsonReplacerApply
                | Intrinsic::JsonReviverStart
                | Intrinsic::JsonReviverNext
                | Intrinsic::JsonReviverApply
                | Intrinsic::JsonParse
                | Intrinsic::JsonObject
                | Intrinsic::JsonStringify
                | Intrinsic::JsonStringifyOptions
                | Intrinsic::JsonGet
                | Intrinsic::JsonIndex
                | Intrinsic::JsonAsNumber
                | Intrinsic::JsonAsBoolean
                | Intrinsic::JsonAsArray
                | Intrinsic::JsonAsString
                | Intrinsic::JsonSet
                | Intrinsic::JsonSetIndex
                | Intrinsic::JsonKeys
                | Intrinsic::JsonHasOwn
                | Intrinsic::JsonDelete
                | Intrinsic::JsonKey
                | Intrinsic::ExitCodeSet
                | Intrinsic::ProcessExit
                | Intrinsic::BufferNew
                | Intrinsic::BytesNew
                | Intrinsic::BytesView
                | Intrinsic::BytesSet
        )
    }
}
