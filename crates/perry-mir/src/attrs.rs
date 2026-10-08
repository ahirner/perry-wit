//! Attributes of the `perry` dialect.
use pliron::derive::pliron_attr;

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
    /// Traps unless the value is an integer in `i32` range. P3.5 removes the check where
    /// integer ranges are proven.
    F64ToI32,
}
