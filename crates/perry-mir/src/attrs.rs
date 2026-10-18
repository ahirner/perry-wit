//! Attributes of the `perry` dialect.
use crate::types::NumericType;
use pliron::{
    attribute::{AttrObj, Attribute},
    builtin::{
        attr_interfaces::{MaterializableAttr, TypedAttrInterface},
        attributes::{FPDoubleAttr, IntegerAttr},
        ops::ConstantOp,
        types::{IntegerType, Signedness},
    },
    context::{Context, Ptr},
    derive::{attr_interface_impl, pliron_attr},
    op::Op,
    operation::Operation,
    r#type::TypeHandle,
    utils::apint::{APInt, bw},
};

/// Measurements of the numeric solver and convergent cleanup for one compilation.
#[pliron_attr(name = "perry.optimization_statistics", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Default, Hash)]
pub struct OptimizationStatistics {
    pub numeric_contexts: u64,
    pub constraint_visits: u64,
    pub summarized_heap_sets: u64,
    pub summarized_contexts: u64,
    pub range_widenings: u64,
    pub specializations: u64,
    pub cleanup_rounds: u64,
    pub parse_stringify_rewrites: u64,
    pub parse_stringify_missed: u64,
}

/// Absence states admitted by a statically typed value.
#[pliron_attr(name = "perry.absence", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum Absence {
    Undefined,
    Null,
    Both,
}

impl Absence {
    pub fn admits_null(self) -> bool {
        self != Self::Undefined
    }

    pub fn admits_undefined(self) -> bool {
        self != Self::Null
    }
}

/// Arithmetic on IEEE-754 doubles with ECMAScript semantics (`%` is `fmod`).
#[pliron_attr(name = "perry.binary_kind", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum BinaryKind {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// Ordered comparison with ECMAScript semantics (`NaN` compares false except for `Ne`).
#[pliron_attr(name = "perry.compare_kind", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum CompareKind {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

/// Memory tier of an allocation site (design decision D3).
///
/// Ordered by escape: `Scalar < Frame < Task < Instance`. `Unassigned` is the builder's
/// output before tier classification runs.
#[pliron_attr(name = "perry.tier", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash, PartialOrd, Ord)]
pub enum Tier {
    Unassigned,
    Scalar,
    Frame,
    Task,
    Instance,
}

/// The ECMAScript error a throwing op raises.
#[pliron_attr(name = "perry.error_kind", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum ErrorKind {
    /// Any error: a call or await propagates whatever the callee threw. Also the kind of a
    /// plain `Error` object.
    Any,
    TypeError,
    RangeError,
    ReferenceError,
    SyntaxError,
    /// A thrown value that is not an `Error` object (`throw 90`).
    Value,
    /// `process.exit`: unwinds to the export without running `catch` or `finally`.
    Exit,
}

/// Operands whose storage may be consumed after their final observation.
#[pliron_attr(
    name = "perry.reuse_operands",
    format = "`[` vec($0, CharSpace(`,`)) `]`",
    verifier = "succ"
)]
#[derive(PartialEq, Eq, Clone, Debug, Hash)]
pub struct ReuseOperands(pub Vec<u32>);

/// An exact signed/unsigned integer or the bit pattern of an IEEE double.
#[pliron_attr(name = "perry.numeric_constant", format, verifier = "succ")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NumericConstant {
    Signed(i64),
    Unsigned(u64),
    Float(u64),
}

impl NumericConstant {
    /// The exact value of a constant attribute: an integer by its type's signedness (signless
    /// as signed), a double by its bits, or a numeric constant itself.
    pub(crate) fn of(ctx: &Context, attr: &dyn Attribute) -> Option<Self> {
        if let Some(&number) = attr.downcast_ref::<Self>() {
            Some(number)
        } else if let Some(integer) = attr.downcast_ref::<IntegerAttr>() {
            Some(
                if integer.get_type().deref(ctx).signedness() == Signedness::Unsigned {
                    Self::Unsigned(integer.value().to_u64())
                } else {
                    Self::Signed(integer.value().to_i64())
                },
            )
        } else {
            let value = f64::from(attr.downcast_ref::<FPDoubleAttr>()?.clone());
            Some(Self::Float(value.to_bits()))
        }
    }

    /// An integer or double scalar as a numeric constant; a double holding a safe integer
    /// (other than `-0`) becomes that integer.
    pub(crate) fn from_scalar(ctx: &Context, attr: &dyn Attribute) -> Option<Self> {
        Some(match Self::of(ctx, attr)? {
            Self::Float(bits) => {
                let value = f64::from_bits(bits);
                let safe = value.is_finite()
                    && value.fract() == 0.0
                    && !(value == 0.0 && value.is_sign_negative())
                    && (-9007199254740992.0..=9007199254740992.0).contains(&value);
                if safe {
                    Self::Signed(value as i64)
                } else {
                    Self::Float(bits)
                }
            }
            integer => integer,
        })
    }

    /// The value as the ABI's number.
    pub(crate) fn number(self) -> perry_abi::number::Number {
        match self {
            Self::Signed(value) => perry_abi::number::Number::Signed(value),
            Self::Unsigned(value) => perry_abi::number::Number::Unsigned(value),
            Self::Float(bits) => perry_abi::number::Number::Float(f64::from_bits(bits)),
        }
    }

    pub(crate) fn scalar(self, ctx: &Context) -> AttrObj {
        let (bits, signedness) = match self {
            Self::Signed(value) => (value as u64, Signedness::Signed),
            Self::Unsigned(value) => (value, Signedness::Unsigned),
            Self::Float(bits) => return Box::new(FPDoubleAttr::from(f64::from_bits(bits))),
        };
        Box::new(IntegerAttr::new(
            IntegerType::get(ctx, 64, signedness),
            APInt::from_u64(bits, bw(64)),
        ))
    }

    pub(crate) fn order(self, other: Self) -> Option<std::cmp::Ordering> {
        use std::cmp::Ordering::{Greater, Less};
        match (self, other) {
            (Self::Signed(a), Self::Signed(b)) => Some(a.cmp(&b)),
            (Self::Unsigned(a), Self::Unsigned(b)) => Some(a.cmp(&b)),
            (Self::Signed(a), Self::Unsigned(b)) => {
                Some(if a < 0 { Less } else { (a as u64).cmp(&b) })
            }
            (Self::Unsigned(a), Self::Signed(b)) => {
                Some(if b < 0 { Greater } else { a.cmp(&(b as u64)) })
            }
            (Self::Float(a), Self::Float(b)) => f64::from_bits(a).partial_cmp(&f64::from_bits(b)),
            (Self::Signed(integer), Self::Float(bits)) => {
                Self::order_integer_float(integer < 0, integer.unsigned_abs(), f64::from_bits(bits))
            }
            (Self::Unsigned(integer), Self::Float(bits)) => {
                Self::order_integer_float(false, integer, f64::from_bits(bits))
            }
            (Self::Float(bits), other) => other
                .order(Self::Float(bits))
                .map(std::cmp::Ordering::reverse),
        }
    }

    fn order_integer_float(
        negative: bool,
        magnitude: u64,
        float: f64,
    ) -> Option<std::cmp::Ordering> {
        use std::cmp::Ordering::{Equal, Greater, Less};
        if float.is_nan() {
            return None;
        }
        if float == f64::INFINITY {
            return Some(Less);
        }
        if float == f64::NEG_INFINITY {
            return Some(Greater);
        }
        if magnitude == 0 {
            return (0.0f64).partial_cmp(&float);
        }
        if negative != float.is_sign_negative() || float == 0.0 {
            return Some(if negative { Less } else { Greater });
        }
        let abs = float.abs();
        if abs < 1.0 {
            return Some(if negative { Less } else { Greater });
        }
        if abs >= 18446744073709551616.0 {
            return Some(if negative { Greater } else { Less });
        }
        let truncated = abs.trunc() as u64;
        let magnitude_order = match magnitude.cmp(&truncated) {
            Equal if abs.fract() != 0.0 => Less,
            ordering => ordering,
        };
        Some(if negative {
            magnitude_order.reverse()
        } else {
            magnitude_order
        })
    }
}

#[attr_interface_impl]
impl TypedAttrInterface for NumericConstant {
    fn get_type(&self, ctx: &Context) -> TypeHandle {
        NumericType::get(ctx).into()
    }
}

#[attr_interface_impl]
impl MaterializableAttr for NumericConstant {
    fn materialize(&self, ctx: &mut Context) -> Ptr<Operation> {
        ConstantOp::new(ctx, Box::new(*self)).get_operation()
    }
}

/// Per-function metadata flags for exports, async splitting, entry specialization, and atomic construction.
#[pliron_attr(name = "perry.function_meta", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Default, Hash)]
pub(crate) struct FunctionMeta {
    pub(crate) exported: bool,
    pub(crate) asynchronous: bool,
    pub(crate) specialized_entry: bool,
    pub(crate) atomic_construction: bool,
}

/// Semantic kind of a compiler-generated JSON decoder function.
#[pliron_attr(name = "perry.json_decoder_kind", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum JsonDecoderKind {
    Parse,
    Value,
    Cursor,
}
