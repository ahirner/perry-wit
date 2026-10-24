//! Opaque MIR types and reference-representation queries.
use super::{ArrayType, RecordType, ResourceType, SettledType, VariantType};
use pliron::{context::Context, derive::pliron_type, r#type::TypeHandle};

macro_rules! value_types {
    (
        opaque { $($(#[$doc:meta])* $ty:ident($name:literal, [$($alias:literal),*]);)* }
        internal { $($(#[$internal_doc:meta])* $internal:ident => $internal_name:literal;)* }
        structural { $($structure:ident;)* }
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

        /// Whether a MIR value has an opaque or structural reference representation.
        pub fn is_reference_type(ctx: &Context, ty: TypeHandle) -> bool {
            let ty = ty.deref(ctx);
            $(ty.is::<$ty>() ||)* $(ty.is::<$structure>() ||)* false
        }

        pub fn builtin_value_type(ctx: &Context, name: &str) -> Option<TypeHandle> {
            match name {
                $($($alias => Some($ty::get(ctx).into()),)*)*
                _ => None,
            }
        }
    };
}

value_types! {
    opaque {
        /// An immutable, validated UTF-8 string indexed by Unicode scalar values.
        StringType("perry.string", []);
        /// Immutable arbitrary-precision signed integer, preserving every WIT s64/u64 value.
        BigIntType("perry.bigint", []);
        /// A `Uint8Array` view `{ length, data }` over bytes in the task region. Views created
        /// with `subarray` share the underlying byte buffer.
        BytesType("perry.bytes", ["Uint8Array"]);

        UrlType("perry.url", ["URL"]);

        SearchParamsType("perry.search_params", ["URLSearchParams"]);
        /// A parsed JSON value (`JSON.parse`): a node of the JSON helper's graph, or `undefined`.
        JsonType("perry.json", []);
        /// A mutable Date object containing epoch milliseconds or NaN.
        DateType("perry.date", ["Date"]);
        /// `fs.Stats` of `stat()`: the entry type and size, read through `perry.intrinsic`.
        StatsType("perry.stats", []);
        /// A Fetch `Headers` list.
        HeadersType("perry.headers", ["Headers"]);
        /// A Fetch `Response`.
        ResponseType("perry.response", ["Response"]);
        /// A Fetch `Request`.
        RequestType("perry.request", ["Request"]);
        /// A Web `WritableStream` over a console output (`Writable.toWeb(process.stdout)`).
        WritableType("perry.writable", ["WritableStream"]);
        /// A `WritableStreamDefaultWriter`.
        WriterType("perry.writer", ["WritableStreamDefaultWriter"]);
        /// A `ReadableStream<Uint8Array>` over a WIT `stream<u8>`.
        ReadableType("perry.readable", ["ReadableStream"]);
        /// A detachable `ArrayBuffer` backing mutable byte views.
        ByteBufferType("perry.byte_buffer", ["ArrayBuffer"]);
        /// A `ReadableStreamBYOBReader`.
        ByobReaderType("perry.byob_reader", ["ReadableStreamBYOBReader"]);
        /// A `ReadableStreamDefaultReader`.
        ReaderType("perry.reader", ["ReadableStreamDefaultReader"]);
        /// A reader's `{ done, value }`.
        ReadResultType("perry.read_result", ["ReadableStreamReadResult"]);
        /// A `Temporal.Instant`: 16 opaque bytes, little-endian `i128` epoch nanoseconds
        /// (the `perry-time` codec).
        InstantType("perry.instant", ["Temporal.Instant"]);
        /// A `Temporal.PlainDate`: 16 opaque bytes, an `i32` epoch day and zero nanoseconds.
        PlainDateType("perry.plain_date", ["Temporal.PlainDate"]);
        /// A `Temporal.PlainDateTime`: 16 opaque bytes, an `i32` epoch day and the `u64`
        /// nanosecond of that day.
        PlainDateTimeType("perry.plain_date_time", ["Temporal.PlainDateTime"]);
        /// An `AbortController` and its `AbortSignal` (one object): aborting rejects the timers that
        /// watch it.
        AbortType("perry.abort", ["AbortController","AbortSignal"]);
        /// An HTTP request a handler serves (`wasi:http/handler`): its parts, its body and the
        /// response being built.
        IncomingType("perry.incoming", []);
        /// A caught exception (`catch (e)`): an `Error` object or a boxed thrown value.
        ExceptionType("perry.exception", ["Error","TypeError","RangeError","ReferenceError","SyntaxError"]);
        /// A `TextEncoder`: stateless, so every instance is the same static UTF-8 encoder.
        EncoderType("perry.encoder", ["TextEncoder"]);
        /// Stateful UTF-8 decoder with at most three pending bytes.
        DecoderType("perry.decoder", ["TextDecoder"]);
        /// Immutable, statically compiled regular expression program.
        RegexType("perry.regex", ["RegExp"]);
        /// The finite primitive union `number | string`, with an exact owner slot for its string case.
        NumberStringType("perry.number_string", []);
    }
    internal {
        /// Suspended JSON serialization between replacer calls.
        JsonTraversalType => "perry.json_traversal";
        /// A numeric value retaining an exact signed/unsigned 64-bit integer or IEEE double.
        /// Converting a narrower integer into this type explicitly widens its domain.
        NumericType => "perry.numeric";
        /// Read-only outbound WIT byte storage, accepting either UTF-8 strings or mutable bytes.
        ByteInputType => "perry.byte_input";
        /// Canonical result area and argument owners of a suspended host call.
        TransferType => "perry.transfer";
    }
    structural {
        ArrayType;
        RecordType;
        VariantType;
        ResourceType;
        SettledType;
    }
}
