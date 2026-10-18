//! Host and runtime services with fixed signatures (`perry.intrinsic`).
//!
//! The builder recognizes the standard API (`console.log`, `Math.floor`, string methods,
//! `Uint8Array`, `JSON`, `Date`, …) and emits one op per service, named by [Intrinsic];
//! codegen maps each to Wasm instructions or a runtime function and checks that the WIT world
//! imports what it needs. Some services throw ([Intrinsic::throws]); they implement
//! [MayThrowInterface] through [may_throw].
use crate::{
    NewOp,
    attrs::ErrorKind,
    exceptions::MayThrowInterface,
    types::{
        AbortType, ArrayType, BytesType, DateType, ExceptionType, HeadersType, IncomingType,
        InstantType, JsonType, OptionalType, PlainDateTimeType, PlainDateType, PromiseType,
        ReadResultType, ReadableType, ReaderType, RequestType, ResponseType, StatsType, StringType,
        WritableType, WriterType,
    },
};
use pliron::{
    builtin::{
        attributes::TypeAttr,
        types::{FP64Type, IntegerType, Signedness},
    },
    common_traits::Verify,
    context::{Context, Ptr},
    derive::{op_interface_impl, pliron_attr, pliron_op},
    op::{Op, op_cast},
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
    verify_err,
};
use thiserror::Error;

mod promises;
pub use promises::{
    Combinator, PromiseCombinatorOp, PromiseRejectedOp, PromiseResolvedOp, SettledField,
    SettledFieldOp,
};

/// Operand and result kinds of intrinsic signatures.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ty {
    BigInt,
    /// A signless i32 index proven in bounds.
    Index,
    /// Unicode scalar value, or -1 for the empty result of charAt. Internal MIR only.
    Scalar,
    I64,
    U64,
    Number,
    Bool,
    String,
    OptString,
    NullString,
    OptNumber,
    StringArray,
    NullStringArray,
    NumberArray,
    Bytes,
    ByteBuffer,
    ByobReader,
    ByteInput,
    Url,
    SearchParams,
    Json,
    JsonCursor,
    Date,
    Decoder,
    Regex,
    Exception,
    Stats,
    PromiseString,
    PromiseBytes,
    PromiseJson,
    PromiseStats,
    PromiseStringArray,
    /// `Promise<void>`: settles `undefined`.
    PromiseVoid,
    OptBytes,
    Headers,
    Response,
    Request,
    Writable,
    Writer,
    Readable,
    Reader,
    ReadResult,
    PromiseReadResult,
    Incoming,
    Abort,
    Instant,
    PlainDate,
    PlainDateTime,
    NullReadable,
    PromiseResponse,
}

impl Ty {
    fn get(self, ctx: &Context) -> TypeHandle {
        let optional = |value: Ty| OptionalType::get(ctx, value.get(ctx)).into();
        let nullable = |value: Ty| {
            OptionalType::get_with(ctx, value.get(ctx), crate::attrs::Absence::Null).into()
        };
        let array = |element: Ty| ArrayType::get(ctx, element.get(ctx)).into();
        let promise = |value: Ty| PromiseType::get(ctx, value.get(ctx)).into();
        match self {
            Ty::BigInt => crate::types::BigIntType::get(ctx).into(),
            Ty::Index | Ty::Scalar => crate::collections::index_type(ctx),
            Ty::I64 => IntegerType::get(ctx, 64, Signedness::Signed).into(),
            Ty::U64 => IntegerType::get(ctx, 64, Signedness::Unsigned).into(),
            Ty::Number => FP64Type::get(ctx).into(),
            Ty::Bool => IntegerType::get(ctx, 1, Signedness::Signless).into(),
            Ty::String => StringType::get(ctx).into(),
            Ty::OptString => optional(Ty::String),
            Ty::NullString => nullable(Ty::String),
            Ty::OptNumber => optional(Ty::Number),
            Ty::StringArray => array(Ty::String),
            Ty::NullStringArray => nullable(Ty::StringArray),
            Ty::NumberArray => array(Ty::Number),
            Ty::Bytes => BytesType::get(ctx).into(),
            Ty::ByteBuffer => crate::types::ByteBufferType::get(ctx).into(),
            Ty::ByobReader => crate::types::ByobReaderType::get(ctx).into(),
            Ty::ByteInput => crate::types::ByteInputType::get(ctx).into(),
            Ty::Url => crate::types::UrlType::get(ctx).into(),
            Ty::SearchParams => crate::types::SearchParamsType::get(ctx).into(),
            Ty::Json => JsonType::get(ctx).into(),
            Ty::JsonCursor => crate::types::JsonCursorType::get(ctx).into(),
            Ty::Regex => crate::types::RegexType::get(ctx).into(),
            Ty::Decoder => crate::types::DecoderType::get(ctx).into(),
            Ty::Date => DateType::get(ctx).into(),
            Ty::Exception => ExceptionType::get(ctx).into(),
            Ty::Stats => StatsType::get(ctx).into(),
            Ty::OptBytes => optional(Ty::Bytes),
            Ty::Headers => HeadersType::get(ctx).into(),
            Ty::Response => ResponseType::get(ctx).into(),
            Ty::Request => RequestType::get(ctx).into(),
            Ty::Writable => WritableType::get(ctx).into(),
            Ty::Writer => WriterType::get(ctx).into(),
            Ty::Readable => ReadableType::get(ctx).into(),
            Ty::Reader => ReaderType::get(ctx).into(),
            Ty::ReadResult => ReadResultType::get(ctx).into(),
            Ty::Incoming => IncomingType::get(ctx).into(),
            Ty::Abort => AbortType::get(ctx).into(),
            Ty::Instant => InstantType::get(ctx).into(),
            Ty::PlainDate => PlainDateType::get(ctx).into(),
            Ty::PlainDateTime => PlainDateTimeType::get(ctx).into(),
            Ty::PromiseReadResult => promise(Ty::ReadResult),
            Ty::NullReadable => nullable(Ty::Readable),
            Ty::PromiseResponse => promise(Ty::Response),
            Ty::PromiseString => promise(Ty::String),
            Ty::PromiseBytes => promise(Ty::Bytes),
            Ty::PromiseJson => promise(Ty::Json),
            Ty::PromiseStats => promise(Ty::Stats),
            Ty::PromiseStringArray => promise(Ty::StringArray),
            Ty::PromiseVoid => promise(Ty::OptNumber),
        }
    }

    /// Whether `ty` is this kind: the type [Ty::get] builds, except that indices and booleans
    /// admit any signedness, readers include BYOB readers, and optional numbers and bytes
    /// (also the `undefined` a `Promise<void>` settles) admit any absence.
    fn matches(self, ctx: &Context, ty: TypeHandle) -> bool {
        let width = || {
            let ty = ty.deref(ctx);
            ty.downcast_ref::<IntegerType>()
                .map(|integer| integer.width())
        };
        let optional_of = |ty: TypeHandle, value: Ty| {
            crate::optional::optional_value(ctx, ty) == Some(value.get(ctx))
        };
        match self {
            Ty::Index | Ty::Scalar => width() == Some(32),
            Ty::Bool => width() == Some(1),
            Ty::Reader => ty == self.get(ctx) || ty == Ty::ByobReader.get(ctx),
            Ty::OptNumber => optional_of(ty, Ty::Number),
            Ty::OptBytes => optional_of(ty, Ty::Bytes),
            Ty::PromiseVoid => TypedHandle::<PromiseType>::from_handle(ty, ctx)
                .is_ok_and(|promise| optional_of(promise.deref(ctx).value(), Ty::Number)),
            _ => ty == self.get(ctx),
        }
    }
}

/// Operand kinds: a value kind, or an aggregate kind that names no single MIR type and so
/// cannot be a result ([Ty] has no such kind).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Operand {
    Value(Ty),
    /// Records and tagged unions retaining their original JSON graph.
    Aggregate,
    Reference,
}

impl Operand {
    fn matches(self, ctx: &Context, ty: TypeHandle) -> bool {
        match self {
            Operand::Value(kind) => kind.matches(ctx, ty),
            Operand::Reference => crate::types::is_reference(ctx, ty),
            Operand::Aggregate => {
                let deref = ty.deref(ctx);
                deref.is::<crate::types::RecordType>() || deref.is::<crate::types::VariantType>()
            }
        }
    }
}

/// The [Operand] an `intrinsics!` operand name denotes.
macro_rules! operand {
    (Reference) => {
        Operand::Reference
    };
    (Aggregate) => {
        Operand::Aggregate
    };
    ($kind:ident) => {
        Operand::Value(Ty::$kind)
    };
}

/// The [Flag] an `intrinsics!` flag name denotes; an unknown name does not compile.
macro_rules! flag {
    (throws) => {
        Flag::Throws
    };
    (pure) => {
        Flag::Pure
    };
    (atomic) => {
        Flag::Atomic
    };
    (noalloc) => {
        Flag::NoAlloc
    };
}

/// The optional [BuiltinContract] of an `intrinsics!` entry.
macro_rules! contract {
    () => {
        None
    };
    ($contract:ident) => {
        Some(BuiltinContract::$contract)
    };
}

/// Contract flags of a service; see the [Intrinsic] queries that read them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Flag {
    Throws,
    Pure,
    Atomic,
    NoAlloc,
}

/// Each entry: `Variant = "name" (operands) -> (results), flags… [=> numeric contract];`.
macro_rules! intrinsics {
    ($($variant:ident = $name:literal ($($operand:ident),*) -> ($($result:ident),*) $(, $flag:ident)* $(=> $contract:ident)?;)*) => {
        /// The services; see each name for the API it implements.
        #[pliron_attr(name = "perry.intrinsic_kind", format, verifier = "succ")]
        #[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
        pub enum Intrinsic {
            $($variant,)*
        }

        impl Intrinsic {
            pub fn name(self) -> &'static str {
                match self {
                    $(Intrinsic::$variant => $name,)*
                }
            }

            /// (operands, results).
            fn types(self) -> (&'static [Operand], &'static [Ty]) {
                match self {
                    $(Intrinsic::$variant => (&[$(operand!($operand)),*], &[$(Ty::$result),*]),)*
                }
            }

            fn flags(self) -> &'static [Flag] {
                match self {
                    $(Intrinsic::$variant => &[$(flag!($flag)),*],)*
                }
            }

            /// Semantic numeric contract used by `facts` for range and integer domain propagation.
            pub(crate) fn numeric_contract(self) -> Option<BuiltinContract> {
                match self {
                    $(Intrinsic::$variant => contract!($($contract)?),)*
                }
            }
        }
    };
}

intrinsics! {
    BigIntFromNumber = "bigint_from_number" (Number) -> (BigInt), throws, atomic;
    BigIntNegate = "bigint_negate" (BigInt) -> (BigInt), pure, atomic;
    BigIntToNumber = "bigint_to_number" (BigInt) -> (Number), pure, noalloc;
    BigIntToString = "bigint_to_string" (BigInt) -> (String), pure, atomic;
    PrintOut = "print_out" (String) -> ();
    PrintErr = "print_err" (String) -> ();
    NumberToString = "number_to_string" (Number) -> (String), pure, atomic => NumberOnly;
    NumberInspect = "number_inspect" (Number) -> (String), pure, atomic => NumberOnly;
    NumberToStringRadix = "number_to_string_radix" (Number, Number) -> (String), throws, atomic => NumberOnly;
    EnvGet = "env_get" (String) -> (OptString);
    EnvKeys = "env_keys" () -> (StringArray);
    PerformanceNow = "performance_now" () -> (Number), noalloc;
    DateNow = "date_now" () -> (Number), noalloc;
    ExitCodeSet = "exit_code_set" (Number) -> (), noalloc;
    MathFloor = "math_floor" (Number) -> (Number), pure, noalloc => Round;
    MathCeil = "math_ceil" (Number) -> (Number), pure, noalloc => Round;
    MathTrunc = "math_trunc" (Number) -> (Number), pure, noalloc => Round;
    MathAbs = "math_abs" (Number) -> (Number), pure, noalloc => Abs;
    MathRound = "math_round" (Number) -> (Number), pure, noalloc => Round;
    MathSqrt = "math_sqrt" (Number) -> (Number), pure, noalloc => NumberOnly;
    MathMin = "math_min" (Number, Number) -> (Number), pure, noalloc => Min;
    MathMax = "math_max" (Number, Number) -> (Number), pure, noalloc => Max;
    ParseInt = "parse_int" (String, Number) -> (Number), pure, noalloc => NumberOnly;
    ParseFloat = "parse_float" (String) -> (Number), pure, noalloc => NumberOnly;
    StringToNumber = "string_to_number" (String) -> (Number), pure, noalloc => NumberOnly;
    NumberIsNaN = "number_is_nan" (Number) -> (Bool), pure, noalloc => Predicate;
    NumberIsFinite = "number_is_finite" (Number) -> (Bool), pure, noalloc => Predicate;
    NumberIsInteger = "number_is_integer" (Number) -> (Bool), pure, noalloc => Predicate;
    NumberIsSafeInteger = "number_is_safe_integer" (Number) -> (Bool), pure, noalloc => Predicate;
    DateFromTime = "date_from_time" (Number) -> (Date);
    DateTime = "date_time" (Date) -> (Number), noalloc;
    DateParse = "date_parse" (String) -> (Number), pure, noalloc;
    DateUtc = "date_utc" (Number, Number, Number, Number, Number, Number, Number) -> (Number), pure, noalloc;
    DatePart = "date_part" (Date, Number) -> (Number), noalloc;
    DateSet = "date_set" (Date, Number, Number, Number, Number, Number, Number) -> (Number), noalloc;
    DateIso = "date_iso" (Date) -> (String), throws;
    DateJson = "date_json" (Date) -> (NullString);
    StringFromCodePoint = "string_from_code_point" (Number) -> (String), throws, atomic;
    StringCharAt = "string_char_at" (String, Number) -> (String), pure, atomic;
    StringCharAtIndex = "string_char_at_index" (String, Index) -> (String), pure, atomic;
    StringScalarAtIndex = "string_scalar_at_index" (String, Index) -> (Scalar), pure, noalloc;
    StringScalarAt = "string_scalar_at" (String, Number) -> (Scalar), pure, noalloc;
    StringScalarLength = "string_scalar_length" (Scalar) -> (Index), pure, noalloc;
    StringScalarConcat = "string_scalar_concat" (String, Scalar) -> (String), pure, atomic;
    StringTryAppend = "string_try_append" (String, String) -> (Bool), noalloc;
    StringScalarTryAppend = "string_scalar_try_append" (String, Scalar) -> (Bool), noalloc;
    StringScalarAppend = "string_scalar_append" (String, Scalar) -> (String), atomic;
    StringCodePointAt = "string_code_point_at" (String, Number) -> (OptNumber), pure, noalloc;
    StringSlice = "string_slice" (String, Number, Number) -> (String), pure, atomic;
    StringSubstring = "string_substring" (String, Number, Number) -> (String), pure, atomic;
    StringIndexOf = "string_index_of" (String, String, Number) -> (Number), pure, noalloc;
    StringLastIndexOf = "string_last_index_of" (String, String, Number) -> (Number), pure, noalloc;
    StringStartsWith = "string_starts_with" (String, String, Number) -> (Bool), pure, noalloc;
    StringEndsWith = "string_ends_with" (String, String, Number) -> (Bool), pure, noalloc;
    StringTrim = "string_trim" (String) -> (String), pure, atomic;
    StringTrimStart = "string_trim_start" (String) -> (String), pure, atomic;
    StringTrimEnd = "string_trim_end" (String) -> (String), pure, atomic;
    StringPadStart = "string_pad_start" (String, Number, String) -> (String), pure, atomic;
    StringPadEnd = "string_pad_end" (String, Number, String) -> (String), pure, atomic;
    StringReplace = "string_replace" (String, String, String) -> (String), pure, atomic;
    StringReplaceAll = "string_replace_all" (String, String, String) -> (String), pure, atomic;
    StringAt = "string_at" (String, Number) -> (OptString), pure, atomic;
    StringSplit = "string_split" (String, String) -> (StringArray), pure, atomic;
    StringIndex = "string_index" (String, Number) -> (OptString), pure, atomic;
    StringLower = "string_lower" (String) -> (String), pure, atomic;
    StringUpper = "string_upper" (String) -> (String), pure, atomic;
    StringRepeat = "string_repeat" (String, Number) -> (String), throws, atomic;
    StringNormalize = "string_normalize" (String, String) -> (String), throws, atomic;
    BytesNew = "bytes_new" (Number) -> (Bytes), throws;
    BytesFromArray = "bytes_from_array" (NumberArray) -> (Bytes);
    BytesCopy = "bytes_copy" (Bytes) -> (Bytes), throws;
    BytesBuffer = "bytes_buffer" (Bytes) -> (ByteBuffer), pure, noalloc;
    BytesOffset = "bytes_offset" (Bytes) -> (Number), noalloc => UnsignedCount;
    BufferLength = "buffer_length" (ByteBuffer) -> (Number), noalloc => UnsignedCount;
    BytesFromBuffer = "bytes_from_buffer" (ByteBuffer) -> (Bytes), throws;
    BytesAt = "bytes_at" (Bytes, Index) -> (Number), pure, noalloc => ByteValue;
    BytesGet = "bytes_get" (Bytes, Number) -> (OptNumber), noalloc => ByteValue;
    BytesSet = "bytes_set" (Bytes, Number, Number) -> (), noalloc;
    BytesLength = "bytes_length" (Bytes) -> (Number), noalloc => UnsignedCount;
    BytesSubarray = "bytes_subarray" (Bytes, Number, Number) -> (Bytes), throws;
    BytesSlice = "bytes_slice" (Bytes, Number, Number) -> (Bytes), throws;
    BytesSetFrom = "bytes_set_from" (Bytes, Bytes, Number) -> (), throws;
    BytesFill = "bytes_fill" (Bytes, Number, Number, Number) -> (), throws;
    TextEncode = "text_encode" (String) -> (Bytes);
    StringByteInput = "string_byte_input" (String) -> (ByteInput);
    BytesByteInput = "bytes_byte_input" (Bytes) -> (ByteInput), noalloc;
    UrlNew = "url_new" (String, OptString) -> (Url), throws;
    UrlGet = "url_get" (Url, Number) -> (String);
    UrlSetPathname = "url_set_pathname" (Url, String) -> ();
    UrlSearchParams = "url_search_params" (Url) -> (SearchParams), pure, noalloc;
    UrlAppend = "url_append" (SearchParams, String, String) -> ();
    EncodeUriComponent = "encode_uri_component" (String) -> (String);
    DecoderFatal = "decoder_fatal" (Decoder) -> (Bool), pure, noalloc;
    DecoderIgnoreBom = "decoder_ignore_bom" (Decoder) -> (Bool), pure, noalloc;
    RegexSearch = "regex_search" (String, Regex) -> (Number), pure, noalloc;
    RegexExec = "regex_exec" (String, Regex) -> (NullStringArray), pure, atomic;
    RegexMatch = "regex_match" (String, Regex) -> (NullStringArray), pure, atomic;
    RegexReplace = "regex_replace" (String, Regex, String) -> (String), pure, atomic;
    RegexSplit = "regex_split" (String, Regex) -> (StringArray), pure, atomic;
    DecoderValidateLabel = "decoder_validate_label" (String) -> (), throws, atomic;
    DecoderNew = "decoder_new" (Bool, Bool) -> (Decoder);
    TextDecode = "text_decode" (Decoder, Bytes, Bool) -> (String), throws;
    JsonParse = "json_parse" (String) -> (Json), throws, atomic;
    JsonCursorBegin = "json_cursor_begin" (String) -> (JsonCursor), atomic;
    JsonCursorByte = "json_cursor_byte" (JsonCursor) -> (Index), noalloc;
    JsonCursorConsume = "json_cursor_consume" (JsonCursor, Index) -> (), noalloc;
    JsonCursorNumber = "json_cursor_number" (JsonCursor) -> (Number), noalloc;
    JsonCursorI64 = "json_cursor_i64" (JsonCursor) -> (I64), throws, noalloc;
    JsonCursorU64 = "json_cursor_u64" (JsonCursor) -> (U64), throws, noalloc;
    JsonCursorFailed = "json_cursor_failed" (JsonCursor) -> (Bool), noalloc;
    JsonCursorBoolean = "json_cursor_boolean" (JsonCursor) -> (Bool), noalloc;
    JsonCursorString = "json_cursor_string" (JsonCursor) -> (String), atomic;
    JsonCursorDiscard = "json_cursor_discard" (JsonCursor) -> (), noalloc;
    JsonCursorValue = "json_cursor_value" (JsonCursor) -> (Json), atomic;
    JsonCursorMember = "json_cursor_member" (JsonCursor, Json) -> (Json), atomic;
    JsonCursorExtra = "json_cursor_extra" (JsonCursor) -> (), atomic;
    JsonCursorRecord = "json_cursor_record" (JsonCursor, Aggregate) -> (), noalloc;
    JsonCursorTail = "json_cursor_tail" (JsonCursor, Json, Index) -> (Json), atomic;
    JsonCursorSeek = "json_cursor_seek" (JsonCursor, Index) -> (Index), noalloc;
    JsonCursorFinish = "json_cursor_finish" (JsonCursor, Bool) -> (), throws, atomic;
    JsonResidue = "json_residue" (Aggregate, Json) -> (), noalloc;
    JsonRetain = "json_retain" (Aggregate, Json) -> (), noalloc;
    JsonSpread = "json_spread" (Aggregate, Aggregate) -> ();
    JsonOrderCopy = "json_order_copy" (Aggregate, Aggregate) -> ();
    JsonInsert = "json_insert" (Aggregate, String) -> ();
    JsonExclude = "json_exclude" (Aggregate, String) -> ();
    JsonMember = "json_member" (Json, String) -> (Json), throws;
    JsonKeys = "json_keys" (Json) -> (StringArray), throws;
    JsonIndex = "json_index" (Json, Number) -> (Json), throws;
    JsonLength = "json_length" (Json) -> (OptNumber), throws;
    JsonToNumber = "json_to_number" (Json) -> (Number), throws;
    JsonToI64 = "json_to_i64" (Json) -> (I64), throws;
    JsonToU64 = "json_to_u64" (Json) -> (U64), throws;
    JsonRequireNumber = "json_require_number" (Json) -> (Number), throws;
    JsonToString = "json_to_string" (Json) -> (String), throws;
    JsonToBool = "json_to_bool" (Json) -> (Bool), throws;
    JsonTruthy = "json_truthy" (Json) -> (Bool), pure, noalloc;
    JsonIsUndefined = "json_is_undefined" (Json) -> (Bool), pure, noalloc;
    JsonIsNull = "json_is_null" (Json) -> (Bool), pure, noalloc;
    JsonSerialize = "json_serialize" (Json) -> (OptString), throws;
    JsonDisplay = "json_display" (Json) -> (String);
    JsonTypeOf = "json_type_of" (Json) -> (String), pure;
    JsonRequireObject = "json_require_object" (Json) -> (), throws;
    JsonFirst = "json_first" (Json) -> (Json), throws;
    JsonNext = "json_next" (Json) -> (Json), pure, noalloc;
    // Object literals with spreads over JSON values: `{}`, `...source`, `key: value`.
    JsonObject = "json_object" () -> (Json);
    JsonAssign = "json_assign" (Json, Json) -> ();
    JsonSet = "json_set" (Json, String, Json) -> ();
    JsonFromNumber = "json_from_number" (Number) -> (Json), pure;
    JsonFromString = "json_from_string" (String) -> (Json), pure;
    JsonFromBool = "json_from_bool" (Bool) -> (Json), pure;
    JsonNull = "json_null" () -> (Json), pure;
    JsonUndefined = "json_undefined" () -> (Json), pure, noalloc;
    AbortNew = "abort_new" () -> (Abort);
    AbortAbort = "abort_abort" (Abort) -> (), throws;
    AbortAborted = "abort_aborted" (Abort) -> (Bool), noalloc;
    InstantFromMs = "instant_from_ms" (Number) -> (Instant), throws, atomic;
    InstantFrom = "instant_from" (String) -> (Instant), throws, atomic;
    InstantMs = "instant_ms" (Instant) -> (Number), throws, noalloc;
    InstantString = "instant_string" (Instant) -> (String), pure, atomic;
    PlainDateFrom = "plain_date_from" (String) -> (PlainDate), throws, atomic;
    PlainDateFromFields = "plain_date_from_fields" (Number, Number, Number, Number) -> (PlainDate), throws, atomic;
    PlainDateAddDays = "plain_date_add_days" (PlainDate, Number) -> (PlainDate), throws, atomic;
    PlainDateAddCalendar = "plain_date_add_calendar" (PlainDate, Number, Number, Number, Number, Number) -> (PlainDate), throws, atomic;
    PlainDateEpochDays = "plain_date_epoch_days" (PlainDate) -> (Number), pure, noalloc;
    PlainDateUntilPart = "plain_date_until_part" (PlainDate, PlainDate, Number, Number) -> (Number), throws, noalloc;
    PlainDatePart = "plain_date_part" (PlainDate, Number) -> (Number), throws, noalloc;
    PlainDateString = "plain_date_string" (PlainDate) -> (String), pure, atomic;
    PlainDateTimeFrom = "plain_date_time_from" (String) -> (PlainDateTime), throws, atomic;
    PlainDateTimeAddDays = "plain_date_time_add_days" (PlainDateTime, Number) -> (PlainDateTime), throws, atomic;
    PlainDateTimeAddCalendar = "plain_date_time_add_calendar" (PlainDateTime, Number, Number, Number, Number, Number) -> (PlainDateTime), throws, atomic;
    PlainDateTimePart = "plain_date_time_part" (PlainDateTime, Number) -> (Number), throws, noalloc;
    PlainDateTimeString = "plain_date_time_string" (PlainDateTime) -> (String), pure, atomic;

    // `Error` objects: `cause` (strings only, set at construction) and `name`.
    JsonStrictEqual = "json_strict_equal" (Json, Json) -> (Bool), pure, noalloc;
    JsonExceptionEqual = "json_exception_equal" (Json, Exception) -> (Bool), pure, noalloc;
    JsonOriginEqual = "json_origin_equal" (Json, Reference) -> (Bool), pure;
    JsonFromException = "json_from_exception" (Exception) -> (Json);
    JsonToException = "json_to_exception" (Json) -> (Exception), throws;
    JsonExceptionIs = "json_exception_is" (Json, Index) -> (Bool), pure;
    ExceptionSetCause = "exception_set_cause" (Exception, Json) -> (), noalloc;
    ExceptionIsNull = "exception_is_null" (Exception) -> (Bool), pure;
    ExceptionIsUndefined = "exception_is_undefined" (Exception) -> (Bool), pure;
    ExceptionCause = "exception_cause" (Exception) -> (Json), noalloc;
    ExceptionName = "exception_name" (Exception) -> (String), pure;
    ExceptionEqual = "exception_equal" (Exception, Exception) -> (Bool), pure, noalloc;
    ExceptionBigIntEqual = "exception_bigint_equal" (Exception, BigInt) -> (Bool), pure, noalloc;
    MathRandom = "math_random" () -> (Number), noalloc;
    RandomUuid = "random_uuid" () -> (String);
    BytesFillRandom = "bytes_fill_random" (Bytes) -> (Bytes), noalloc;
    ExceptionToBool = "exception_to_bool" (Exception) -> (Bool), throws;
    ExceptionToNumber = "exception_to_number" (Exception) -> (Number), throws;
    ExceptionToString = "exception_to_string" (Exception) -> (String), throws;
    FsReadText = "fs_read_text" (String) -> (PromiseString);
    FsReadBytes = "fs_read_bytes" (String) -> (PromiseBytes);
    FsWriteText = "fs_write_text" (String, String) -> (PromiseVoid);
    ModuleInitialize = "module_initialize" () -> (PromiseVoid);
    FsWriteBytes = "fs_write_bytes" (String, Bytes) -> (PromiseVoid);
    FsStat = "fs_stat" (String) -> (PromiseStats);
    FsReaddir = "fs_readdir" (String) -> (PromiseStringArray);
    FsMkdir = "fs_mkdir" (String) -> (PromiseVoid);
    FsUnlink = "fs_unlink" (String) -> (PromiseVoid);
    FsRmdir = "fs_rmdir" (String) -> (PromiseVoid);
    StatsIsFile = "stats_is_file" (Stats) -> (Bool), pure, noalloc;
    StatsIsDirectory = "stats_is_directory" (Stats) -> (Bool), pure, noalloc;
    StatsSize = "stats_size" (Stats) -> (Number), pure, noalloc;
    HeadersNew = "headers_new" () -> (Headers);
    HeadersCopy = "headers_copy" (Headers) -> (Headers);
    HeadersAppend = "headers_append" (Headers, String, String) -> (), throws;
    HeadersSet = "headers_set" (Headers, String, String) -> (), throws;
    HeadersGet = "headers_get" (Headers, String) -> (NullString), throws;
    HeadersHas = "headers_has" (Headers, String) -> (Bool), throws;
    HeadersDelete = "headers_delete" (Headers, String) -> (), throws;
    ResponseNew = "response_new" (OptBytes, Bool, Number, String, Headers) -> (Response), throws;
    ResponseStatus = "response_status" (Response) -> (Number), pure, noalloc;
    ResponseStatusText = "response_status_text" (Response) -> (String), pure, noalloc;
    ResponseOk = "response_ok" (Response) -> (Bool), pure, noalloc;
    ResponseHeaders = "response_headers" (Response) -> (Headers), pure, noalloc;
    ResponseBodyUsed = "response_body_used" (Response) -> (Bool), noalloc;
    ResponseBytes = "response_bytes" (Response) -> (PromiseBytes);
    ResponseText = "response_text" (Response) -> (PromiseString);
    ResponseJson = "response_json" (Response) -> (PromiseJson);
    RequestNew = "request_new" (String, String, OptBytes, Bool, Headers) -> (Request), throws;
    RequestMethod = "request_method" (Request) -> (String), pure, noalloc;
    RequestUrl = "request_url" (Request) -> (String), pure, noalloc;
    RequestHeaders = "request_headers" (Request) -> (Headers), pure, noalloc;
    RequestBodyUsed = "request_body_used" (Request) -> (Bool), noalloc;
    RequestBytes = "request_bytes" (Request) -> (PromiseBytes);
    RequestText = "request_text" (Request) -> (PromiseString);
    RequestJson = "request_json" (Request) -> (PromiseJson);
    // Serving `wasi:http/handler`: the request's parts (the handle is the `request`
    // resource), its body, then the response's headers and the response.
    ServeRequest = "serve_request" (Number) -> (Incoming);
    ServeBody = "serve_body" (Incoming) -> (PromiseBytes);
    ServeMethod = "serve_method" (Incoming) -> (String), pure, noalloc;
    ServeMethodOther = "serve_method_other" (Incoming) -> (OptString), pure, noalloc;
    ServeHasScheme = "serve_has_scheme" (Incoming) -> (Bool), pure, noalloc;
    ServeScheme = "serve_scheme" (Incoming) -> (String), pure, noalloc;
    ServeSchemeOther = "serve_scheme_other" (Incoming) -> (OptString), pure, noalloc;
    ServeAuthority = "serve_authority" (Incoming) -> (OptString), pure, noalloc;
    ServePath = "serve_path" (Incoming) -> (OptString), pure, noalloc;
    ServeHeaderCount = "serve_header_count" (Incoming) -> (Number), pure, noalloc;
    ServeHeaderName = "serve_header_name" (Incoming, Number) -> (String), pure;
    ServeHeaderValue = "serve_header_value" (Incoming, Number) -> (Bytes), pure;
    ServeAddHeader = "serve_add_header" (Incoming, String, Bytes) -> (), throws;
    ServeRespond = "serve_respond" (Incoming, Number, Bytes) -> (Number), throws;
    WritableStdout = "writable_stdout" () -> (Writable);
    WritableStderr = "writable_stderr" () -> (Writable);
    WritableLocked = "writable_locked" (Writable) -> (Bool), noalloc;
    WritableGetWriter = "writable_get_writer" (Writable) -> (Writer), throws;
    WriterWrite = "writer_write" (Writer, Bytes) -> (PromiseVoid);
    WriterClose = "writer_close" (Writer) -> (PromiseVoid);
    WriterReleaseLock = "writer_release_lock" (Writer) -> (), noalloc;
    ReadableLocked = "readable_locked" (Readable) -> (Bool), noalloc;
    ReadableGetReader = "readable_get_reader" (Readable) -> (Reader), throws;
    ReadableCancel = "readable_cancel" (Readable) -> (PromiseVoid);
    ReaderRead = "reader_read" (Reader) -> (PromiseReadResult);
    ReadableGetByobReader = "readable_get_byob_reader" (Readable) -> (ByobReader), throws;
    ReaderReadByob = "reader_read_byob" (ByobReader, Bytes, Number) -> (PromiseReadResult);
    ReaderCancel = "reader_cancel" (Reader) -> (PromiseVoid);
    ReaderReleaseLock = "reader_release_lock" (Reader) -> ();
    ReadDone = "read_done" (ReadResult) -> (Bool), pure, noalloc;
    ReadValue = "read_value" (ReadResult) -> (OptBytes), pure, noalloc;
    Fetch = "fetch" (String, String, Headers, OptBytes, Bool) -> (PromiseResponse);
    ResponseBody = "response_body" (Response) -> (NullReadable);
    RequestBody = "request_body" (Request) -> (NullReadable);
}

impl Intrinsic {
    /// Allocation and internal safe-point contract, including hidden temporaries.
    /// Unclassified helpers remain conservative.
    pub(crate) fn allocation(self) -> crate::effects::Allocation {
        if self.flags().contains(&Flag::NoAlloc) {
            crate::effects::Allocation::None
        } else if self.flags().contains(&Flag::Atomic) {
            crate::effects::Allocation::Atomic
        } else {
            crate::effects::Allocation::Conservative
        }
    }

    /// Pure services can be removed when unused.
    pub(crate) fn pure(self) -> bool {
        self.flags().contains(&Flag::Pure)
    }

    /// Whether the service may throw (as an ECMAScript exception).
    pub fn throws(self) -> bool {
        self.flags().contains(&Flag::Throws)
    }

    /// Whether the service produces a value (all services have at most one result).
    pub fn has_result(self) -> bool {
        !self.types().1.is_empty()
    }

    /// `Math.floor`, `ceil`, `trunc`, `round`, `abs`, `min` and `max`: an integer (or lossless
    /// numeric) operand type carries over to the result.
    pub(crate) fn preserves_integers(self) -> bool {
        matches!(
            self.numeric_contract(),
            Some(
                BuiltinContract::Round
                    | BuiltinContract::Abs
                    | BuiltinContract::Min
                    | BuiltinContract::Max
            )
        )
    }
}

/// Builtin numeric and value-range contract for `ProgramFacts`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuiltinContract {
    /// `Math.floor`, `Math.ceil`, `Math.trunc`, `Math.round`: identity on exact integers.
    Round,
    /// `Math.abs`: preserves integer domain and non-negative bounds when negation cannot overflow.
    Abs,
    /// `Math.min`: preserves common integer domain and elementwise minimum bounds.
    Min,
    /// `Math.max`: preserves common integer domain and elementwise maximum bounds.
    Max,
    /// Non-negative 32-bit byte/buffer length or offset (`0..=u32::MAX`).
    UnsignedCount,
    /// Unsigned byte read (`0..=255`).
    ByteValue,
    /// Produces or consumes an IEEE-754 `f64` number (`Math.sqrt`, `parseInt`, `parseFloat`, `Number(s)`).
    NumberOnly,
    /// Boolean numeric predicate (`Number.isInteger`, `Number.isSafeInteger`, `Number.isFinite`, `Number.isNaN`).
    Predicate,
}

/// The service `op` calls, if it is a `perry.intrinsic`.
pub(crate) fn intrinsic_of(ctx: &Context, op: Ptr<Operation>) -> Option<Intrinsic> {
    Operation::get_op::<IntrinsicOp>(op, ctx).map(|op| op.intrinsic(ctx))
}

/// Whether `op` may throw: it implements [MayThrowInterface], and for intrinsics the service
/// throws.
pub fn may_throw(ctx: &Context, op: Ptr<Operation>) -> bool {
    if let Some(intrinsic) = Operation::get_op::<IntrinsicOp>(op, ctx) {
        return intrinsic.intrinsic(ctx).throws();
    }
    op_cast::<dyn MayThrowInterface>(Operation::get_op_dyn(op, ctx).as_ref()).is_some()
}

#[derive(Debug, Error)]
enum IntrinsicErr {
    #[error("missing intrinsic attribute")]
    Missing,
    #[error("intrinsic {0} has the wrong operand or result types")]
    Signature(String),
}

/// A call of a host or runtime service.
#[pliron_op(
    name = "perry.intrinsic",
    format = "attr($perry_intrinsic, $Intrinsic) ` (` operands(CharSpace(`,`)) `) : ` types(CharSpace(`,`))",
    attributes = (perry_intrinsic: Intrinsic)
)]
pub struct IntrinsicOp;

impl IntrinsicOp {
    pub fn new(ctx: &mut Context, intrinsic: Intrinsic, args: Vec<Value>) -> Self {
        let results = intrinsic
            .types()
            .1
            .iter()
            .map(|ty| ty.get(ctx))
            .collect::<Vec<_>>();
        let op = IntrinsicOp {
            op: Self::new_op(ctx, results, args),
        };
        op.set_attr_perry_intrinsic(ctx, intrinsic);
        op
    }

    /// The service; the verifier rejects an op without one.
    pub fn intrinsic(&self, ctx: &Context) -> Intrinsic {
        // pliron models every attribute as optional; construction always sets this one.
        *self
            .get_attr_perry_intrinsic(ctx)
            .expect("verified perry.intrinsic has its service")
    }
}

impl Verify for IntrinsicOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let Some(intrinsic) = self.get_attr_perry_intrinsic(ctx).map(|i| *i) else {
            return verify_err!(self.loc(ctx), IntrinsicErr::Missing);
        };
        let op = self.get_operation().deref(ctx);
        let operands: Vec<_> = op.operands().map(|v| v.get_type(ctx)).collect();
        let results: Vec<_> = op.results().map(|v| v.get_type(ctx)).collect();
        drop(op);
        let (ins, outs) = intrinsic.types();
        let operands_fit = operands.len() == ins.len()
            && operands.iter().zip(ins).all(|(t, e)| e.matches(ctx, *t));
        let results_fit = results.len() == outs.len()
            && results.iter().zip(outs).all(|(t, e)| e.matches(ctx, *t));
        let specialized_int_fit = match intrinsic {
            _ if intrinsic.preserves_integers() => {
                operands.len() == ins.len()
                    && results.len() == 1
                    && (results[0].deref(ctx).is::<IntegerType>()
                        || results[0].deref(ctx).is::<crate::types::NumericType>())
                    && operands.iter().all(|&t| t == results[0])
            }
            Intrinsic::BytesAt => {
                operands_fit && results.len() == 1 && results[0].deref(ctx).is::<IntegerType>()
            }
            Intrinsic::ParseInt => {
                operands.len() == 2
                    && results.len() == 1
                    && operands[0].deref(ctx).is::<StringType>()
                    && (operands[1].deref(ctx).is::<FP64Type>()
                        || operands[1].deref(ctx).is::<crate::types::NumericType>()
                        || operands[1].deref(ctx).is::<IntegerType>())
                    && (results[0].deref(ctx).is::<FP64Type>()
                        || results[0].deref(ctx).is::<crate::types::NumericType>()
                        || results[0].deref(ctx).is::<IntegerType>())
            }
            Intrinsic::NumberToString
            | Intrinsic::NumberIsNaN
            | Intrinsic::NumberIsFinite
            | Intrinsic::NumberIsInteger
            | Intrinsic::NumberIsSafeInteger => {
                operands.len() == 1
                    && (operands[0].deref(ctx).is::<crate::types::NumericType>()
                        || operands[0].deref(ctx).is::<IntegerType>())
                    && results_fit
            }
            Intrinsic::NumberToStringRadix => {
                operands.len() == 2
                    && (operands[0].deref(ctx).is::<FP64Type>()
                        || operands[0].deref(ctx).is::<crate::types::NumericType>()
                        || operands[0].deref(ctx).is::<IntegerType>())
                    && operands[1].deref(ctx).is::<FP64Type>()
                    && results_fit
            }
            _ => false,
        };
        if !(operands_fit && results_fit) && !specialized_int_fit {
            verify_err!(
                self.loc(ctx),
                IntrinsicErr::Signature(intrinsic.name().into())
            )?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for IntrinsicOp {
    fn has_side_effects(&self, ctx: &Context) -> bool {
        !self.intrinsic(ctx).pure()
    }
}

/// Throwing intrinsics raise ordinary errors; [may_throw] decides per service.
#[op_interface_impl]
impl MayThrowInterface for IntrinsicOp {
    fn exception(&self, _ctx: &Context) -> ErrorKind {
        ErrorKind::Any
    }
}

#[derive(Debug, Error)]
#[error("perry.json_stringify produces a string, or string | undefined for an optional value")]
struct StringifyErr;

/// `JSON.stringify(value)` of a typed value: records, arrays, strings, numbers, booleans,
/// `Uint8Array`s, JSON values and their optionals.
#[pliron_op(
    name = "perry.json_stringify",
    format = "$0 ` : ` type($0)",
    interfaces = [pliron::builtin::op_interfaces::OneOpdInterface, pliron::builtin::op_interfaces::OneResultInterface]
)]
pub struct JsonStringifyOp;

impl JsonStringifyOp {
    pub fn new(ctx: &mut Context, value: Value) -> Self {
        let string: TypeHandle = StringType::get(ctx).into();
        let optional = value.get_type(ctx).deref(ctx).is::<OptionalType>()
            || value.get_type(ctx).deref(ctx).is::<JsonType>();
        let result = if optional {
            OptionalType::get(ctx, string).into()
        } else {
            string
        };
        JsonStringifyOp {
            op: Self::new_op(ctx, vec![result], vec![value]),
        }
    }
}

impl Verify for JsonStringifyOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        use pliron::builtin::op_interfaces::OneResultInterface;
        let result = self.get_result(ctx).get_type(ctx);
        if !(Ty::String.matches(ctx, result) || Ty::OptString.matches(ctx, result)) {
            verify_err!(self.loc(ctx), StringifyErr)?
        }
        Ok(())
    }
}

side_effects!(true; JsonStringifyOp, JsonCursorKeyOp);

#[op_interface_impl]
impl MayThrowInterface for JsonStringifyOp {
    fn exception(&self, _ctx: &Context) -> ErrorKind {
        ErrorKind::TypeError
    }
}

/// Select a record field by its static schema names, without allocating a key string.
#[pliron_op(
    name = "perry.json_cursor_key",
    format = "$0 ` : ` type($0) ` schema ` attr($perry_json_record, $TypeAttr)",
    interfaces = [pliron::builtin::op_interfaces::OneOpdInterface, pliron::builtin::op_interfaces::OneResultInterface],
    attributes = (perry_json_record: pliron::builtin::attributes::TypeAttr)
)]
pub struct JsonCursorKeyOp;
impl JsonCursorKeyOp {
    pub fn new(ctx: &mut Context, cursor: Value, record: TypeHandle) -> Self {
        let index = crate::collections::index_type(ctx);
        let op = Self {
            op: Self::new_op(ctx, vec![index], vec![cursor]),
        };
        op.set_attr_perry_json_record(ctx, pliron::builtin::attributes::TypeAttr::new(record));
        op
    }
    pub fn record(&self, ctx: &Context) -> TypeHandle {
        self.get_attr_perry_json_record(ctx)
            .expect("record schema")
            .get_type(ctx)
    }
}
impl Verify for JsonCursorKeyOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let operand = self.get_operation().deref(ctx).get_operand(0).get_type(ctx);
        let schema = self.record(ctx);
        let schema_ref = schema.deref(ctx);
        if !operand.deref(ctx).is::<crate::types::JsonCursorType>()
            || !(schema_ref.is::<crate::types::RecordType>()
                || schema_ref.is::<crate::types::VariantType>())
        {
            verify_err!(
                self.loc(ctx),
                IntrinsicErr::Signature("json_cursor_key".into())
            )?
        }
        Ok(())
    }
}
