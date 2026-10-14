//! Numeric domains and range operations used by the compositional solver.
use super::{Solver, State};
use crate::{attrs::BinaryKind, integers::BigIntConstOp};
use pliron::{
    builtin::{
        attributes::{FPDoubleAttr, IntegerAttr},
        ops::ConstantOp,
        types::{IntegerType, Signedness},
    },
    context::{Context, Ptr},
    operation::Operation,
    r#type::{TypeHandle, Typed},
    value::Value,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Integer {
    pub(super) bits: u32,
    pub(super) signed: bool,
}

impl Integer {
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

    pub(super) fn ty(self, ctx: &mut Context) -> TypeHandle {
        IntegerType::get(
            ctx,
            self.bits,
            if self.signed {
                Signedness::Signed
            } else {
                Signedness::Unsigned
            },
        )
        .into()
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

impl Range {
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
    Literal,
    Integer(Integer),
    Original,
}

impl Fact {
    pub(super) fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Bottom, fact) | (fact, Self::Bottom) => fact,
            (Self::Original, _) | (_, Self::Original) => Self::Original,
            (Self::Literal, fact) | (fact, Self::Literal) => fact,
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

pub(super) fn constant_range(ctx: &Context, op: Ptr<Operation>) -> Option<Interval> {
    let value = if let Some(constant) = Operation::get_op::<BigIntConstOp>(op, ctx) {
        constant.magnitude(ctx)? as i128
    } else {
        let constant = Operation::get_op::<ConstantOp>(op, ctx)?;
        let attr = constant.get_attr_builtin_constant_value(ctx)?;
        if let Some(integer) = attr.downcast_ref::<IntegerAttr>() {
            if integer.get_type().deref(ctx).signedness() == Signedness::Unsigned {
                integer.value().to_u64() as i128
            } else {
                integer.value().to_i64() as i128
            }
        } else {
            let value = f64::from(attr.downcast_ref::<FPDoubleAttr>()?.clone());
            if !value.is_finite() || value.fract() != 0.0 {
                return None;
            }
            value as i128
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
        _ => return domain.map_or(Range::Unknown, |domain| Range::Bounds(domain.bounds())),
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
                return domain.map_or(Range::Unknown, |domain| Range::Bounds(domain.bounds()));
            };
            Interval {
                lower: ll.min(lu).min(ul).min(uu),
                upper: ll.max(lu).max(ul).max(uu),
            }
        }
        _ => return Range::Unknown,
    };
    Range::Bounds(
        domain
            .filter(|&domain| !interval.fits(domain))
            .map_or(interval, Integer::bounds),
    )
}

pub(super) fn comparison_domain(state: &State, inputs: &[Value]) -> Option<Integer> {
    let mut domain = inputs
        .iter()
        .filter_map(|&value| state.fact(value).integer())
        .reduce(Integer::common)?;
    for &input in inputs {
        match state.fact(input) {
            Fact::Integer(_) => {}
            Fact::Literal => {
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
    if let Some(constant) = Operation::get_op::<BigIntConstOp>(op, ctx) {
        return constant.magnitude(ctx);
    }
    let constant = Operation::get_op::<ConstantOp>(op, ctx)?;
    let attr = constant.get_attr_builtin_constant_value(ctx)?;
    let value = f64::from(attr.downcast_ref::<FPDoubleAttr>()?.clone());
    (value.is_finite()
        && value.fract() == 0.0
        && !(value == 0.0 && value.is_sign_negative())
        && value >= i64::MIN as f64
        && value < 18446744073709551616.0)
        .then_some(value as i128 as u64)
}

impl Solver {
    pub(super) fn arithmetic(
        &self,
        ctx: &Context,
        id: usize,
        inputs: &[Value],
        result: Value,
    ) -> Fact {
        let state = &self.analysis.states[id];
        let facts = || inputs.iter().map(|&value| state.fact(value));
        if let Some(integer) = Integer::from_type(ctx, result.get_type(ctx)) {
            return Fact::Integer(integer);
        }
        if facts().any(|fact| fact == Fact::Original) {
            return Fact::Original;
        }
        if facts().any(|fact| fact == Fact::Bottom) {
            return Fact::Bottom;
        }
        if !self.analysis.keys[id].specialize {
            return if facts().all(|fact| fact == Fact::Literal) {
                Fact::Literal
            } else {
                Fact::Original
            };
        }
        let input = facts().filter_map(Fact::integer).reduce(Integer::common);
        let required = state.required.get(&result).copied();
        match (input, required) {
            (Some(input), Some(required)) => Fact::Integer(input.common(required)),
            (Some(input), None) => Fact::Integer(input),
            (None, Some(required)) => Fact::Integer(required),
            _ => Fact::Literal,
        }
    }
}
