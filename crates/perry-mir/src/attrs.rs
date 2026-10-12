//! Attributes of the `perry` dialect.
use pliron::derive::pliron_attr;

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

/// Numeric conversions between ECMAScript numbers and proven integer ranges.
#[pliron_attr(name = "perry.convert_kind", format, verifier = "succ")]
#[derive(PartialEq, Eq, Clone, Copy, Debug, Hash)]
pub enum ConvertKind {
    /// Exact: every signless `i32` (read as signed) is an `f64`.
    I32ToF64,
    /// Only inserted by P3.5 for values proven integral and in `i32` range. Runtime failures
    /// in the subset are exceptions, never traps, so this conversion never checks.
    F64ToI32,
    /// A proven nonnegative integral Wasm32 index.
    F64ToU32,
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
