//! Canonical result types and their accepted IR types, shared by signatures and projections.
use crate::types::{
    AbortType, ArrayType, BytesType, DateType, ExceptionType, HeadersType, IncomingType,
    InstantType, JsonType, OptionalType, PlainDateTimeType, PlainDateType, PromiseType,
    ReadResultType, ReadableType, ReaderType, RequestType, ResponseType, StatsType, StringType,
    WritableType, WriterType,
};
use pliron::{
    builtin::types::{FP64Type, IntegerType, Signedness},
    context::Context,
    r#type::{TypeHandle, Typed, TypedHandle},
};

pub(super) trait ResultType: Copy {
    fn get(self, ctx: &Context) -> TypeHandle;
    fn matches(self, ctx: &Context, ty: TypeHandle) -> bool;
}

impl ResultType for TypeHandle {
    fn get(self, _: &Context) -> TypeHandle {
        self
    }

    fn matches(self, _: &Context, ty: TypeHandle) -> bool {
        self == ty
    }
}

macro_rules! type_kinds {
    ($ctx:ident, $ty:ident;
        $($(#[$meta:meta])* $kind:ident = $canonical:expr $(, accepts $accepts:expr)?;)*
    ) => {
        /// Operand and result kinds of intrinsic signatures.
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub(super) enum Ty { $($(#[$meta])* $kind),* }

        impl ResultType for Ty {
            fn get(self, $ctx: &Context) -> TypeHandle {
                match self { $(Ty::$kind => $canonical),* }
            }
            /// Whether `ty` is the canonical type or an explicitly accepted alternative:
            /// indices and intrinsic booleans admit any signedness, readers include BYOB,
            /// and optional numbers and bytes (including `Promise<void>`) admit any absence.
            fn matches(self, $ctx: &Context, $ty: TypeHandle) -> bool {
                match self {
                    $(Ty::$kind => type_kinds!(@accept self, $ctx, $ty $(, $accepts)?)),*
                }
            }
        }
    };
    (@accept $kind:ident, $ctx:ident, $ty:ident, $accepts:expr) => { $accepts };
    (@accept $kind:ident, $ctx:ident, $ty:ident) => { $ty == $kind.get($ctx) };
}

type_kinds! {
    ctx, ty;
    BigInt = crate::types::BigIntType::get(ctx).into();
    /// A signless i32 index proven in bounds; verification admits any signedness.
    Index = IntegerType::get(ctx, 32, Signedness::Signless).into(),
        accepts integer_width(ctx, ty) == Some(32);
    /// Unicode scalar value, or -1 for the empty result of charAt. Internal MIR only.
    Scalar = Ty::Index.get(ctx),
        accepts Ty::Index.matches(ctx, ty);
    Number = FP64Type::get(ctx).into();
    Numeric = Ty::Number.get(ctx),
        accepts ty.deref(ctx).is::<crate::types::NumericType>()
            || Ty::NumberOrInteger.matches(ctx, ty);
    NumberOrInteger = Ty::Number.get(ctx),
        accepts ty.deref(ctx).is::<IntegerType>() || Ty::Number.matches(ctx, ty);
    Bool = Ty::Predicate.get(ctx),
        accepts integer_width(ctx, ty) == Some(1);
    /// Strict boolean predicate; unlike intrinsic Bool, signedness is significant.
    Predicate = IntegerType::get(ctx, 1, Signedness::Signless).into();
    /// Exception status; unlike an Index, it must be signless.
    Status = Ty::Index.get(ctx);
    String = StringType::get(ctx).into();
    OptString = OptionalType::get(ctx, Ty::String.get(ctx)).into();
    NullString = OptionalType::get_with(ctx, Ty::String.get(ctx), crate::attrs::Absence::Null).into();
    OptNumber = OptionalType::get(ctx, Ty::Number.get(ctx)).into(),
        accepts optional_of(ctx, ty, Ty::Number);
    StringArray = ArrayType::get(ctx, Ty::String.get(ctx)).into();
    NullStringArray = OptionalType::get_with(
        ctx, Ty::StringArray.get(ctx), crate::attrs::Absence::Null
    ).into();
    NumberArray = ArrayType::get(ctx, Ty::Number.get(ctx)).into();
    Bytes = BytesType::get(ctx).into();
    ByteBuffer = crate::types::ByteBufferType::get(ctx).into();
    ByobReader = crate::types::ByobReaderType::get(ctx).into();
    ByteInput = crate::types::ByteInputType::get(ctx).into();
    Url = crate::types::UrlType::get(ctx).into();
    SearchParams = crate::types::SearchParamsType::get(ctx).into();
    Json = JsonType::get(ctx).into();
    Date = DateType::get(ctx).into();
    Decoder = crate::types::DecoderType::get(ctx).into();
    Regex = crate::types::RegexType::get(ctx).into();
    Exception = ExceptionType::get(ctx).into();
    Stats = StatsType::get(ctx).into();
    PromiseString = PromiseType::get(ctx, Ty::String.get(ctx)).into();
    PromiseBytes = PromiseType::get(ctx, Ty::Bytes.get(ctx)).into();
    PromiseJson = PromiseType::get(ctx, Ty::Json.get(ctx)).into();
    PromiseStats = PromiseType::get(ctx, Ty::Stats.get(ctx)).into();
    PromiseStringArray = PromiseType::get(ctx, Ty::StringArray.get(ctx)).into();
    /// `Promise<void>`: settles `undefined`, admitting any optional-number absence.
    PromiseVoid = PromiseType::get(ctx, Ty::OptNumber.get(ctx)).into(),
        accepts TypedHandle::<PromiseType>::from_handle(ty, ctx)
            .is_ok_and(|promise| optional_of(ctx, promise.deref(ctx).value(), Ty::Number));
    OptBytes = OptionalType::get(ctx, Ty::Bytes.get(ctx)).into(),
        accepts optional_of(ctx, ty, Ty::Bytes);
    Headers = HeadersType::get(ctx).into();
    Response = ResponseType::get(ctx).into();
    Request = RequestType::get(ctx).into();
    Writable = WritableType::get(ctx).into();
    Writer = WriterType::get(ctx).into();
    Readable = ReadableType::get(ctx).into();
    Reader = ReaderType::get(ctx).into(),
        accepts ty == Ty::Reader.get(ctx) || ty == Ty::ByobReader.get(ctx);
    ReadResult = ReadResultType::get(ctx).into();
    PromiseReadResult = PromiseType::get(ctx, Ty::ReadResult.get(ctx)).into();
    Incoming = IncomingType::get(ctx).into();
    Abort = AbortType::get(ctx).into();
    Instant = InstantType::get(ctx).into();
    PlainDate = PlainDateType::get(ctx).into();
    PlainDateTime = PlainDateTimeType::get(ctx).into();
    NullReadable = OptionalType::get_with(ctx, Ty::Readable.get(ctx), crate::attrs::Absence::Null).into();
    PromiseResponse = PromiseType::get(ctx, Ty::Response.get(ctx)).into();
}

fn integer_width(ctx: &Context, ty: TypeHandle) -> Option<u32> {
    ty.deref(ctx)
        .downcast_ref::<IntegerType>()
        .map(|integer| integer.width())
}

fn optional_of(ctx: &Context, ty: TypeHandle, value: Ty) -> bool {
    crate::optional::optional_value(ctx, ty) == Some(value.get(ctx))
}

#[derive(Debug, thiserror::Error)]
#[error("record projection is missing its field name")]
pub(super) struct MissingProjectionField;

pub(super) trait TypePattern: Copy {
    fn accepts_type(self, ctx: &Context, ty: TypeHandle) -> bool;
}

impl<T: ResultType> TypePattern for T {
    fn accepts_type(self, ctx: &Context, ty: TypeHandle) -> bool {
        self.matches(ctx, ty)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TypeMismatch {
    Count,
    Type(usize),
}

pub(super) fn type_mismatch<T: TypePattern>(
    ctx: &Context,
    expected: impl ExactSizeIterator<Item = T>,
    actual: impl ExactSizeIterator<Item = TypeHandle>,
) -> Option<TypeMismatch> {
    if expected.len() != actual.len() {
        Some(TypeMismatch::Count)
    } else {
        expected
            .zip(actual)
            .position(|(e, a)| !e.accepts_type(ctx, a))
            .map(TypeMismatch::Type)
    }
}

pub(super) fn operand_types<'a>(
    ctx: &'a Context,
    op: &'a pliron::operation::Operation,
) -> impl ExactSizeIterator<Item = TypeHandle> + 'a {
    (0..op.get_num_operands()).map(|i| op.get_operand(i).get_type(ctx))
}

pub(super) fn result_types<'a>(
    ctx: &'a Context,
    op: &'a pliron::operation::Operation,
) -> impl ExactSizeIterator<Item = TypeHandle> + 'a {
    (0..op.get_num_results()).map(|i| op.get_result(i).get_type(ctx))
}

pub(super) struct Signature<I, O> {
    pub inputs: I,
    pub outputs: O,
}
impl<I, O> OutputContract for Signature<I, O>
where
    I: IntoIterator<Item: TypePattern, IntoIter: ExactSizeIterator>,
    O: IntoIterator<Item: TypePattern, IntoIter: ExactSizeIterator>,
{
    fn accepts(self, ctx: &Context, op: &pliron::operation::Operation) -> bool {
        type_mismatch(ctx, self.inputs.into_iter(), operand_types(ctx, op)).is_none()
            && type_mismatch(ctx, self.outputs.into_iter(), result_types(ctx, op)).is_none()
    }
}

pub(super) trait OutputContract {
    fn accepts(self, ctx: &Context, op: &pliron::operation::Operation) -> bool;
}

impl<T: ResultType> OutputContract for T {
    fn accepts(self, ctx: &Context, op: &pliron::operation::Operation) -> bool {
        op.get_num_results() == 1 && self.matches(ctx, op.get_result(0).get_type(ctx))
    }
}

impl OutputContract for () {
    fn accepts(self, _: &Context, op: &pliron::operation::Operation) -> bool {
        op.get_num_results() == 0
    }
}

pub(super) struct Accepts<F>(pub F);
impl<F: FnOnce(&Context, TypeHandle) -> bool> OutputContract for Accepts<F> {
    fn accepts(self, ctx: &Context, op: &pliron::operation::Operation) -> bool {
        op.get_num_results() == 1 && self.0(ctx, op.get_result(0).get_type(ctx))
    }
}

#[derive(Clone, Copy)]
pub(super) struct OptionalPayload(pub TypeHandle);
impl ResultType for OptionalPayload {
    fn get(self, ctx: &Context) -> TypeHandle {
        OptionalType::get(ctx, self.0).into()
    }
    fn matches(self, ctx: &Context, ty: TypeHandle) -> bool {
        crate::optional::optional_value(ctx, ty) == Some(self.0)
    }
}
