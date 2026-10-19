//! Logical value families and their opaque MIR types.
use super::{ArrayType, RecordType, ResourceType, SettledType, VariantType};
use pliron::{
    builtin::types::{FP64Type, IntegerType},
    context::Context,
    derive::pliron_type,
    r#type::TypeHandle,
};

macro_rules! value_types {
    (
        opaque { $($(#[$doc:meta])* $kind:ident => $ty:ident($name:literal, $label:literal, [$($alias:literal),*]);)* }
        internal { $($(#[$internal_doc:meta])* $internal:ident => $internal_name:literal;)* }
        structural { $($structural:ident => $structure:ident, $structural_label:literal;)* }
    ) => {
        $(
            $(#[$doc])*
            #[pliron_type(name = $name, generate_get = true, format, verifier = "succ")]
            #[derive(Hash, PartialEq, Eq, Debug)]
            pub struct $ty;
        )*
        $(
            $(#[$internal_doc])*
            #[pliron_type(name = $internal_name, generate_get = true, format, verifier = "succ")]
            #[derive(Hash, PartialEq, Eq, Debug)]
            pub struct $internal;
        )*

        /// Source-level families whose values participate in ECMAScript conversions.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum ValueKind { Number, Boolean, $($kind,)* $($structural,)* }

        impl ValueKind {
            pub const fn label(self) -> &'static str {
                match self {
                    Self::Number => "number",
                    Self::Boolean => "boolean",
                    $(Self::$kind => $label,)*
                    $(Self::$structural => $structural_label,)*
                }
            }

            pub fn of(ctx: &Context, ty: TypeHandle) -> Option<Self> {
                let ty = ty.deref(ctx);
                if ty.is::<FP64Type>() {
                    return Some(Self::Number);
                }
                if let Some(integer) = ty.downcast_ref::<IntegerType>() {
                    return match integer.width() {
                        1 => Some(Self::Boolean),
                        64 => Some(Self::BigInt),
                        _ => None,
                    };
                }
                $(if ty.is::<$ty>() { return Some(Self::$kind); })*
                $(if ty.is::<$structure>() { return Some(Self::$structural); })*
                None
            }

            pub fn builtin_type(ctx: &Context, name: &str) -> Option<TypeHandle> {
                match name {
                    $($($alias => Some($ty::get(ctx).into()),)*)*
                    _ => None,
                }
            }
        }
    };
}

value_types! {
    opaque {
        /// An immutable, validated UTF-8 string indexed by Unicode scalar values.
        String => StringType("perry.string", "string", []);
        /// Immutable sign and 64-bit magnitude, preserving every WIT s64/u64 value.
        BigInt => BigIntType("perry.bigint", "bigint", []);
        /// A `Uint8Array`: a view `{ length, data }` over bytes in the task region; `subarray`
        /// views share their bytes.
        Bytes => BytesType("perry.bytes", "Uint8Array", ["Uint8Array"]);

        Url => UrlType("perry.url", "URL", ["URL"]);

        SearchParams => SearchParamsType("perry.search_params", "URLSearchParams", ["URLSearchParams"]);
        /// A parsed JSON value (`JSON.parse`): a node of the JSON helper's graph, or `undefined`.
        /// Untyped data is a JSON tree, never `any` (subset rule R6).
        Json => JsonType("perry.json", "JSON value", []);
        /// A mutable Date object containing epoch milliseconds or NaN.
        Date => DateType("perry.date", "Date", ["Date"]);
        /// `fs.Stats` of `stat()`: the entry type and size, read through `perry.intrinsic`.
        Stats => StatsType("perry.stats", "Stats", []);
        /// A Fetch `Headers` list.
        Headers => HeadersType("perry.headers", "Headers", ["Headers"]);
        /// A Fetch `Response`.
        Response => ResponseType("perry.response", "Response", ["Response"]);
        /// A Fetch `Request`.
        Request => RequestType("perry.request", "Request", ["Request"]);
        /// A Web `WritableStream` over a console output (`Writable.toWeb(process.stdout)`).
        Writable => WritableType("perry.writable", "WritableStream", ["WritableStream"]);
        /// A `WritableStreamDefaultWriter`.
        Writer => WriterType("perry.writer", "WritableStreamDefaultWriter", ["WritableStreamDefaultWriter"]);
        /// A `ReadableStream<Uint8Array>` over a WIT `stream<u8>`.
        Readable => ReadableType("perry.readable", "ReadableStream", ["ReadableStream"]);
        /// A detachable `ArrayBuffer` backing mutable byte views.
        ByteBuffer => ByteBufferType("perry.byte_buffer", "ArrayBuffer", ["ArrayBuffer"]);
        /// A `ReadableStreamBYOBReader`.
        ByobReader => ByobReaderType("perry.byob_reader", "ReadableStreamBYOBReader", ["ReadableStreamBYOBReader"]);
        /// A `ReadableStreamDefaultReader`.
        Reader => ReaderType("perry.reader", "ReadableStreamDefaultReader", ["ReadableStreamDefaultReader"]);
        /// A reader's `{ done, value }`.
        ReadResult => ReadResultType("perry.read_result", "ReadableStreamReadResult", ["ReadableStreamReadResult"]);
        /// A `Temporal.Instant`: 16 opaque bytes, little-endian `i128` epoch nanoseconds
        /// (the `time-helper` codec).
        Instant => InstantType("perry.instant", "Temporal.Instant", ["Temporal.Instant"]);
        /// A `Temporal.PlainDate`: 16 opaque bytes, an `i32` epoch day and zero nanoseconds.
        PlainDate => PlainDateType("perry.plain_date", "Temporal.PlainDate", ["Temporal.PlainDate"]);
        /// A `Temporal.PlainDateTime`: 16 opaque bytes, an `i32` epoch day and the `u64`
        /// nanosecond of that day.
        PlainDateTime => PlainDateTimeType("perry.plain_date_time", "Temporal.PlainDateTime", ["Temporal.PlainDateTime"]);
        /// An `AbortController` and its `AbortSignal` (one object): aborting rejects the timers that
        /// watch it.
        Abort => AbortType("perry.abort", "AbortController", ["AbortController","AbortSignal"]);
        /// An HTTP request a handler serves (`wasi:http/handler`): its parts, its body and the
        /// response being built.
        Incoming => IncomingType("perry.incoming", "IncomingRequest", []);
        /// A caught exception (`catch (e)`): an `Error` object or a boxed thrown value. Lives in the
        /// task like every other value (R3, R4).
        Exception => ExceptionType("perry.exception", "Error", ["Error","TypeError","RangeError","ReferenceError","SyntaxError"]);
        /// Stateful UTF-8 decoder with at most three pending bytes.
        Decoder => DecoderType("perry.decoder", "TextDecoder", ["TextDecoder"]);
        /// Immutable, statically compiled regex program; no mutable lastIndex state.
        Regex => RegexType("perry.regex", "RegExp", ["RegExp"]);
        /// The finite primitive union `number | string`, with an exact owner slot for its string case.
        NumberString => NumberStringType("perry.number_string", "number | string", []);
    }
    internal {
        /// A numeric value retaining an exact signed/unsigned 64-bit integer or IEEE double.
        /// Converting a narrower integer into this type explicitly widens its domain.
        NumericType => "perry.numeric";
        /// Read-only outbound WIT byte storage, accepting either UTF-8 strings or mutable bytes.
        ByteInputType => "perry.byte_input";
        /// Canonical result area and argument owners of a suspended host call.
        TransferType => "perry.transfer";
    }
    structural {
        Array => ArrayType, "array";
        Record => RecordType, "record";
        Variant => VariantType, "variant";
        Resource => ResourceType, "WIT resource";
        Settled => SettledType, "PromiseSettlementResult";
    }
}
