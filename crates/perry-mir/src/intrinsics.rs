//! Host and runtime services with fixed signatures (`perry.intrinsic`).
//!
//! The builder recognizes the standard API (`console.log`, `Math.floor`, string methods,
//! `Uint8Array`, `JSON`, `Date`, …) and emits one op per service, named by [Intrinsic];
//! codegen maps each to Wasm instructions or a runtime function and checks that the WIT world
//! imports what it needs. Some services throw ([Intrinsic::throws]); they implement
//! [MayThrowInterface] through [may_throw].
use crate::{
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
        attributes::IdentifierAttr,
        types::{FP64Type, IntegerType, Signedness},
    },
    common_traits::Verify,
    context::{Context, Ptr},
    derive::{op_interface_impl, pliron_op},
    identifier::Identifier,
    op::{Op, op_cast},
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::{TypeHandle, Typed},
    value::Value,
    verify_err,
};
use thiserror::Error;

/// Operand and result kinds of intrinsic signatures.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ty {
    BigInt,
    /// A signless i32 index proven in bounds.
    Index,
    /// Unicode scalar value, or -1 for the empty result of charAt. Internal MIR only.
    Scalar,
    Number,
    Bool,
    String,
    OptString,
    NullString,
    OptNumber,
    StringArray,
    NumberArray,
    Bytes,
    ByteBuffer,
    ByobReader,
    ByteInput,
    Url,
    SearchParams,
    Json,
    JsonReader,
    Aggregate,
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
    pub fn get(self, ctx: &mut Context) -> TypeHandle {
        let string: TypeHandle = StringType::get(ctx).into();
        let number: TypeHandle = FP64Type::get(ctx).into();
        match self {
            Ty::BigInt => crate::types::BigIntType::get(ctx).into(),
            Ty::Index | Ty::Scalar => crate::collections::index_type(ctx),
            Ty::Number => number,
            Ty::Bool => IntegerType::get(ctx, 1, Signedness::Signless).into(),
            Ty::String => string,
            Ty::OptString => OptionalType::get(ctx, string).into(),
            Ty::NullString => {
                OptionalType::get_with(ctx, string, crate::attrs::Absence::Null).into()
            }
            Ty::OptNumber => OptionalType::get(ctx, number).into(),
            Ty::StringArray => ArrayType::get(ctx, string).into(),
            Ty::NumberArray => ArrayType::get(ctx, number).into(),
            Ty::Bytes => BytesType::get(ctx).into(),
            Ty::ByteBuffer => crate::types::ByteBufferType::get(ctx).into(),
            Ty::ByobReader => crate::types::ByobReaderType::get(ctx).into(),
            Ty::ByteInput => crate::types::ByteInputType::get(ctx).into(),
            Ty::Url => crate::types::UrlType::get(ctx).into(),
            Ty::SearchParams => crate::types::SearchParamsType::get(ctx).into(),
            Ty::Json => JsonType::get(ctx).into(),
            Ty::JsonReader => crate::types::JsonReaderType::get(ctx).into(),
            Ty::Aggregate => panic!("aggregate is an operand-only intrinsic kind"),
            Ty::Regex => crate::types::RegexType::get(ctx).into(),
            Ty::Decoder => crate::types::DecoderType::get(ctx).into(),
            Ty::Date => DateType::get(ctx).into(),
            Ty::Exception => ExceptionType::get(ctx).into(),
            Ty::Stats => StatsType::get(ctx).into(),
            Ty::OptBytes => {
                let bytes = BytesType::get(ctx).into();
                OptionalType::get(ctx, bytes).into()
            }
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
            Ty::PromiseReadResult => {
                let result = ReadResultType::get(ctx).into();
                PromiseType::get(ctx, result).into()
            }
            Ty::NullReadable => {
                let stream = ReadableType::get(ctx).into();
                OptionalType::get_with(ctx, stream, crate::attrs::Absence::Null).into()
            }
            Ty::PromiseResponse => {
                let response = ResponseType::get(ctx).into();
                PromiseType::get(ctx, response).into()
            }
            Ty::PromiseString => PromiseType::get(ctx, string).into(),
            Ty::PromiseBytes => {
                let bytes = BytesType::get(ctx).into();
                PromiseType::get(ctx, bytes).into()
            }
            Ty::PromiseJson => {
                let json = JsonType::get(ctx).into();
                PromiseType::get(ctx, json).into()
            }
            Ty::PromiseStats => {
                let stats = StatsType::get(ctx).into();
                PromiseType::get(ctx, stats).into()
            }
            Ty::PromiseStringArray => {
                let array = ArrayType::get(ctx, string).into();
                PromiseType::get(ctx, array).into()
            }
            Ty::PromiseVoid => {
                let undefined = OptionalType::get(ctx, number).into();
                PromiseType::get(ctx, undefined).into()
            }
        }
    }

    fn matches(self, ctx: &Context, ty: TypeHandle) -> bool {
        let deref = ty.deref(ctx);
        let optional_of = |is: &dyn Fn(TypeHandle) -> bool| {
            deref
                .downcast_ref::<OptionalType>()
                .is_some_and(|o| is(o.value()))
        };
        let array_of = |is: &dyn Fn(TypeHandle) -> bool| {
            deref
                .downcast_ref::<ArrayType>()
                .is_some_and(|a| is(a.element()))
        };
        let promise_of = |is: &dyn Fn(TypeHandle) -> bool| {
            deref
                .downcast_ref::<PromiseType>()
                .is_some_and(|p| is(p.value()))
        };
        let string = |t: TypeHandle| t.deref(ctx).is::<StringType>();
        let number = |t: TypeHandle| t.deref(ctx).is::<FP64Type>();
        match self {
            Ty::BigInt => deref.is::<crate::types::BigIntType>(),
            Ty::Index | Ty::Scalar => deref
                .downcast_ref::<IntegerType>()
                .is_some_and(|i| i.width() == 32 && i.signedness() == Signedness::Signless),
            Ty::Number => deref.is::<FP64Type>(),
            Ty::Bool => deref
                .downcast_ref::<IntegerType>()
                .is_some_and(|i| i.width() == 1),
            Ty::String => deref.is::<StringType>(),
            Ty::OptString => {
                optional_of(&string)
                    && crate::optional::absence(ctx, ty) == Some(crate::attrs::Absence::Undefined)
            }
            Ty::NullString => {
                optional_of(&string)
                    && crate::optional::absence(ctx, ty) == Some(crate::attrs::Absence::Null)
            }
            Ty::OptNumber => optional_of(&number),
            Ty::StringArray => array_of(&string),
            Ty::NumberArray => array_of(&number),
            Ty::Bytes => deref.is::<BytesType>(),
            Ty::ByteBuffer => deref.is::<crate::types::ByteBufferType>(),
            Ty::ByobReader => deref.is::<crate::types::ByobReaderType>(),
            Ty::ByteInput => deref.is::<crate::types::ByteInputType>(),
            Ty::Url => deref.is::<crate::types::UrlType>(),
            Ty::SearchParams => deref.is::<crate::types::SearchParamsType>(),
            Ty::Json => deref.is::<JsonType>(),
            Ty::JsonReader => deref.is::<crate::types::JsonReaderType>(),
            Ty::Aggregate => {
                deref.is::<crate::types::RecordType>()
                    || deref.is::<crate::types::ArrayType>()
                    || deref.is::<crate::types::VariantType>()
            }
            Ty::Regex => deref.is::<crate::types::RegexType>(),
            Ty::Decoder => deref.is::<crate::types::DecoderType>(),
            Ty::Date => deref.is::<DateType>(),
            Ty::Exception => deref.is::<ExceptionType>(),
            Ty::Stats => deref.is::<StatsType>(),
            Ty::OptBytes => optional_of(&|t| t.deref(ctx).is::<BytesType>()),
            Ty::Headers => deref.is::<HeadersType>(),
            Ty::Response => deref.is::<ResponseType>(),
            Ty::Request => deref.is::<RequestType>(),
            Ty::Writable => deref.is::<WritableType>(),
            Ty::Writer => deref.is::<WriterType>(),
            Ty::Readable => deref.is::<ReadableType>(),
            Ty::Reader => deref.is::<ReaderType>() || deref.is::<crate::types::ByobReaderType>(),
            Ty::ReadResult => deref.is::<ReadResultType>(),
            Ty::Incoming => deref.is::<IncomingType>(),
            Ty::Abort => deref.is::<AbortType>(),
            Ty::Instant => deref.is::<InstantType>(),
            Ty::PlainDate => deref.is::<PlainDateType>(),
            Ty::PlainDateTime => deref.is::<PlainDateTimeType>(),
            Ty::PromiseReadResult => promise_of(&|t| t.deref(ctx).is::<ReadResultType>()),
            Ty::NullReadable => {
                optional_of(&|t| t.deref(ctx).is::<ReadableType>())
                    && crate::optional::absence(ctx, ty) == Some(crate::attrs::Absence::Null)
            }
            Ty::PromiseResponse => promise_of(&|t| t.deref(ctx).is::<ResponseType>()),
            Ty::PromiseString => promise_of(&string),
            Ty::PromiseBytes => promise_of(&|t| t.deref(ctx).is::<BytesType>()),
            Ty::PromiseJson => promise_of(&|t| t.deref(ctx).is::<JsonType>()),
            Ty::PromiseStats => promise_of(&|t| t.deref(ctx).is::<StatsType>()),
            Ty::PromiseStringArray => promise_of(&|t| {
                t.deref(ctx)
                    .downcast_ref::<ArrayType>()
                    .is_some_and(|a| string(a.element()))
            }),
            Ty::PromiseVoid => promise_of(&|t| {
                t.deref(ctx)
                    .downcast_ref::<OptionalType>()
                    .is_some_and(|o| number(o.value()))
            }),
        }
    }
}

macro_rules! intrinsics {
    ($($variant:ident = $name:literal ($($operand:ident),*) -> ($($result:ident),*) $(, $flag:ident)*;)*) => {
        /// The services; see each name for the API it implements.
        #[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
        pub enum Intrinsic {
            $($variant,)*
        }

        impl Intrinsic {
            pub const ALL: &[Intrinsic] = &[$(Intrinsic::$variant,)*];

            pub fn name(self) -> &'static str {
                match self {
                    $(Intrinsic::$variant => $name,)*
                }
            }

            /// (operands, results).
            pub fn types(self) -> (&'static [Ty], &'static [Ty]) {
                match self {
                    $(Intrinsic::$variant => (&[$(Ty::$operand),*], &[$(Ty::$result),*]),)*
                }
            }

            fn flags(self) -> &'static [&'static str] {
                match self {
                    $(Intrinsic::$variant => &[$(stringify!($flag)),*],)*
                }
            }
        }
    };
}

intrinsics! {
    BigIntFromNumber = "bigint_from_number" (Number) -> (BigInt), throws;
    BigIntNegate = "bigint_negate" (BigInt) -> (BigInt), pure;
    BigIntToNumber = "bigint_to_number" (BigInt) -> (Number), pure, noalloc;
    BigIntToString = "bigint_to_string" (BigInt) -> (String), pure;
    PrintOut = "print_out" (String) -> ();
    PrintErr = "print_err" (String) -> ();
    NumberToString = "number_to_string" (Number) -> (String), pure;
    EnvGet = "env_get" (String) -> (OptString);
    EnvKeys = "env_keys" () -> (StringArray);
    PerformanceNow = "performance_now" () -> (Number), noalloc;
    DateNow = "date_now" () -> (Number), noalloc;
    ExitCodeSet = "exit_code_set" (Number) -> (), noalloc;
    MathFloor = "math_floor" (Number) -> (Number), pure, noalloc;
    MathCeil = "math_ceil" (Number) -> (Number), pure, noalloc;
    MathTrunc = "math_trunc" (Number) -> (Number), pure, noalloc;
    MathAbs = "math_abs" (Number) -> (Number), pure, noalloc;
    MathRound = "math_round" (Number) -> (Number), pure, noalloc;
    MathSqrt = "math_sqrt" (Number) -> (Number), pure, noalloc;
    MathMin = "math_min" (Number, Number) -> (Number), pure, noalloc;
    MathMax = "math_max" (Number, Number) -> (Number), pure, noalloc;
    DateFromTime = "date_from_time" (Number) -> (Date);
    DateTime = "date_time" (Date) -> (Number), noalloc;
    DateParse = "date_parse" (String) -> (Number), pure, noalloc;
    DateUtc = "date_utc" (Number, Number, Number, Number, Number, Number, Number) -> (Number), pure, noalloc;
    DatePart = "date_part" (Date, Number) -> (Number), noalloc;
    DateSet = "date_set" (Date, Number, Number, Number, Number, Number, Number) -> (Number), noalloc;
    DateIso = "date_iso" (Date) -> (String), throws;
    DateJson = "date_json" (Date) -> (NullString);
    StringFromCodePoint = "string_from_code_point" (Number) -> (String), throws;
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
    StringSplit = "string_split" (String, String) -> (StringArray), pure, atomic;
    StringIndex = "string_index" (String, Number) -> (OptString), pure, atomic;
    StringLower = "string_lower" (String) -> (String), pure, atomic;
    StringUpper = "string_upper" (String) -> (String), pure, atomic;
    StringRepeat = "string_repeat" (String, Number) -> (String), throws;
    BytesNew = "bytes_new" (Number) -> (Bytes), throws;
    BytesFromArray = "bytes_from_array" (NumberArray) -> (Bytes);
    BytesCopy = "bytes_copy" (Bytes) -> (Bytes), throws;
    BytesBuffer = "bytes_buffer" (Bytes) -> (ByteBuffer), pure, noalloc;
    BytesOffset = "bytes_offset" (Bytes) -> (Number), noalloc;
    BufferLength = "buffer_length" (ByteBuffer) -> (Number), noalloc;
    BytesFromBuffer = "bytes_from_buffer" (ByteBuffer) -> (Bytes), throws;
    BytesAt = "bytes_at" (Bytes, Index) -> (Number), pure, noalloc;
    BytesGet = "bytes_get" (Bytes, Number) -> (OptNumber), noalloc;
    BytesSet = "bytes_set" (Bytes, Number, Number) -> (), noalloc;
    BytesLength = "bytes_length" (Bytes) -> (Number), noalloc;
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
    DecoderNew = "decoder_new" (Bool, Bool) -> (Decoder);
    TextDecode = "text_decode" (Decoder, Bytes, Bool) -> (String), throws;
    JsonParse = "json_parse" (String) -> (Json), throws;
    JsonMember = "json_member" (Json, String) -> (Json), throws;
    JsonKeys = "json_keys" (Json) -> (StringArray), throws;
    JsonIndex = "json_index" (Json, Number) -> (Json), throws;
    JsonLength = "json_length" (Json) -> (OptNumber), throws;
    JsonToNumber = "json_to_number" (Json) -> (Number), throws;
    JsonRequireNumber = "json_require_number" (Json) -> (Number), throws;
    JsonToString = "json_to_string" (Json) -> (String), throws;
    JsonToBool = "json_to_bool" (Json) -> (Bool), throws;
    JsonTruthy = "json_truthy" (Json) -> (Bool), pure, noalloc;
    JsonIsUndefined = "json_is_undefined" (Json) -> (Bool), pure, noalloc;
    JsonIsNull = "json_is_null" (Json) -> (Bool), pure, noalloc;
    JsonSerialize = "json_serialize" (Json) -> (OptString), throws;
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
    JsonUndefined = "json_undefined" () -> (Json), pure;
    // Typed `JSON.parse`: generated decoders pull values from a reader (perry-std
    // `json_read`). Only `end` and `fail` throw; the rest record errors for them.
    JsonReadVariant = "json_read_variant" (JsonReader, String) -> (Index), atomic;
    JsonReadPayload = "json_read_payload" (JsonReader) -> (Bool), noalloc;
    JsonReadResume = "json_read_resume" (JsonReader, Index) -> (), noalloc;
    JsonReadPosition = "json_read_position" (JsonReader) -> (Index), noalloc;
    JsonReadAttach = "json_read_attach" (JsonReader, Aggregate, Index, String, Index) -> (), atomic;
    JsonReadBegin = "json_read_begin" (String) -> (JsonReader), atomic;
    JsonReadEnd = "json_read_end" (JsonReader) -> (), throws, atomic;
    JsonReadFail = "json_read_fail" (JsonReader, Index) -> (), noalloc;
    JsonReadTakeError = "json_read_take_error" (JsonReader) -> (Index), noalloc;
    JsonReadKind = "json_read_kind" (JsonReader) -> (Index), noalloc;
    JsonReadSkip = "json_read_skip" (JsonReader) -> (), noalloc;
    JsonReadRaw = "json_read_raw" (JsonReader) -> (String), atomic;
    JsonReadNumber = "json_read_number" (JsonReader) -> (Number), noalloc;
    JsonReadBool = "json_read_bool" (JsonReader) -> (Bool), noalloc;
    JsonReadString = "json_read_string" (JsonReader) -> (String), atomic;
    JsonReadObject = "json_read_object" (JsonReader) -> (Bool), noalloc;
    JsonReadArray = "json_read_array" (JsonReader) -> (Bool), noalloc;
    JsonReadElement = "json_read_element" (JsonReader) -> (Bool), noalloc;
    JsonReadField = "json_read_field" (JsonReader, String, Index) -> (Index), atomic;
    AbortNew = "abort_new" () -> (Abort);
    AbortAbort = "abort_abort" (Abort) -> (), throws;
    AbortAborted = "abort_aborted" (Abort) -> (Bool), noalloc;
    InstantFromMs = "instant_from_ms" (Number) -> (Instant), throws;
    InstantFrom = "instant_from" (String) -> (Instant), throws;
    InstantMs = "instant_ms" (Instant) -> (Number), pure, noalloc;
    InstantString = "instant_string" (Instant) -> (String), pure;
    PlainDateFrom = "plain_date_from" (String) -> (PlainDate), throws;
    PlainDateAddDays = "plain_date_add_days" (PlainDate, Number) -> (PlainDate), throws;
    PlainDatePart = "plain_date_part" (PlainDate, Number) -> (Number), pure, noalloc;
    PlainDateString = "plain_date_string" (PlainDate) -> (String), pure;
    PlainDateTimeFrom = "plain_date_time_from" (String) -> (PlainDateTime), throws;
    PlainDateTimeAddDays = "plain_date_time_add_days" (PlainDateTime, Number) -> (PlainDateTime), throws;
    PlainDateTimePart = "plain_date_time_part" (PlainDateTime, Number) -> (Number), pure, noalloc;
    PlainDateTimeString = "plain_date_time_string" (PlainDateTime) -> (String), pure;

    // `Error` objects: `cause` (strings only, set at construction) and `name`.
    ExceptionSetCause = "exception_set_cause" (Exception, String) -> (), noalloc;
    ExceptionCause = "exception_cause" (Exception) -> (OptString), noalloc;
    ExceptionName = "exception_name" (Exception) -> (String), pure;
    MathRandom = "math_random" () -> (Number), noalloc;
    RandomUuid = "random_uuid" () -> (String);
    BytesFillRandom = "bytes_fill_random" (Bytes) -> (Bytes), noalloc;
    ExceptionToNumber = "exception_to_number" (Exception) -> (Number), throws;
    ExceptionToString = "exception_to_string" (Exception) -> (String), throws;
    FsReadText = "fs_read_text" (String) -> (PromiseString);
    FsReadBytes = "fs_read_bytes" (String) -> (PromiseBytes);
    FsWriteText = "fs_write_text" (String, String) -> (PromiseVoid);
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
    pub fn allocation(self) -> crate::effects::Allocation {
        if self.flags().contains(&"noalloc") {
            crate::effects::Allocation::None
        } else if self.flags().contains(&"atomic") {
            crate::effects::Allocation::Atomic
        } else {
            crate::effects::Allocation::Conservative
        }
    }

    pub(crate) fn may_allocate(self) -> bool {
        self.allocation() != crate::effects::Allocation::None
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|i| i.name() == name)
    }

    /// Pure services can be removed when unused.
    pub fn pure(self) -> bool {
        self.flags().contains(&"pure")
    }

    /// Whether the service may throw (as an ECMAScript exception).
    pub fn throws(self) -> bool {
        self.flags().contains(&"throws")
    }
}

/// Whether `op` may throw: it implements [MayThrowInterface], and for intrinsics the service
/// throws.
pub fn may_throw(ctx: &Context, op: Ptr<Operation>) -> bool {
    if let Some(intrinsic) = Operation::get_op::<IntrinsicOp>(op, ctx) {
        return intrinsic.intrinsic(ctx).is_some_and(Intrinsic::throws);
    }
    op_cast::<dyn MayThrowInterface>(Operation::get_op_dyn(op, ctx).as_ref()).is_some()
}

#[derive(Debug, Error)]
pub(crate) enum IntrinsicErr {
    #[error("unknown intrinsic {0}")]
    Unknown(String),
    #[error("intrinsic {0} has the wrong operand or result types")]
    Signature(String),
}

/// A call of a host or runtime service.
#[pliron_op(
    name = "perry.intrinsic",
    format = "attr($perry_intrinsic, $IdentifierAttr) ` (` operands(CharSpace(`,`)) `) : ` types(CharSpace(`,`))",
    attributes = (perry_intrinsic: IdentifierAttr)
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
            op: Operation::new(ctx, Self::get_concrete_op_info(), results, args, vec![], 0),
        };
        let name: Identifier = intrinsic.name().try_into().expect("identifier");
        op.set_attr_perry_intrinsic(ctx, IdentifierAttr::new(name));
        op
    }

    pub fn intrinsic(&self, ctx: &Context) -> Option<Intrinsic> {
        let name: Identifier = self.get_attr_perry_intrinsic(ctx)?.clone().into();
        Intrinsic::from_name(name.as_ref())
    }
}

impl Verify for IntrinsicOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let Some(intrinsic) = self.intrinsic(ctx) else {
            let name = format!(
                "{:?}",
                self.get_attr_perry_intrinsic(ctx).map(|a| a.clone())
            );
            return verify_err!(self.loc(ctx), IntrinsicErr::Unknown(name));
        };
        let op = self.get_operation().deref(ctx);
        let operands: Vec<_> = op.operands().map(|v| v.get_type(ctx)).collect();
        let results: Vec<_> = op.results().map(|v| v.get_type(ctx)).collect();
        drop(op);
        let (ins, outs) = intrinsic.types();
        let fits = |types: &[TypeHandle], expected: &[Ty]| {
            types.len() == expected.len()
                && types.iter().zip(expected).all(|(t, e)| e.matches(ctx, *t))
        };
        if !fits(&operands, ins) || !fits(&results, outs) {
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
        !self.intrinsic(ctx).is_some_and(Intrinsic::pure)
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
pub(crate) struct StringifyErr;

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
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![result],
                vec![value],
                vec![],
                0,
            ),
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

#[op_interface_impl]
impl SideEffects for JsonStringifyOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        true
    }
}

#[op_interface_impl]
impl MayThrowInterface for JsonStringifyOp {
    fn exception(&self, _ctx: &Context) -> ErrorKind {
        ErrorKind::TypeError
    }
}

#[derive(Debug, Error)]
#[error("perry.promise_combinator takes an array of promises of one type")]
pub(crate) struct CombinatorErr;

/// How a [PromiseCombinatorOp] settles.
#[pliron::derive::pliron_attr(name = "perry.combinator", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum Combinator {
    /// `Promise.all`: an array of every value, or the first rejection.
    All,
    /// `Promise.race`: the first settled input, in time and then input order.
    Race,
    /// `Promise.allSettled`: every input once all have settled, as `perry.settled` views.
    AllSettled,
}

/// `Promise.all`, `Promise.race` and `Promise.allSettled` of `perry.array<perry.promise<T>>`.
#[pliron_op(
    name = "perry.promise_combinator",
    format = "attr($perry_combinator, $Combinator) ` ` $0 ` : ` type($0)",
    interfaces = [pliron::builtin::op_interfaces::OneOpdInterface, pliron::builtin::op_interfaces::OneResultInterface],
    attributes = (perry_combinator: Combinator)
)]
pub struct PromiseCombinatorOp;

impl PromiseCombinatorOp {
    /// `None` unless `promises` is an array of promises.
    pub fn new(ctx: &mut Context, combinator: Combinator, promises: Value) -> Option<Self> {
        use crate::types::PromiseType;
        let element =
            pliron::r#type::TypedHandle::<ArrayType>::from_handle(promises.get_type(ctx), ctx)
                .ok()?
                .deref(ctx)
                .element();
        let value = pliron::r#type::TypedHandle::<PromiseType>::from_handle(element, ctx)
            .ok()?
            .deref(ctx)
            .value();
        let settled = match combinator {
            Combinator::All => ArrayType::get(ctx, value).into(),
            Combinator::Race => value,
            Combinator::AllSettled => {
                let settled = crate::types::SettledType::get(ctx, value).into();
                ArrayType::get(ctx, settled).into()
            }
        };
        let result = PromiseType::get(ctx, settled).into();
        let op = PromiseCombinatorOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![result],
                vec![promises],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_combinator(ctx, combinator);
        Some(op)
    }

    pub fn combinator(&self, ctx: &Context) -> Combinator {
        *self.get_attr_perry_combinator(ctx).expect("combinator")
    }
}

impl Verify for PromiseCombinatorOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        use crate::types::PromiseType;
        let operand = self.get_operation().deref(ctx).get_operand(0).get_type(ctx);
        let ok = pliron::r#type::TypedHandle::<ArrayType>::from_handle(operand, ctx)
            .is_ok_and(|a| a.deref(ctx).element().deref(ctx).is::<PromiseType>());
        if !ok {
            verify_err!(self.loc(ctx), CombinatorErr)?
        }
        Ok(())
    }
}

/// A promise fulfilled with its operand: a plain value among a combinator's inputs
/// (`Promise.all([pending, 1])`).
#[pliron_op(
    name = "perry.promise_resolved",
    format = "$0 ` : ` type($0)",
    interfaces = [pliron::builtin::op_interfaces::OneOpdInterface, pliron::builtin::op_interfaces::OneResultInterface]
)]
pub struct PromiseResolvedOp;

impl PromiseResolvedOp {
    pub fn new(ctx: &mut Context, value: Value) -> Self {
        let ty = value.get_type(ctx);
        let result = crate::types::PromiseType::get(ctx, ty).into();
        PromiseResolvedOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![result],
                vec![value],
                vec![],
                0,
            ),
        }
    }
}

/// A promise rejected with its operand (`Promise.reject(e)`), of `perry.promise<T>` for the
/// settled type `T` its context expects.
#[pliron_op(
    name = "perry.promise_rejected",
    format = "$0 ` : ` type($0)",
    interfaces = [pliron::builtin::op_interfaces::OneOpdInterface, pliron::builtin::op_interfaces::OneResultInterface]
)]
pub struct PromiseRejectedOp;

impl PromiseRejectedOp {
    pub fn new(ctx: &mut Context, reason: Value, settled: pliron::r#type::TypeHandle) -> Self {
        let result = crate::types::PromiseType::get(ctx, settled).into();
        PromiseRejectedOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![result],
                vec![reason],
                vec![],
                0,
            ),
        }
    }
}

impl Verify for PromiseRejectedOp {
    fn verify(&self, _ctx: &Context) -> Result<()> {
        Ok(())
    }
}

impl Verify for PromiseResolvedOp {
    fn verify(&self, _ctx: &Context) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Error)]
#[error("perry.settled_field reads a perry.settled value")]
pub(crate) struct SettledErr;

/// A property of a `Promise.allSettled` result element.
#[pliron::derive::pliron_attr(name = "perry.settled_field", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum SettledField {
    /// `"fulfilled"` or `"rejected"`.
    Status,
    /// The fulfillment value, or `undefined`.
    Value,
    /// The rejection (an exception), or `undefined`.
    Reason,
}

/// `settled.status`, `settled.value` or `settled.reason`.
#[pliron_op(
    name = "perry.settled_field",
    format = "attr($perry_settled_field, $SettledField) ` ` $0 ` : ` type($0)",
    interfaces = [pliron::builtin::op_interfaces::OneOpdInterface, pliron::builtin::op_interfaces::OneResultInterface],
    attributes = (perry_settled_field: SettledField)
)]
pub struct SettledFieldOp;

impl SettledFieldOp {
    /// `None` unless `settled` is a `perry.settled` value.
    pub fn new(ctx: &mut Context, field: SettledField, settled: Value) -> Option<Self> {
        use crate::types::SettledType;
        let value =
            pliron::r#type::TypedHandle::<SettledType>::from_handle(settled.get_type(ctx), ctx)
                .ok()?
                .deref(ctx)
                .value();
        let result = match field {
            SettledField::Status => StringType::get(ctx).into(),
            SettledField::Value => OptionalType::get(ctx, value).into(),
            SettledField::Reason => {
                let exception = ExceptionType::get(ctx).into();
                OptionalType::get(ctx, exception).into()
            }
        };
        let op = SettledFieldOp {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![result],
                vec![settled],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_settled_field(ctx, field);
        Some(op)
    }

    pub fn field(&self, ctx: &Context) -> SettledField {
        *self.get_attr_perry_settled_field(ctx).expect("field")
    }
}

impl Verify for SettledFieldOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let operand = self.get_operation().deref(ctx).get_operand(0).get_type(ctx);
        if !operand.deref(ctx).is::<crate::types::SettledType>() {
            verify_err!(self.loc(ctx), SettledErr)?
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for SettledFieldOp {
    fn has_side_effects(&self, _ctx: &Context) -> bool {
        false
    }
}
