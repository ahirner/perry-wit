//! Host and runtime services with fixed signatures (`perry.intrinsic`).
//!
//! The builder recognizes the standard API (`console.log`, `Math.floor`, string methods,
//! `Uint8Array`, `JSON`, `Date`, and related built-ins) and emits one op per service, named by
//! [Intrinsic]. Codegen maps each intrinsic to Wasm instructions or a runtime function and
//! verifies that the WIT world imports the required host interfaces. Services that may throw
//! report their exception contract through [Intrinsic::throws] and [may_throw].
use crate::operation_schema::op_schema;
use crate::signatures::{Accepts, OutputContract, ResultType, Signature, Ty, TypePattern};
use crate::types::{ArrayType, ExceptionType, JsonType, OptionalType, StringType};
use pliron::{
    builtin::types::IntegerType,
    common_traits::Verify,
    context::{Context, Ptr},
    derive::pliron_attr,
    op::Op,
    operation::Operation,
    result::Result,
    r#type::{TypeHandle, Typed},
    value::Value,
};

mod catalog;
mod promises;
pub use promises::{
    Combinator, PromiseCombinatorOp, PromiseRejectedOp, PromiseResolvedOp, SettledField,
    SettledFieldOp,
};

/// Operand kinds: a value kind, or an aggregate kind that names no single MIR type and so
/// cannot be a result ([Ty] has no such kind).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Operand {
    Value(Ty),
    /// Records and tagged unions retaining their original JSON graph.
    Aggregate,
    Reference,
}

impl TypePattern for Operand {
    fn accepts_type(self, ctx: &Context, ty: TypeHandle) -> bool {
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServiceEffect {
    Pure,
    Effectful,
    Throws,
}

pub(crate) struct IntrinsicPolicy {
    pub effect: ServiceEffect,
    pub allocation: crate::effects::Allocation,
    pub numeric: Option<BuiltinContract>,
}

/// Derive MIR declarations from the shared catalog, ignoring explicit runtime ABI rows.
macro_rules! intrinsics {
    (
        inline { $($inline:tt)* }
        runtime {
            $($runtime:ident = $export:literal
                $(($($param:ident),*))? $([$($intrinsic:tt)*])? $(-> $carrier:ident)?;)*
        }
    ) => {
        intrinsics!(@declare $($inline)* $($($($intrinsic)*)?)*);
    };
    (@declare $($variant:ident = $name:literal ($($operand:ident),*) -> ($($result:ident),*) , $effect:ident, $allocation:ident $(=> $contract:ident)?;)*) => {
        /// Built-in runtime and host services, each named after the API it implements.
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

            pub(crate) fn policy(self) -> IntrinsicPolicy {
                match self {
                    $(Intrinsic::$variant => IntrinsicPolicy {
                        effect: ServiceEffect::$effect,
                        allocation: crate::effects::Allocation::$allocation,
                        numeric: None $(.or(Some(BuiltinContract::$contract)))?,
                    },)*
                }
            }

        }
    };
}

crate::intrinsic_catalog!(intrinsics);

impl Intrinsic {
    /// Whether the service may throw (as an ECMAScript exception).
    pub fn throws(self) -> bool {
        self.policy().effect == ServiceEffect::Throws
    }

    /// Whether the service produces a value (all services have at most one result).
    pub fn has_result(self) -> bool {
        !self.types().1.is_empty()
    }

    /// `Math.floor`, `ceil`, `trunc`, `round`, `abs`, `min` and `max`: an integer (or lossless
    /// numeric) operand type carries over to the result.
    pub(crate) fn preserves_integers(self) -> bool {
        matches!(
            self.policy().numeric,
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

/// Whether the operation contract permits throwing, including the selected intrinsic.
pub fn may_throw(ctx: &Context, op: Ptr<Operation>) -> bool {
    if let Some(intrinsic) = Operation::get_op::<IntrinsicOp>(op, ctx) {
        return intrinsic.intrinsic(ctx).throws();
    }
    crate::semantics::exception(ctx, op).is_some()
}

#[derive(Debug)]
pub(super) enum IntrinsicErr {
    Missing,
    Signature(String),
    Stringify,
    Combinator,
    PromiseResult(&'static str),
    Settled,
}

impl core::fmt::Display for IntrinsicErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Missing => f.write_str("missing intrinsic attribute"),
            Self::Signature(name) => write!(f, "intrinsic {name} has the wrong operand or result types"),
            Self::Stringify => f.write_str(
                "perry.json_stringify produces a string, or string | undefined for an optional value",
            ),
            Self::Combinator => {
                f.write_str("perry.promise_combinator takes an array of promises of one type")
            }
            Self::PromiseResult(op) => write!(f, "{op} has an invalid promise result type"),
            Self::Settled => f.write_str("perry.settled_field reads a perry.settled value"),
        }
    }
}

impl core::error::Error for IntrinsicErr {}

op_schema! {
    /// A call of a host or runtime service.
    pub IntrinsicOp(.._args) -> infallible;
    name = "perry.intrinsic";
    format = "attr($perry_intrinsic, $Intrinsic) ` (` operands(CharSpace(`,`)) `) : ` types(CharSpace(`,`))";
    attributes = (perry_intrinsic: Intrinsic);
    constructors {
        pub new(intrinsic: Intrinsic, args: Vec<Value>) operands(args)
            => results(intrinsic.types().1.iter().map(|ty| ty.get(ctx)).collect());
    }
    attribute(intrinsic: Intrinsic, get_attr_perry_intrinsic, set_attr_perry_intrinsic) =
        intrinsic => IntrinsicErr::Missing;
    prepare(_this) { let intrinsic = *intrinsic; }
    result(ctx, intrinsic: Intrinsic): Intrinsic = intrinsic;
    error = IntrinsicErr::Signature(intrinsic.name().into());
}

impl OutputContract for Intrinsic {
    fn accepts(self, ctx: &Context, op: &Operation) -> bool {
        let (inputs, outputs) = self.types();
        if (Signature {
            inputs: inputs.iter().copied(),
            outputs: outputs.iter().copied(),
        })
        .accepts(ctx, op)
        {
            return true;
        }
        let Some(result) = crate::signatures::result_types(ctx, op).next() else {
            return false;
        };
        self.preserves_integers()
            && (result.deref(ctx).is::<IntegerType>()
                || result.deref(ctx).is::<crate::types::NumericType>())
            && (Signature {
                inputs: std::iter::repeat_n(result, inputs.len()),
                outputs: [result],
            })
            .accepts(ctx, op)
    }
}

op_schema! {
    /// `JSON.stringify(value)` of a typed value: records, arrays, strings, numbers, booleans,
    /// `Uint8Array`s, JSON values and their optionals.
    pub JsonStringifyOp(_value) -> infallible;
    name = "perry.json_stringify";
    format = "$0 ` : ` type($0)";
    constructors {
        pub new(_value: Value) => explicit({
            let ty = _value.get_type(ctx).deref(ctx);
            if ty.is::<OptionalType>() || ty.is::<JsonType>() {
                Ty::OptString.get(ctx)
            } else {
                Ty::String.get(ctx)
            }
        });
    }
    result(ctx): impl OutputContract = Accepts(|ctx: &Context, ty| {
        Ty::String.matches(ctx, ty) || Ty::OptString.matches(ctx, ty)
    });
    error = IntrinsicErr::Stringify;
}

op_schema! {
    /// Validate and construct the declared schema directly from text or an immutable JSON graph.
    pub JsonDecodeOp(source) -> infallible;
    name = "perry.json_decode";
    format = "$0 ` : ` type($0)";
    constructors { pub new(source: Value, target: TypeHandle) => explicit(target); }
    result(ctx, source: Value): impl OutputContract = Accepts(move |ctx: &Context, ty| {
        if Ty::String.matches(ctx, source.get_type(ctx)) { crate::JsonSchema::supports_cursor(ctx, ty) }
        else { crate::JsonSchema::supports_descriptor(ctx, ty) }
    });
    error = IntrinsicErr::Signature("json_decode".into());
    check_before Ty::Json.matches(ctx, source.get_type(ctx)) || Ty::String.matches(ctx, source.get_type(ctx))
        => IntrinsicErr::Signature("json_decode".into());
}

op_schema! {
    /// JSON round-trip normalization after schema compatibility has been proved.
    pub JsonCopyOp(source) -> infallible;
    name = "perry.json_copy";
    format = "$0 ` : ` type($0)";
    constructors { pub new(source: Value, target: TypeHandle) => explicit(target); }
    result(ctx, source: Value): impl OutputContract = Accepts(move |ctx: &Context, ty| crate::JsonSchema::supports_native_copy(ctx, source.get_type(ctx), ty));
    error = IntrinsicErr::Signature("json_copy".into());
}
