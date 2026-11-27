use perry_abi::{
    layout::numeric::{self, Tag},
    number::{Number, Relation},
};

#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_number_relation(at: i32, a: u64, bt: i32, b: u64) -> i32 {
    let at = Tag::try_from(at).expect("invalid numeric tag");
    let bt = Tag::try_from(bt).expect("invalid numeric tag");
    match (at, bt) {
        (Tag::Null, Tag::Null) | (Tag::Undefined | Tag::Omitted, Tag::Undefined | Tag::Omitted) => {
            Relation::SameAbsence as i32
        }
        (Tag::Null | Tag::Undefined | Tag::Omitted, _)
        | (_, Tag::Null | Tag::Undefined | Tag::Omitted) => Relation::Unordered as i32,
        _ => Relation::order_mask(Number::from_numeric(at, a).order(Number::from_numeric(bt, b))),
    }
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_number_extremum(
    at: i32,
    a: u64,
    bt: i32,
    b: u64,
    min: i32,
    out: *mut u64,
) -> i32 {
    let min = min != 0;
    let relation = perry_number_relation(at, a, bt, b);
    let preferred = if min {
        Relation::Less
    } else {
        Relation::Greater
    } as i32;
    let left_negative_zero = at == numeric::FLOAT && a == (-0.0f64).to_bits();
    let (tag, payload) = if relation == Relation::Unordered as i32 {
        Number::Float(f64::NAN).into_numeric()
    } else if relation == preferred
        || (relation == Relation::Equal as i32 && min == left_negative_zero)
    {
        (at, a)
    } else {
        (bt, b)
    };
    unsafe { out.write(payload) };
    tag
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_number_abs(tag: i32, bits: u64, out: *mut u64) -> i32 {
    let (tag, magnitude) = if tag == numeric::FLOAT {
        (tag, bits & !(1 << 63))
    } else if tag & 1 != 0 {
        let magnitude = (bits as i64).unsigned_abs();
        let tag = if magnitude > i64::MAX as u64 {
            numeric::UNSIGNED
        } else {
            numeric::SIGNED
        };
        (tag, magnitude)
    } else {
        (tag, bits)
    };
    unsafe { out.write(magnitude) };
    tag
}

/// ECMAScript `%` on numbers: the IEEE remainder truncated toward zero (C `fmod`).
#[inline(never)]
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_f64_rem(dividend: f64, divisor: f64) -> f64 {
    libm::fmod(dividend, divisor)
}

#[derive(Clone, Copy)]
enum Arithmetic {
    Add,
    Sub,
    Mul,
    Rem,
}

impl Arithmetic {
    // Specialize each guest export so addition never links floating-point remainder.
    #[inline(always)]
    fn apply(self, at: Tag, a: u64, bt: Tag, b: u64, widen: bool) -> (i32, u64) {
        let (Some(left_signed), Some(right_signed)) = (at.integer_signed(), bt.integer_signed())
        else {
            let (a, b) = (
                Number::from_numeric(at, a).as_f64(),
                Number::from_numeric(bt, b).as_f64(),
            );
            let result = match self {
                Self::Add => a + b,
                Self::Sub => a - b,
                Self::Mul => a * b,
                Self::Rem => perry_f64_rem(a, b),
            };
            return Number::Float(result).into_numeric();
        };
        let fixed = match (at.fixed(), bt.fixed()) {
            (Some(a), Some(b)) if matches!(self, Self::Rem) => Some(a.max(b)),
            (Some(a), Some(b)) => Some(a.common(b)),
            (Some(width), _) | (_, Some(width)) => Some(width),
            _ => None,
        };
        let both_signed = left_signed && right_signed;
        let bits = match self {
            Self::Add => a.wrapping_add(b),
            Self::Sub => a.wrapping_sub(b),
            Self::Mul => {
                if fixed.is_none() && both_signed && (a == 0 || b == 0) && ((a | b) as i64) < 0 {
                    return Number::Float(-0.0).into_numeric();
                }
                a.wrapping_mul(b)
            }
            Self::Rem => {
                if b == 0 {
                    return Number::Float(f64::NAN).into_numeric();
                }
                let magnitude = |signed, bits: u64| {
                    if signed {
                        (bits as i64).unsigned_abs()
                    } else {
                        bits
                    }
                };
                let rem = magnitude(left_signed, a) % magnitude(right_signed, b);
                if left_signed {
                    if (a as i64) < 0 {
                        return if rem == 0 {
                            Number::Float(-0.0).into_numeric()
                        } else {
                            (numeric::SIGNED, rem.wrapping_neg())
                        };
                    }
                    return (numeric::SIGNED, rem);
                }
                return (fixed.map_or(numeric::UNSIGNED, |width| width.tag()), rem);
            }
        };
        if let Some(width) = fixed {
            let width = if widen { width.unconstrained() } else { width };
            let shift = 64 - width.bits();
            let shifted = bits << shift;
            let value = if width.signed() {
                ((shifted as i64) >> shift) as u64
            } else {
                shifted >> shift
            };
            (width.tag(), value)
        } else {
            (
                if both_signed {
                    numeric::SIGNED
                } else {
                    numeric::UNSIGNED
                },
                bits,
            )
        }
    }
}

macro_rules! arithmetic {
    ($($name:ident => $op:ident),+ $(,)?) => {$(
        #[unsafe(no_mangle)]
        pub(crate) unsafe extern "C" fn $name(
            at: i32, a: u64, bt: i32, b: u64, widen: i32, out: *mut u64,
        ) -> i32 {
            let at = Tag::try_from(at).expect("invalid numeric tag");
            let bt = Tag::try_from(bt).expect("invalid numeric tag");
            let (tag, payload) = Arithmetic::$op.apply(at, a, bt, b, widen != 0);
            unsafe { out.write(payload) };
            tag
        }
    )+};
}

arithmetic! {
    perry_number_add => Add,
    perry_number_sub => Sub,
    perry_number_mul => Mul,
    perry_number_rem => Rem,
}
