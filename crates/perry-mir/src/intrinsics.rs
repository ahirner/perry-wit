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
    types::{ArrayType, BytesType, DateType, JsonType, OptionalType, StringType},
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
}

/// `Promise.all(promises)` and `Promise.race(promises)` over `perry.array<perry.promise<T>>`.
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
