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
        ArrayType, BytesType, DateType, ExceptionType, HeadersType, JsonType, OptionalType,
        PromiseType, ReadResultType, ReadableType, ReaderType, RequestType, ResponseType,
        StatsType, StringType, WritableType, WriterType,
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
    Number,
    Bool,
    String,
    OptString,
    OptNumber,
    StringArray,
    NumberArray,
    Bytes,
    Json,
    Date,
    Exception,
    Stats,
    PromiseString,
    PromiseBytes,
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
    OptReadable,
    PromiseResponse,
}

impl Ty {
    pub fn get(self, ctx: &mut Context) -> TypeHandle {
        let string: TypeHandle = StringType::get(ctx).into();
        let number: TypeHandle = FP64Type::get(ctx).into();
        match self {
            Ty::Number => number,
            Ty::Bool => IntegerType::get(ctx, 1, Signedness::Signless).into(),
            Ty::String => string,
            Ty::OptString => OptionalType::get(ctx, string).into(),
            Ty::OptNumber => OptionalType::get(ctx, number).into(),
            Ty::StringArray => ArrayType::get(ctx, string).into(),
            Ty::NumberArray => ArrayType::get(ctx, number).into(),
            Ty::Bytes => BytesType::get(ctx).into(),
            Ty::Json => JsonType::get(ctx).into(),
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
            Ty::PromiseReadResult => {
                let result = ReadResultType::get(ctx).into();
                PromiseType::get(ctx, result).into()
            }
            Ty::OptReadable => {
                let stream = ReadableType::get(ctx).into();
                OptionalType::get(ctx, stream).into()
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
            Ty::Number => deref.is::<FP64Type>(),
            Ty::Bool => deref
                .downcast_ref::<IntegerType>()
                .is_some_and(|i| i.width() == 1),
            Ty::String => deref.is::<StringType>(),
            Ty::OptString => optional_of(&string),
            Ty::OptNumber => optional_of(&number),
            Ty::StringArray => array_of(&string),
            Ty::NumberArray => array_of(&number),
            Ty::Bytes => deref.is::<BytesType>(),
            Ty::Json => deref.is::<JsonType>(),
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
            Ty::Reader => deref.is::<ReaderType>(),
            Ty::ReadResult => deref.is::<ReadResultType>(),
            Ty::PromiseReadResult => promise_of(&|t| t.deref(ctx).is::<ReadResultType>()),
            Ty::OptReadable => optional_of(&|t| t.deref(ctx).is::<ReadableType>()),
            Ty::PromiseResponse => promise_of(&|t| t.deref(ctx).is::<ResponseType>()),
            Ty::PromiseString => promise_of(&string),
            Ty::PromiseBytes => promise_of(&|t| t.deref(ctx).is::<BytesType>()),
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
    PrintOut = "print_out" (String) -> ();
    PrintErr = "print_err" (String) -> ();
    NumberToString = "number_to_string" (Number) -> (String), pure;
    EnvGet = "env_get" (String) -> (OptString);
    EnvKeys = "env_keys" () -> (StringArray);
    PerformanceNow = "performance_now" () -> (Number);
    DateNow = "date_now" () -> (Number);
    ExitCodeSet = "exit_code_set" (Number) -> ();
    MathFloor = "math_floor" (Number) -> (Number), pure;
    MathCeil = "math_ceil" (Number) -> (Number), pure;
    MathTrunc = "math_trunc" (Number) -> (Number), pure;
    MathAbs = "math_abs" (Number) -> (Number), pure;
    MathRound = "math_round" (Number) -> (Number), pure;
    MathSqrt = "math_sqrt" (Number) -> (Number), pure;
    MathMin = "math_min" (Number, Number) -> (Number), pure;
    MathMax = "math_max" (Number, Number) -> (Number), pure;
    DateFromTime = "date_from_time" (Number) -> (Date), pure;
    DateTime = "date_time" (Date) -> (Number), pure;
    DateIso = "date_iso" (Date) -> (String), throws;
    StringCharAt = "string_char_at" (String, Number) -> (String), pure;
    StringCharCodeAt = "string_char_code_at" (String, Number) -> (Number), pure;
    StringSlice = "string_slice" (String, Number, Number) -> (String), pure;
    StringSubstring = "string_substring" (String, Number, Number) -> (String), pure;
    StringIndexOf = "string_index_of" (String, String, Number) -> (Number), pure;
    StringLastIndexOf = "string_last_index_of" (String, String, Number) -> (Number), pure;
    StringStartsWith = "string_starts_with" (String, String, Number) -> (Bool), pure;
    StringEndsWith = "string_ends_with" (String, String, Number) -> (Bool), pure;
    StringTrim = "string_trim" (String) -> (String), pure;
    StringLower = "string_lower" (String) -> (String), pure;
    StringUpper = "string_upper" (String) -> (String), pure;
    StringRepeat = "string_repeat" (String, Number) -> (String), throws;
    BytesNew = "bytes_new" (Number) -> (Bytes), throws;
    BytesFromArray = "bytes_from_array" (NumberArray) -> (Bytes);
    BytesCopy = "bytes_copy" (Bytes) -> (Bytes);
    BytesGet = "bytes_get" (Bytes, Number) -> (OptNumber);
    BytesSet = "bytes_set" (Bytes, Number, Number) -> ();
    BytesLength = "bytes_length" (Bytes) -> (Number), pure;
    BytesSubarray = "bytes_subarray" (Bytes, Number, Number) -> (Bytes), pure;
    BytesSlice = "bytes_slice" (Bytes, Number, Number) -> (Bytes);
    BytesSetFrom = "bytes_set_from" (Bytes, Bytes, Number) -> (), throws;
    BytesFill = "bytes_fill" (Bytes, Number, Number, Number) -> ();
    TextEncode = "text_encode" (String) -> (Bytes);
    TextDecode = "text_decode" (Bytes, Bool) -> (String), throws;
    JsonParse = "json_parse" (String) -> (Json), throws;
    JsonMember = "json_member" (Json, String) -> (Json), throws;
    JsonIndex = "json_index" (Json, Number) -> (Json), throws;
    JsonLength = "json_length" (Json) -> (OptNumber), throws;
    JsonToNumber = "json_to_number" (Json) -> (Number), throws;
    JsonToString = "json_to_string" (Json) -> (String), throws;
    JsonToBool = "json_to_bool" (Json) -> (Bool), throws;
    JsonIsUndefined = "json_is_undefined" (Json) -> (Bool), pure;
    JsonSerialize = "json_serialize" (Json) -> (OptString), throws;
    JsonTypeOf = "json_type_of" (Json) -> (String), pure;
    MathRandom = "math_random" () -> (Number);
    RandomUuid = "random_uuid" () -> (String);
    BytesFillRandom = "bytes_fill_random" (Bytes) -> (Bytes);
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
    StatsIsFile = "stats_is_file" (Stats) -> (Bool), pure;
    StatsIsDirectory = "stats_is_directory" (Stats) -> (Bool), pure;
    StatsSize = "stats_size" (Stats) -> (Number), pure;
    HeadersNew = "headers_new" () -> (Headers);
    HeadersCopy = "headers_copy" (Headers) -> (Headers);
    HeadersAppend = "headers_append" (Headers, String, String) -> (), throws;
    HeadersSet = "headers_set" (Headers, String, String) -> (), throws;
    HeadersGet = "headers_get" (Headers, String) -> (OptString), throws;
    HeadersHas = "headers_has" (Headers, String) -> (Bool), throws;
    HeadersDelete = "headers_delete" (Headers, String) -> (), throws;
    ResponseNew = "response_new" (OptBytes, Bool, Number, String, Headers) -> (Response), throws;
    ResponseStatus = "response_status" (Response) -> (Number), pure;
    ResponseStatusText = "response_status_text" (Response) -> (String), pure;
    ResponseOk = "response_ok" (Response) -> (Bool), pure;
    ResponseHeaders = "response_headers" (Response) -> (Headers), pure;
    ResponseBodyUsed = "response_body_used" (Response) -> (Bool);
    ResponseBytes = "response_bytes" (Response) -> (PromiseBytes);
    ResponseText = "response_text" (Response) -> (PromiseString);
    RequestNew = "request_new" (String, String, OptBytes, Bool, Headers) -> (Request), throws;
    RequestMethod = "request_method" (Request) -> (String), pure;
    RequestUrl = "request_url" (Request) -> (String), pure;
    RequestHeaders = "request_headers" (Request) -> (Headers), pure;
    RequestBodyUsed = "request_body_used" (Request) -> (Bool);
    RequestBytes = "request_bytes" (Request) -> (PromiseBytes);
    RequestText = "request_text" (Request) -> (PromiseString);
    WritableStdout = "writable_stdout" () -> (Writable);
    WritableStderr = "writable_stderr" () -> (Writable);
    WritableLocked = "writable_locked" (Writable) -> (Bool);
    WritableGetWriter = "writable_get_writer" (Writable) -> (Writer), throws;
    WriterWrite = "writer_write" (Writer, Bytes) -> (PromiseVoid);
    WriterClose = "writer_close" (Writer) -> (PromiseVoid);
    WriterReleaseLock = "writer_release_lock" (Writer) -> ();
    ReadableLocked = "readable_locked" (Readable) -> (Bool);
    ReadableGetReader = "readable_get_reader" (Readable) -> (Reader), throws;
    ReadableCancel = "readable_cancel" (Readable) -> (PromiseVoid);
    ReaderRead = "reader_read" (Reader) -> (PromiseReadResult);
    ReaderCancel = "reader_cancel" (Reader) -> (PromiseVoid);
    ReaderReleaseLock = "reader_release_lock" (Reader) -> ();
    ReadDone = "read_done" (ReadResult) -> (Bool), pure;
    ReadValue = "read_value" (ReadResult) -> (OptBytes), pure;
    Fetch = "fetch" (String, String, Headers, OptBytes, Bool) -> (PromiseResponse);
    ResponseBody = "response_body" (Response) -> (OptReadable);
    RequestBody = "request_body" (Request) -> (OptReadable);
}

impl Intrinsic {
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|i| i.name() == name)
    }

    /// (operands, results) as MIR types.
    pub fn signature(self, ctx: &mut Context) -> (Vec<TypeHandle>, Vec<TypeHandle>) {
        let (operands, results) = self.types();
        (
            operands.iter().map(|t| t.get(ctx)).collect(),
            results.iter().map(|t| t.get(ctx)).collect(),
        )
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
pub enum IntrinsicErr {
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
        let (_, results) = intrinsic.signature(ctx);
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
pub struct StringifyErr;

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
        false
    }
}

#[derive(Debug, Error)]
#[error("perry.promise_combinator takes an array of promises of one type")]
pub struct CombinatorErr;

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

#[derive(Debug, Error)]
#[error("perry.settled_field reads a perry.settled value")]
pub struct SettledErr;

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
            SettledField::Value if value.deref(ctx).is::<OptionalType>() => value,
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
