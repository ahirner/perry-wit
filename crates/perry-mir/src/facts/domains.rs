//! Numeric domains and range operations used by the compositional solver.
use super::{Solver, State};
use crate::{
    attrs::{BinaryKind, NumericConstant},
    integers::BigIntConstOp,
};
use pliron::{
    builtin::{
        attributes::IntegerAttr,
        ops::ConstantOp,
        types::{IntegerType, Signedness},
    },
    context::{Context, Ptr},
    operation::Operation,
    r#type::{TypeHandle, Typed, TypedHandle},
    utils::apint::{APInt, bw},
    value::Value,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Integer {
    pub(super) bits: u32,
    pub(super) signed: bool,
}

impl Integer {
    pub(super) const U8: Self = Self {
        bits: 8,
        signed: false,
    };
    pub(super) const U32: Self = Self {
        bits: 32,
        signed: false,
    };
    pub(super) const I32: Self = Self {
        bits: 32,
        signed: true,
    };
    pub(super) const I64: Self = Self {
        bits: 64,
        signed: true,
    };

    pub(super) fn bounds(self) -> Interval {
        if self.signed {
            Interval {
                lower: -(1i128 << (self.bits - 1)),
                upper: (1i128 << (self.bits - 1)) - 1,
            }
        } else {
            Interval {
                lower: 0,
                upper: (1i128 << self.bits) - 1,
            }
        }
    }
    pub(super) fn from_type(ctx: &Context, ty: TypeHandle) -> Option<Self> {
        let ty = ty.deref(ctx);
        let ty = ty.downcast_ref::<IntegerType>()?;
        (matches!(ty.width(), 8 | 16 | 32 | 64) && ty.signedness() != Signedness::Signless)
            .then_some(Self {
                bits: ty.width(),
                signed: ty.signedness() != Signedness::Unsigned,
            })
    }

    pub(super) fn ty(self, ctx: &Context) -> TypeHandle {
        self.integer_type(ctx).into()
    }

    pub(super) fn integer_type(self, ctx: &Context) -> TypedHandle<IntegerType> {
        let signedness = if self.signed {
            Signedness::Signed
        } else {
            Signedness::Unsigned
        };
        IntegerType::get(ctx, self.bits, signedness)
    }

    /// The constant of this domain holding the two's-complement `bits`.
    pub(super) fn constant(self, ctx: &mut Context, bits: u64) -> ConstantOp {
        let ty = self.integer_type(ctx);
        let value = APInt::from_u64(bits, bw(self.bits as usize));
        ConstantOp::new(ctx, Box::new(IntegerAttr::new(ty, value)))
    }

    pub(super) fn common(self, other: Self) -> Self {
        if self.signed == other.signed {
            return Self {
                bits: self.bits.max(other.bits),
                signed: self.signed,
            };
        }
        let (signed, unsigned) = if self.signed {
            (self, other)
        } else {
            (other, self)
        };
        if signed.bits > unsigned.bits {
            return signed;
        }
        [8, 16, 32, 64]
            .into_iter()
            .find(|&bits| bits > unsigned.bits)
            .map(|bits| Self { bits, signed: true })
            .unwrap_or(Self {
                bits: 64,
                signed: false,
            })
    }

    pub(super) fn unconstrained(self) -> Self {
        Self { bits: 64, ..self }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Interval {
    pub(super) lower: i128,
    pub(super) upper: i128,
}

impl Interval {
    pub(super) fn fits(self, domain: Integer) -> bool {
        let bounds = domain.bounds();
        self.lower >= bounds.lower && self.upper <= bounds.upper
    }

    pub(super) fn include(self, domain: Integer) -> Option<Integer> {
        if self.fits(domain) {
            return Some(domain);
        }
        for bits in [8, 16, 32, 64] {
            for signed in [true, false] {
                let common = domain.common(Integer { bits, signed });
                if self.fits(common) {
                    return Some(common);
                }
            }
        }
        None
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Range {
    #[default]
    Bottom,
    Bounds(Interval),
    Unknown,
}

impl Interval {
    /// `Math.abs` over the interval.
    pub(super) fn abs(self) -> Self {
        let (lower, upper) = (self.lower.abs(), self.upper.abs());
        Self {
            lower: if self.lower <= 0 && self.upper >= 0 {
                0
            } else {
                lower.min(upper)
            },
            upper: lower.max(upper),
        }
    }

    /// `Math.min` (`max: false`) or `Math.max` of values from both intervals.
    pub(super) fn extreme(self, other: Self, max: bool) -> Self {
        let pick = |a: i128, b: i128| if max { a.max(b) } else { a.min(b) };
        Self {
            lower: pick(self.lower, other.lower),
            upper: pick(self.upper, other.upper),
        }
    }
}

impl Range {
    /// The bounds of `domain`, or no bound without one.
    pub(super) fn of(domain: Option<Integer>) -> Self {
        domain.map_or(Self::Unknown, |domain| Self::Bounds(domain.bounds()))
    }

    /// `interval`, widened to the bounds of `domain` when it does not fit there.
    pub(super) fn clamp(interval: Interval, domain: Option<Integer>) -> Self {
        Self::Bounds(
            domain
                .filter(|&domain| !interval.fits(domain))
                .map_or(interval, Integer::bounds),
        )
    }

    pub(super) fn join(self, other: Self, domain: Option<Integer>) -> Self {
        self.join_step(other, domain, true).0
    }

    /// Join `other` into `self`. When `widen` is false (first expansion), compute the
    /// exact convex hull clamped to `domain` bounds; when `widen` is true (second or later
    /// expansion), widen expanding bounds to the enclosing integer domain bounds.
    /// Returns `(joined_range, expanded, widened)`.
    pub(super) fn join_step(
        self,
        other: Self,
        domain: Option<Integer>,
        widen: bool,
    ) -> (Self, bool, bool) {
        match (self, other) {
            (Self::Bottom, range) => (range, false, false),
            (range, Self::Bottom) => (range, false, false),
            (Self::Unknown, _) | (_, Self::Unknown) => (Self::Unknown, false, false),
            (Self::Bounds(old), Self::Bounds(new)) => {
                let limits = domain.map_or(
                    Interval {
                        lower: i128::MIN,
                        upper: i128::MAX,
                    },
                    Integer::bounds,
                );
                let expand_lower = new.lower < old.lower;
                let expand_upper = new.upper > old.upper;
                let expanded = expand_lower || expand_upper;
                if !expanded {
                    return (Self::Bounds(old), false, false);
                }
                let lower = if expand_lower {
                    if widen {
                        limits.lower
                    } else {
                        new.lower.max(limits.lower)
                    }
                } else {
                    old.lower
                };
                let upper = if expand_upper {
                    if widen {
                        limits.upper
                    } else {
                        new.upper.min(limits.upper)
                    }
                } else {
                    old.upper
                };
                let joined = Self::Bounds(Interval { lower, upper });
                let widened = widen
                    && ((expand_lower && lower < new.lower.max(limits.lower))
                        || (expand_upper && upper > new.upper.min(limits.upper)));
                (joined, true, widened)
            }
        }
    }
}

/// Literal integer values are eligible for specialization, but do not themselves impose
/// wrapping semantics on an unconstrained TypeScript computation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Fact {
    #[default]
    Bottom,
    Literal {
        negative: bool,
    },
    Integer(Integer),
    Numeric,
    Original,
}

impl Fact {
    pub(super) fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Bottom, fact) | (fact, Self::Bottom) => fact,
            (Self::Numeric, _) | (_, Self::Numeric) => Self::Numeric,
            (Self::Original, Self::Integer(integer)) | (Self::Integer(integer), Self::Original)
                if integer.bits == 64 =>
            {
                Self::Numeric
            }
            (Self::Original, _) | (_, Self::Original) => Self::Original,
            (Self::Literal { negative: a }, Self::Literal { negative: b }) => {
                Self::Literal { negative: a || b }
            }
            (Self::Literal { negative }, Self::Integer(integer))
            | (Self::Integer(integer), Self::Literal { negative }) => {
                if negative && !integer.signed {
                    let widened = integer.common(Integer {
                        bits: 8,
                        signed: true,
                    });
                    if widened.signed {
                        Self::Integer(widened)
                    } else {
                        Self::Numeric
                    }
                } else {
                    Self::Integer(integer)
                }
            }
            (Self::Integer(a), Self::Integer(b)) => Self::Integer(a.common(b)),
        }
    }

    pub(super) fn integer(self) -> Option<Integer> {
        if let Self::Integer(integer) = self {
            Some(integer)
        } else {
            None
        }
    }

    pub(super) fn needs_lossless_64(self) -> bool {
        self == Self::Numeric || self.integer().is_some_and(|integer| integer.bits == 64)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ResultDomain {
    Inferred,
    Integer(Integer),
    /// Escaping promises keep their declared payload representation; their callees may
    /// still specialize arithmetic using known argument domains.
    Original,
}

impl ResultDomain {
    pub(super) fn integer(self) -> Option<Integer> {
        match self {
            Self::Integer(integer) => Some(integer),
            Self::Inferred | Self::Original => None,
        }
    }
}

impl From<Option<Integer>> for ResultDomain {
    fn from(integer: Option<Integer>) -> Self {
        integer.map_or(Self::Inferred, Self::Integer)
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ConstParsedInt {
    Integer {
        bits: u64,
        domain: Integer,
        interval: Interval,
        needs_i64: bool,
    },
    Float(u64),
}

pub(super) fn const_parsed_int(ctx: &Context, op: Ptr<Operation>) -> Option<ConstParsedInt> {
    if crate::intrinsics::intrinsic_of(ctx, op) != Some(crate::intrinsics::Intrinsic::ParseInt) {
        return None;
    }
    let operation = op.deref(ctx);
    let str_op = operation.get_operand(0).defining_op()?;
    let str_const = Operation::get_op::<crate::collections::StringConstOp>(str_op, ctx)?;
    let text_attr = str_const.get_attr_perry_string_value(ctx)?;
    let text: &str = text_attr.as_ref();
    let radix_op = operation.get_operand(1).defining_op()?;
    let radix_const = Operation::get_op::<ConstantOp>(radix_op, ctx)?;
    let radix_attr = radix_const.get_attr_builtin_constant_value(ctx)?;
    let radix = NumericConstant::of(ctx, &**radix_attr)?.number();
    match perry_abi::number::parse_integer(text, radix) {
        perry_abi::number::Number::Signed(value) => {
            let magnitude = value.unsigned_abs();
            let needs_i64 =
                magnitude > 0 && (64 - magnitude.leading_zeros() - magnitude.trailing_zeros() > 53);
            Some(ConstParsedInt::Integer {
                bits: value as u64,
                domain: Integer::I64,
                interval: Interval {
                    lower: value as i128,
                    upper: value as i128,
                },
                needs_i64,
            })
        }
        perry_abi::number::Number::Unsigned(value) => {
            let needs_i64 = value > 0 && (64 - value.leading_zeros() - value.trailing_zeros() > 53);
            Some(ConstParsedInt::Integer {
                bits: value,
                domain: Integer {
                    bits: 64,
                    signed: value <= i64::MAX as u64,
                },
                interval: Interval {
                    lower: value as i128,
                    upper: value as i128,
                },
                needs_i64,
            })
        }
        perry_abi::number::Number::Float(f) => Some(ConstParsedInt::Float(f.to_bits())),
    }
}

pub(super) fn constant_range(ctx: &Context, op: Ptr<Operation>) -> Option<Interval> {
    if let Some(ConstParsedInt::Integer { interval, .. }) = const_parsed_int(ctx, op) {
        return Some(interval);
    }
    let value = if let Some(constant) = Operation::get_op::<BigIntConstOp>(op, ctx) {
        constant.magnitude(ctx)? as i128
    } else {
        match constant(ctx, op)? {
            NumericConstant::Signed(value) => value as i128,
            NumericConstant::Unsigned(value) => value as i128,
            NumericConstant::Float(bits) => {
                let value = f64::from_bits(bits);
                (value.is_finite() && value.fract() == 0.0).then_some(value as i128)?
            }
        }
    };
    Some(Interval {
        lower: value,
        upper: value,
    })
}

pub(super) fn arithmetic_range(
    kind: BinaryKind,
    lhs: Range,
    rhs: Range,
    domain: Option<Integer>,
) -> Range {
    let (a, b) = match (lhs, rhs) {
        (Range::Bottom, _) | (_, Range::Bottom) => return Range::Bottom,
        (Range::Bounds(a), Range::Bounds(b)) => (a, b),
        _ => return Range::of(domain),
    };
    let interval = match kind {
        _ if domain.is_none()
            && [a.lower, a.upper, b.lower, b.upper]
                .iter()
                .any(|&bound| bound == i128::MIN || bound == i128::MAX)
            && !(kind == BinaryKind::Add
                && ((a.lower >= 0 && b.lower >= 0) || (a.upper <= 0 && b.upper <= 0))) =>
        {
            return Range::Unknown;
        }
        BinaryKind::Add => Interval {
            lower: a.lower.saturating_add(b.lower),
            upper: a.upper.saturating_add(b.upper),
        },
        BinaryKind::Sub => Interval {
            lower: a.lower.saturating_sub(b.upper),
            upper: a.upper.saturating_sub(b.lower),
        },
        BinaryKind::Mul => {
            let [Some(ll), Some(lu), Some(ul), Some(uu)] = [
                a.lower.checked_mul(b.lower),
                a.lower.checked_mul(b.upper),
                a.upper.checked_mul(b.lower),
                a.upper.checked_mul(b.upper),
            ] else {
                return Range::of(domain);
            };
            Interval {
                lower: ll.min(lu).min(ul).min(uu),
                upper: ll.max(lu).max(ul).max(uu),
            }
        }
        _ => return Range::Unknown,
    };
    Range::clamp(interval, domain)
}

/// The kind of `op` when it is integer-domain arithmetic: `+`, `-`, `*`, or BigInt `/` and `%`.
pub(super) fn integer_arithmetic(ctx: &Context, op: Ptr<Operation>) -> Option<BinaryKind> {
    let kind = Operation::get_op::<crate::scalar::BinaryOp>(op, ctx)?.kind(ctx);
    let bigint = || {
        op.deref(ctx)
            .get_result(0)
            .get_type(ctx)
            .deref(ctx)
            .is::<crate::types::BigIntType>()
    };
    (matches!(kind, BinaryKind::Add | BinaryKind::Sub | BinaryKind::Mul) || bigint())
        .then_some(kind)
}

pub(super) fn comparison_domain(state: &State, inputs: &[Value]) -> Option<Integer> {
    let mut domain = inputs
        .iter()
        .filter_map(|&value| state.fact(value).integer())
        .reduce(Integer::common)?;
    for &input in inputs {
        match state.fact(input) {
            Fact::Integer(_) => {}
            Fact::Literal { .. } => {
                let Range::Bounds(interval) = state.range(input) else {
                    return None;
                };
                domain = interval.include(domain)?;
            }
            _ => return None,
        }
    }
    Some(domain)
}

pub(super) fn literal(ctx: &Context, op: Ptr<Operation>) -> Option<u64> {
    if let Some(ConstParsedInt::Integer { bits, .. }) = const_parsed_int(ctx, op) {
        return Some(bits);
    }
    if let Some(constant) = Operation::get_op::<BigIntConstOp>(op, ctx) {
        return constant.magnitude(ctx);
    }
    // Integer-typed constants already have their domain; they are not literals.
    let attr = Operation::get_op::<ConstantOp>(op, ctx)?.get_attr_builtin_constant_value(ctx)?;
    if attr.is::<IntegerAttr>() {
        return None;
    }
    match NumericConstant::of(ctx, &**attr)? {
        NumericConstant::Signed(value) => Some(value as u64),
        NumericConstant::Unsigned(value) => Some(value),
        NumericConstant::Float(bits) => {
            let value = f64::from_bits(bits);
            (value.is_finite()
                && value.fract() == 0.0
                && !(value == 0.0 && value.is_sign_negative())
                && value >= i64::MIN as f64
                && value < 18446744073709551616.0)
                .then_some(value as i128 as u64)
        }
    }
}

/// The value of a `builtin.constant` op.
fn constant(ctx: &Context, op: Ptr<Operation>) -> Option<NumericConstant> {
    let attr = Operation::get_op::<ConstantOp>(op, ctx)?.get_attr_builtin_constant_value(ctx)?;
    NumericConstant::of(ctx, &**attr)
}

impl Solver {
    pub(super) fn arithmetic(
        &self,
        ctx: &Context,
        id: usize,
        inputs: impl Iterator<Item = Value> + Clone,
        result: Value,
    ) -> Fact {
        let state = &self.analysis.states[id];
        let facts = || inputs.clone().map(|value| state.fact(value));
        if let Some(integer) = Integer::from_type(ctx, result.get_type(ctx)) {
            return Fact::Integer(integer);
        }
        if facts().any(|fact| fact == Fact::Bottom) {
            return Fact::Bottom;
        }
        let required = state.required.get(&result).copied();
        if self.analysis.keys[id].specialize
            && (facts().any(|fact| fact == Fact::Numeric)
                || (facts().any(|fact| fact == Fact::Original)
                    && facts().any(|fact| fact.integer().is_some_and(|int| int.bits == 64))))
        {
            return Fact::Numeric;
        }
        if facts().any(|fact| fact == Fact::Original) {
            return Fact::Original;
        }
        let negative = inputs.clone().any(|v| {
            matches!(state.fact(v), Fact::Literal { negative: true })
                || matches!(state.range(v), Range::Bounds(i) if i.lower < 0)
        });
        if !self.analysis.keys[id].specialize {
            return if facts().all(|fact| matches!(fact, Fact::Literal { .. })) {
                Fact::Literal { negative }
            } else {
                Fact::Original
            };
        }
        let input = facts().filter_map(Fact::integer).reduce(Integer::common);
        match (input, required) {
            (Some(input), Some(required)) => Fact::Integer(input.common(required)),
            (Some(input), None) => Fact::Integer(input),
            (None, Some(required)) => Fact::Integer(required),
            _ => Fact::Literal { negative },
        }
    }
}
