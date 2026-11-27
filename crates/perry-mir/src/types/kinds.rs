//! MIR value families, builtin names, and reference representations.
use super::{ArrayType, RecordType, ResourceType, SettledType, ValueUnionType, VariantType};
use pliron::{
    builtin::types::{FP64Type, IntegerType},
    context::Context,
    derive::pliron_type,
    r#type::TypeHandle,
};

macro_rules! value_types {
    (
        opaque { $($(#[$doc:meta])* $variant:ident $label:literal => $ty:ident($name:literal, [$($alias:literal),*]);)* }
        internal { $($(#[$internal_doc:meta])* $internal:ident => $internal_name:literal;)* }
        structural { $($family:ident $description:literal => $structure:ident;)* }
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

        /// Language-level value family of a concrete MIR type.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum ValueKind { Number, Boolean, $($variant,)* $($family,)* }

        impl ValueKind {
            pub fn of(ctx: &Context, ty: TypeHandle) -> Option<Self> {
                let ty = ty.deref(ctx);
                if ty.is::<FP64Type>() { return Some(Self::Number); }
                if let Some(integer) = ty.downcast_ref::<IntegerType>() {
                    return match integer.width() {
                        1 => Some(Self::Boolean),
                        64 => Some(Self::BigInt),
                        _ => None,
                    };
                }
                $(if ty.is::<$ty>() { return Some(Self::$variant); })*
                $(if ty.is::<$structure>() { return Some(Self::$family); })*
                None
            }

            pub const fn label(self) -> &'static str {
                match self {
                    Self::Number => "number",
                    Self::Boolean => "boolean",
                    $(Self::$variant => $label,)*
                    $(Self::$family => $description,)*
                }
            }

            pub fn builtin(name: &str) -> Option<Self> {
                match name { $($($alias => Some(Self::$variant),)*)* _ => None }
            }
        }

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
        String "string" => StringType("perry.string", []);
        /// Immutable arbitrary-precision signed integer, preserving every WIT s64/u64 value.
        BigInt "bigint" => BigIntType("perry.bigint", []);
        /// A `Uint8Array` view `{ length, data }` over bytes in the task region. Views created
        /// with `subarray` share the underlying byte buffer.
        Bytes "Uint8Array" => BytesType("perry.bytes", ["Uint8Array"]);

        Url "URL" => UrlType("perry.url", ["URL"]);

        SearchParams "URLSearchParams" => SearchParamsType("perry.search_params", ["URLSearchParams"]);
        /// A parsed JSON value (`JSON.parse`): a node of the JSON codec's graph, or `undefined`.
        Json "JSON value" => JsonType("perry.json", []);
        /// A mutable Date object containing epoch milliseconds or NaN.
        Date "Date" => DateType("perry.date", ["Date"]);
        /// `fs.Stats` of `stat()`: the entry type and size, read through `perry.intrinsic`.
        Stats "Stats" => StatsType("perry.stats", ["Stats", "fs.Stats"]);
        /// A Fetch `Headers` list.
        Headers "Headers" => HeadersType("perry.headers", ["Headers"]);
        /// A Fetch `Response`.
        Response "Response" => ResponseType("perry.response", ["Response"]);
        /// A Fetch `Request`.
        Request "Request" => RequestType("perry.request", ["Request"]);
        /// A Web `WritableStream` over a console output (`Writable.toWeb(process.stdout)`).
        Writable "WritableStream" => WritableType("perry.writable", ["WritableStream"]);
        /// A `WritableStreamDefaultWriter`.
        Writer "WritableStreamDefaultWriter" => WriterType("perry.writer", ["WritableStreamDefaultWriter"]);
        /// A `ReadableStream<Uint8Array>` over a WIT `stream<u8>`.
        Readable "ReadableStream" => ReadableType("perry.readable", ["ReadableStream"]);
        /// A detachable `ArrayBuffer` backing mutable byte views.
        ByteBuffer "ArrayBuffer" => ByteBufferType("perry.byte_buffer", ["ArrayBuffer"]);
        /// An offset and extent over a detachable byte buffer.
        DataView "DataView" => DataViewType("perry.data_view", ["DataView"]);
        /// A `ReadableStreamBYOBReader`.
        ByobReader "ReadableStreamBYOBReader" => ByobReaderType("perry.byob_reader", ["ReadableStreamBYOBReader"]);
        /// A `ReadableStreamDefaultReader`.
        Reader "ReadableStreamDefaultReader" => ReaderType("perry.reader", ["ReadableStreamDefaultReader"]);
        /// A reader's `{ done, value }`.
        ReadResult "ReadableStreamReadResult" => ReadResultType("perry.read_result", ["ReadableStreamReadResult"]);
        /// A `Temporal.Instant`: 16 opaque bytes, little-endian `i128` epoch nanoseconds
        /// (the `perry-time` codec).
        Instant "Temporal.Instant" => InstantType("perry.instant", ["Temporal.Instant"]);
        /// A `Temporal.PlainDate`: 16 opaque bytes, an `i32` epoch day and zero nanoseconds.
        PlainDate "Temporal.PlainDate" => PlainDateType("perry.plain_date", ["Temporal.PlainDate"]);
        /// A `Temporal.PlainDateTime`: 16 opaque bytes, an `i32` epoch day and the `u64`
        /// nanosecond of that day.
        PlainDateTime "Temporal.PlainDateTime" => PlainDateTimeType("perry.plain_date_time", ["Temporal.PlainDateTime"]);
        /// An `AbortController` and its `AbortSignal` (one object): aborting rejects the timers that
        /// watch it.
        Abort "AbortController" => AbortType("perry.abort", ["AbortController","AbortSignal"]);
        /// An HTTP request a handler serves (`wasi:http/handler`): its parts, its body and the
        /// response being built.
        Incoming "IncomingRequest" => IncomingType("perry.incoming", ["__PerryIncoming"]);
        /// A caught exception (`catch (e)`): an `Error` object or a boxed thrown value.
        Exception "Error" => ExceptionType("perry.exception", ["Error","TypeError","RangeError","ReferenceError","SyntaxError"]);
        /// A `TextEncoder`: stateless, so every instance is the same static UTF-8 encoder.
        Encoder "TextEncoder" => EncoderType("perry.encoder", ["TextEncoder"]);
        /// Stateful UTF-8 decoder with at most three pending bytes.
        Decoder "TextDecoder" => DecoderType("perry.decoder", ["TextDecoder"]);
        /// Immutable, statically compiled regular expression program.
        Regex "RegExp" => RegexType("perry.regex", ["RegExp"]);
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
        Array "array" => ArrayType;
        Record "record" => RecordType;
        Variant "variant" => VariantType;
        ValueUnion "finite value union" => ValueUnionType;
        Resource "WIT resource" => ResourceType;
        Settled "PromiseSettlementResult" => SettledType;
    }
}
