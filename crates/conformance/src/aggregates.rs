//! Orthogonal generator and exact reference model for aggregate layout specialization,
//! recursive types, `parseInt` value sources, origin-limit scaling, and WIT export boundaries.
//!
//! Covers the product of:
//! - aggregate shapes (`FlatRecord`, `NestedRecord`, `SelfRecursive`, `MutuallyRecursive`,
//!   `RecordWithArrays`),
//! - WIT export signatures (`ObservationRecord`, `MultiFieldRecord`, `DirectList`, `OptionRecord`),
//! - value sources (`FloatFraction`, `NegativeZero`, `NanValue`, `NarrowInt`, `BoundaryInput`,
//!   constant-foldable `ConstParseInt` with radix wrapping, and runtime-string `RuntimeParseInt`),
//! - multiple distinct instances of the same type with independent field representations + optional
//!   origin-limit overflow (`> 32` origins),
//! - operations (`ReadMod`, `MutateSlot`, `ArrayConcatSpread`, `SharedHelperCall`, `CompareSlots`,
//!   `InspectSpecial`),
//! - declaration-order permutation (`permuted`).

use crate::{
    execution::{Observation, Outcome, check_equivalence, number},
    program::INPUTS,
    registry::{Case, Contract, Witness},
};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

mod emit;

/// Aggregate type topology generated for the test case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AggregateShape {
    FlatRecord,
    NestedRecord,
    SelfRecursive,
    MutuallyRecursive,
    RecordWithArrays,
}

/// WIT export signature used by the generated component.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WitSignature {
    /// `record observation { value: f64, trace: list<f64> }`
    ObservationRecord,
    /// `record observation { value: f64, head: f64, tail: f64, trace: list<f64> }`
    MultiFieldRecord,
    /// `export run: func(x: f64) -> result<list<f64>>;`
    DirectList,
    /// `record observation { value: f64, maybe: option<f64>, trace: list<f64> }`
    OptionRecord,
}

/// Exact 64-bit integer seeds (including values `> 2^53` and radix-wrapping cases).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExactIntSeed {
    /// `"9007199254740993"`, radix `10` (`2^53 + 1`)
    TwoPow53Plus1,
    /// `"9007199254740995"`, radix `10` (`2^53 + 3`)
    TwoPow53Plus3,
    /// `"-9007199254740993"`, radix `10` (`-(2^53 + 1)`)
    NegTwoPow53Plus1,
    /// `"18014398509481987"`, radix `10` (`2^54 + 3`)
    TwoPow54Plus3,
    /// `"9223372036854775807"`, radix `10` (`i64::MAX`)
    I64MaxDec,
    /// `"-9223372036854775808"`, radix `10` (`i64::MIN`)
    I64MinDec,
    /// `"7fffffffffffffff"`, radix `16` (`i64::MAX`)
    I64MaxHex,
    /// `"9007199254740993"`, radix `4294967306` (`2^32 + 10` -> wraps to radix `10` via `ToInt32`)
    RadixWrapTwoPow53Plus1,
}

impl ExactIntSeed {
    pub fn as_i64(self) -> i64 {
        match self {
            Self::TwoPow53Plus1 | Self::RadixWrapTwoPow53Plus1 => 9_007_199_254_740_993,
            Self::TwoPow53Plus3 => 9_007_199_254_740_995,
            Self::NegTwoPow53Plus1 => -9_007_199_254_740_993,
            Self::TwoPow54Plus3 => 18_014_398_509_481_987,
            Self::I64MaxDec | Self::I64MaxHex => i64::MAX,
            Self::I64MinDec => i64::MIN,
        }
    }

    pub fn text_and_radix(self) -> (&'static str, i64) {
        match self {
            Self::TwoPow53Plus1 => ("9007199254740993", 10),
            Self::TwoPow53Plus3 => ("9007199254740995", 10),
            Self::NegTwoPow53Plus1 => ("-9007199254740993", 10),
            Self::TwoPow54Plus3 => ("18014398509481987", 10),
            Self::I64MaxDec => ("9223372036854775807", 10),
            Self::I64MinDec => ("-9223372036854775808", 10),
            Self::I64MaxHex => ("7fffffffffffffff", 16),
            Self::RadixWrapTwoPow53Plus1 => ("9007199254740993", 4_294_967_306),
        }
    }
}

/// Source expression used to populate a field or array slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValueSource {
    /// Non-integer `f64` fraction: `(k as f64) * 2.0 + 0.5` (e.g. `-1.5`, `-0.5`, `0.5`, `1.5`).
    FloatFraction(i8),
    /// `-0.0`
    NegativeZero,
    /// `NaN` via `parseInt("invalid", 10)`
    NanValue,
    /// Small integer literal `-30..=30`
    NarrowInt(i8),
    /// Function boundary `f64` parameter `x`
    BoundaryInput,
    /// Widened 64-bit `Fact::Integer` arithmetic expression `(rtStr("abc").length * k)`
    IntArith(i8),
    /// Widened 64-bit `Fact::Integer` arithmetic returned from a helper `helperIntArith(k)`
    HelperIntArith(i8),
    /// Constant-foldable `parseInt("...", radix)`
    ConstParseInt(ExactIntSeed),
    /// Non-constant-foldable `parseInt(rtStr("..."), rtRadix(radix))`
    RuntimeParseInt(ExactIntSeed),
}

fn int_arith_factor(raw: i8) -> i64 {
    i64::from(raw.rem_euclid(11)) + 2
}

impl ValueSource {
    pub fn uses_wide_exact_int(self) -> bool {
        matches!(self, Self::ConstParseInt(_) | Self::RuntimeParseInt(_))
    }

    fn eval(self, x: f64) -> RefVal {
        match self {
            Self::FloatFraction(k) => RefVal::Float(f64::from(k) * 2.0 + 0.5),
            Self::NegativeZero => RefVal::Float(-0.0),
            Self::NanValue => RefVal::Float(f64::NAN),
            Self::NarrowInt(n) => RefVal::Float(f64::from(n)),
            Self::BoundaryInput => RefVal::Float(x),
            Self::IntArith(k) | Self::HelperIntArith(k) => RefVal::Int64(3 * int_arith_factor(k)),
            Self::ConstParseInt(seed) | Self::RuntimeParseInt(seed) => RefVal::Int64(seed.as_i64()),
        }
    }
}

/// Which slot of an aggregate instance to read or mutate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotSelector {
    PrimaryA,
    PrimaryB,
    ChildOrOpt,
}

/// Specification of one instance of the aggregate shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceSpec {
    pub a: ValueSource,
    pub b: ValueSource,
    pub child_a: ValueSource,
    pub child_b: ValueSource,
    pub has_child: bool,
}

/// An operation applied to the generated instances.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AggregateOp {
    /// Read a slot and record `((val % divisor) + 0)` and a `JSON.stringify` probe.
    ReadMod {
        inst: usize,
        slot: SlotSelector,
        divisor: i8,
    },
    /// Select between two instances across a control-flow branch (`x >= 0 ? inst_a : inst_b`),
    /// read `slot` through the shared getter, and record `% 2`, `JSON.stringify`, and a
    /// helper-returned arithmetic aggregate probe.
    BranchRead {
        inst_a: usize,
        inst_b: usize,
        slot: SlotSelector,
    },
    /// Overwrite a primary slot (`PrimaryA` or `PrimaryB`) on `inst` with `new_source`.
    MutateSlot {
        inst: usize,
        slot_b: bool,
        new_source: ValueSource,
    },
    /// Combine slots from `inst_a` and `inst_b` via array literal, `.concat(...)`, and `[...arr]`
    /// spread, then record `% 2` of each element.
    ArrayConcatSpread { inst_a: usize, inst_b: usize },
    /// Pass `inst` through a shared helper function called across all instances and record `% 2`.
    SharedHelperCall { inst: usize, slot_b: bool },
    /// Compare two slots for `<` and `===` without rounding 64-bit integers.
    CompareSlots {
        inst_a: usize,
        slot_a_b: bool,
        inst_b: usize,
        slot_b_b: bool,
    },
    /// Inspect `Number.isNaN(val)` and `-0.0` (`1 / val === -Infinity`) and `((val % 2) + 0)`.
    InspectSpecial { inst: usize, slot_b: bool },
}

/// Complete generated aggregate conformance test case.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateCase {
    pub shape: AggregateShape,
    pub wit_signature: WitSignature,
    pub instances: Vec<InstanceSpec>,
    pub ops: Vec<AggregateOp>,
    pub overflow_origins: bool,
    pub permuted: bool,
}

#[derive(Clone, Copy, Debug)]
enum RefVal {
    Int64(i64),
    Float(f64),
}

impl RefVal {
    fn to_f64(self) -> f64 {
        match self {
            Self::Int64(n) => n as f64,
            Self::Float(f) => f,
        }
    }

    fn is_nan(self) -> bool {
        matches!(self, Self::Float(f) if f.is_nan())
    }

    fn is_neg_zero(self) -> bool {
        match self {
            Self::Int64(_) => false,
            Self::Float(f) => f == 0.0 && f.is_sign_negative(),
        }
    }

    fn rem_plus_zero(self, divisor: i64) -> f64 {
        let r = match self {
            Self::Int64(n) => (n.wrapping_rem(divisor)) as f64,
            Self::Float(f) => f % (divisor as f64),
        };
        r + 0.0
    }

    fn eq_val(self, other: Self) -> bool {
        match (self, other) {
            (Self::Float(a), _) if a.is_nan() => false,
            (_, Self::Float(b)) if b.is_nan() => false,
            (Self::Int64(a), Self::Int64(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a == b,
            (Self::Int64(i), Self::Float(f)) | (Self::Float(f), Self::Int64(i)) => {
                if !f.is_finite() || f.fract() != 0.0 {
                    return false;
                }
                if f >= 9_223_372_036_854_775_808.0 || f < -9_223_372_036_854_775_808.0 {
                    return false;
                }
                (i as i128) == (f as i128)
            }
        }
    }

    fn lt_val(self, other: Self) -> bool {
        match (self, other) {
            (Self::Float(a), _) if a.is_nan() => false,
            (_, Self::Float(b)) if b.is_nan() => false,
            (Self::Int64(a), Self::Int64(b)) => a < b,
            (Self::Float(a), Self::Float(b)) => a < b,
            (Self::Int64(i), Self::Float(f)) => {
                if f == f64::INFINITY {
                    return true;
                }
                if f == f64::NEG_INFINITY {
                    return false;
                }
                if f >= 9_223_372_036_854_775_808.0 {
                    return true;
                }
                if f < -9_223_372_036_854_775_808.0 {
                    return false;
                }
                let fl = f.floor();
                let fl_i = fl as i128;
                let ii = i as i128;
                if f.fract() == 0.0 {
                    ii < fl_i
                } else {
                    ii <= fl_i
                }
            }
            (Self::Float(f), Self::Int64(i)) => {
                if f == f64::NEG_INFINITY {
                    return true;
                }
                if f == f64::INFINITY {
                    return false;
                }
                if f >= 9_223_372_036_854_775_808.0 {
                    return false;
                }
                if f < -9_223_372_036_854_775_808.0 {
                    return true;
                }
                let fl = f.floor();
                let fl_i = fl as i128;
                let ii = i as i128;
                fl_i < ii
            }
        }
    }

    fn json_probe_code(self) -> f64 {
        if self.eq_val(Self::Int64(9_007_199_254_740_993)) {
            1.0
        } else if self.eq_val(Self::Int64(6)) {
            2.0
        } else {
            0.0
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct RefInstance {
    a: RefVal,
    b: RefVal,
    child_a: RefVal,
    has_child: bool,
}

impl RefInstance {
    fn get_slot(&self, slot: SlotSelector) -> RefVal {
        match slot {
            SlotSelector::PrimaryA => self.a,
            SlotSelector::PrimaryB => self.b,
            SlotSelector::ChildOrOpt => {
                if self.has_child {
                    self.child_a
                } else {
                    self.a
                }
            }
        }
    }
}

fn non_zero_divisor(raw: i8) -> i64 {
    match raw.rem_euclid(6) {
        0 => 2,
        1 => 3,
        2 => 5,
        3 => -2,
        4 => -3,
        _ => -5,
    }
}

impl AggregateCase {
    pub fn with_permuted(&self, permuted: bool) -> Self {
        let mut copy = self.clone();
        copy.permuted = permuted;
        copy
    }

    /// Whether this case uses `> 2^53` exact `parseInt` values that Node's IEEE-754 `f64`
    /// `parseInt` rounds.
    pub fn uses_wide_exact_int(&self) -> bool {
        self.overflow_origins
            || self.instances.iter().any(|inst| {
                inst.a.uses_wide_exact_int()
                    || inst.b.uses_wide_exact_int()
                    || (inst.has_child
                        && (inst.child_a.uses_wide_exact_int()
                            || inst.child_b.uses_wide_exact_int()))
            })
            || self.ops.iter().any(|op| {
                matches!(
                    op,
                    AggregateOp::MutateSlot { new_source, .. } if new_source.uses_wide_exact_int()
                )
            })
    }

    /// The WIT world definition for this case's export signature.
    pub fn wit_world(&self) -> &'static str {
        match self.wit_signature {
            WitSignature::ObservationRecord => {
                "package test:generated;\nworld generated {\n    record observation {\n        value: f64,\n        trace: list<f64>,\n    }\n    export run: func(x: f64) -> result<observation>;\n}\n"
            }
            WitSignature::MultiFieldRecord => {
                "package test:generated;\nworld generated {\n    record observation {\n        value: f64,\n        head: f64,\n        tail: f64,\n        trace: list<f64>,\n    }\n    export run: func(x: f64) -> result<observation>;\n}\n"
            }
            WitSignature::DirectList => {
                "package test:generated;\nworld generated {\n    export run: func(x: f64) -> result<list<f64>>;\n}\n"
            }
            WitSignature::OptionRecord => {
                "package test:generated;\nworld generated {\n    record observation {\n        value: f64,\n        maybe: option<f64>,\n        trace: list<f64>,\n    }\n    export run: func(x: f64) -> result<observation>;\n}\n"
            }
        }
    }

    /// Evaluate the exact expected `Observation` for input `x` using the Rust reference model.
    pub(crate) fn evaluate(&self, x: f64) -> Observation {
        let mut insts: Vec<RefInstance> = self
            .instances
            .iter()
            .map(|spec| RefInstance {
                a: spec.a.eval(x),
                b: spec.b.eval(x),
                child_a: spec.child_a.eval(x),
                has_child: spec.has_child,
            })
            .collect();

        let mut trace: Vec<f64> = Vec::new();
        let n = insts.len();

        for &op in &self.ops {
            match op {
                AggregateOp::ReadMod {
                    inst,
                    slot,
                    divisor,
                } => {
                    let v = insts[inst % n].get_slot(slot);
                    let d = non_zero_divisor(divisor);
                    trace.push(v.rem_plus_zero(d));
                    trace.push(v.json_probe_code());
                }
                AggregateOp::BranchRead {
                    inst_a,
                    inst_b,
                    slot,
                } => {
                    let chosen = if x >= 0.0 {
                        insts[inst_a % n]
                    } else {
                        insts[inst_b % n]
                    };
                    let v = chosen.get_slot(slot);
                    trace.push(v.rem_plus_zero(2));
                    trace.push(v.json_probe_code());
                    // `makeArithShape(2)` produces a `Shape` whose slots hold `3 * 2 = 6`.
                    trace.push(2.0);
                }
                AggregateOp::MutateSlot {
                    inst,
                    slot_b,
                    new_source,
                } => {
                    let v = new_source.eval(x);
                    let target = &mut insts[inst % n];
                    if slot_b {
                        target.b = v;
                    } else {
                        target.a = v;
                    }
                    trace.push(v.rem_plus_zero(2));
                }
                AggregateOp::ArrayConcatSpread { inst_a, inst_b } => {
                    let ia = insts[inst_a % n];
                    let ib = insts[inst_b % n];
                    for v in [ia.a, ia.b, ib.a, ib.b] {
                        trace.push(v.rem_plus_zero(2));
                    }
                }
                AggregateOp::SharedHelperCall { inst, slot_b } => {
                    let target = insts[inst % n];
                    let v = if slot_b { target.b } else { target.a };
                    trace.push(v.rem_plus_zero(2));
                }
                AggregateOp::CompareSlots {
                    inst_a,
                    slot_a_b,
                    inst_b,
                    slot_b_b,
                } => {
                    let va = if slot_a_b {
                        insts[inst_a % n].b
                    } else {
                        insts[inst_a % n].a
                    };
                    let vb = if slot_b_b {
                        insts[inst_b % n].b
                    } else {
                        insts[inst_b % n].a
                    };
                    trace.push(if va.lt_val(vb) { 1.0 } else { 0.0 });
                    trace.push(if va.eq_val(vb) { 1.0 } else { 0.0 });
                }
                AggregateOp::InspectSpecial { inst, slot_b } => {
                    let v = if slot_b {
                        insts[inst % n].b
                    } else {
                        insts[inst % n].a
                    };
                    let code = if v.is_nan() {
                        -99.0
                    } else if v.is_neg_zero() {
                        -88.0
                    } else {
                        v.rem_plus_zero(2)
                    };
                    trace.push(code);
                }
            }
        }

        if self.overflow_origins {
            // 36 distinct origins (`0..36`), last element has `a = 9007199254740993`, `9007199254740993 % 2 == 1`.
            trace.push(1.0);
        }

        let mut checksum = 0.0f64;
        for (idx, &item) in trace.iter().enumerate() {
            let term = if item.is_nan() { -55.0 } else { item + 0.0 };
            checksum += term * ((idx % 5) as f64 + 1.0);
        }

        // Append boundary-lifted fields (`head`, `tail`, `maybe`) after `finalizeAgg(trace)` so
        // Wasmtime's WIT lifting of those fields is directly verified in `Observation.trace`.
        match self.wit_signature {
            WitSignature::ObservationRecord | WitSignature::DirectList => {}
            WitSignature::MultiFieldRecord => {
                trace.push(insts[0].a.to_f64() + 0.0);
                trace.push(insts[n - 1].b.to_f64() + 0.0);
            }
            WitSignature::OptionRecord => {
                if insts[0].has_child {
                    trace.push(insts[0].child_a.to_f64() + 0.0);
                } else {
                    trace.push(-66.0);
                }
            }
        }

        Observation {
            value: number(checksum),
            trace: trace.into_iter().map(number).collect(),
        }
    }

    pub(crate) fn expected_outcome(&self) -> Outcome {
        Outcome::Values {
            observations: INPUTS.into_iter().map(|x| self.evaluate(x)).collect(),
            stdout: Vec::new(),
            stderr: Vec::new(),
            max_fuel_used: 0,
            component_bytes: 0,
            fuel_used: Vec::new(),
        }
    }
}

pub(crate) fn exact_int_seed_strategy() -> impl Strategy<Value = ExactIntSeed> {
    prop_oneof![
        Just(ExactIntSeed::TwoPow53Plus1),
        Just(ExactIntSeed::TwoPow53Plus3),
        Just(ExactIntSeed::NegTwoPow53Plus1),
        Just(ExactIntSeed::TwoPow54Plus3),
        Just(ExactIntSeed::I64MaxDec),
        Just(ExactIntSeed::I64MinDec),
        Just(ExactIntSeed::I64MaxHex),
        Just(ExactIntSeed::RadixWrapTwoPow53Plus1),
    ]
}

pub(crate) fn value_source_strategy() -> impl Strategy<Value = ValueSource> {
    prop_oneof![
        2 => (-4..=4i8).prop_map(ValueSource::FloatFraction),
        1 => Just(ValueSource::NegativeZero),
        1 => Just(ValueSource::NanValue),
        2 => (-30..=30i8).prop_map(ValueSource::NarrowInt),
        1 => Just(ValueSource::BoundaryInput),
        2 => (0..11i8).prop_map(ValueSource::IntArith),
        2 => (0..11i8).prop_map(ValueSource::HelperIntArith),
        3 => exact_int_seed_strategy().prop_map(ValueSource::ConstParseInt),
        3 => exact_int_seed_strategy().prop_map(ValueSource::RuntimeParseInt),
    ]
}

fn instance_spec_strategy() -> impl Strategy<Value = InstanceSpec> {
    (
        value_source_strategy(),
        value_source_strategy(),
        value_source_strategy(),
        value_source_strategy(),
        any::<bool>(),
    )
        .prop_map(|(a, b, child_a, child_b, has_child)| InstanceSpec {
            a,
            b,
            child_a,
            child_b,
            has_child,
        })
}

fn slot_selector_strategy() -> impl Strategy<Value = SlotSelector> {
    prop_oneof![
        Just(SlotSelector::PrimaryA),
        Just(SlotSelector::PrimaryB),
        Just(SlotSelector::ChildOrOpt),
    ]
}

fn aggregate_op_strategy() -> impl Strategy<Value = AggregateOp> {
    prop_oneof![
        4 => (0..5usize, slot_selector_strategy(), 0..6i8)
            .prop_map(|(inst, slot, divisor)| AggregateOp::ReadMod { inst, slot, divisor }),
        3 => (0..5usize, 0..5usize, slot_selector_strategy())
            .prop_map(|(inst_a, inst_b, slot)| AggregateOp::BranchRead {
                inst_a,
                inst_b,
                slot,
            }),
        2 => (0..5usize, any::<bool>(), value_source_strategy())
            .prop_map(|(inst, slot_b, new_source)| AggregateOp::MutateSlot {
                inst,
                slot_b,
                new_source,
            }),
        2 => (0..5usize, 0..5usize)
            .prop_map(|(inst_a, inst_b)| AggregateOp::ArrayConcatSpread { inst_a, inst_b }),
        3 => (0..5usize, any::<bool>())
            .prop_map(|(inst, slot_b)| AggregateOp::SharedHelperCall { inst, slot_b }),
        2 => (0..5usize, any::<bool>(), 0..5usize, any::<bool>())
            .prop_map(|(inst_a, slot_a_b, inst_b, slot_b_b)| AggregateOp::CompareSlots {
                inst_a,
                slot_a_b,
                inst_b,
                slot_b_b,
            }),
        2 => (0..5usize, any::<bool>())
            .prop_map(|(inst, slot_b)| AggregateOp::InspectSpecial { inst, slot_b }),
    ]
}

pub(crate) fn aggregate_shape_strategy() -> impl Strategy<Value = AggregateShape> {
    prop_oneof![
        Just(AggregateShape::FlatRecord),
        Just(AggregateShape::NestedRecord),
        Just(AggregateShape::SelfRecursive),
        Just(AggregateShape::MutuallyRecursive),
        Just(AggregateShape::RecordWithArrays),
    ]
}

pub(crate) fn wit_signature_strategy() -> impl Strategy<Value = WitSignature> {
    prop_oneof![
        Just(WitSignature::ObservationRecord),
        Just(WitSignature::MultiFieldRecord),
        Just(WitSignature::DirectList),
        Just(WitSignature::OptionRecord),
    ]
}

/// Strategy generating orthogonal aggregate specialization test cases.
pub(crate) fn aggregate_case_strategy() -> impl Strategy<Value = AggregateCase> {
    (
        aggregate_shape_strategy(),
        wit_signature_strategy(),
        prop::collection::vec(instance_spec_strategy(), 2..=5),
        prop::collection::vec(aggregate_op_strategy(), 3..=8),
        any::<bool>(),
    )
        .prop_map(
            |(shape, wit_signature, instances, ops, overflow_origins)| AggregateCase {
                shape,
                wit_signature,
                instances,
                ops,
                overflow_origins,
                permuted: false,
            },
        )
}

pub(crate) fn contract() -> Contract {
    let witnesses = vec![
        Witness {
            partition: "self-recursive-wide-parseint-and-float",
            case: Case::Aggregate(AggregateCase {
                shape: AggregateShape::SelfRecursive,
                wit_signature: WitSignature::ObservationRecord,
                instances: vec![
                    InstanceSpec {
                        a: ValueSource::ConstParseInt(ExactIntSeed::RadixWrapTwoPow53Plus1),
                        b: ValueSource::FloatFraction(0),
                        child_a: ValueSource::RuntimeParseInt(ExactIntSeed::TwoPow53Plus3),
                        child_b: ValueSource::FloatFraction(1),
                        has_child: true,
                    },
                    InstanceSpec {
                        a: ValueSource::FloatFraction(-1),
                        b: ValueSource::RuntimeParseInt(ExactIntSeed::TwoPow53Plus1),
                        child_a: ValueSource::NarrowInt(7),
                        child_b: ValueSource::ConstParseInt(ExactIntSeed::TwoPow53Plus1),
                        has_child: true,
                    },
                    InstanceSpec {
                        a: ValueSource::IntArith(0),
                        b: ValueSource::HelperIntArith(1),
                        child_a: ValueSource::NarrowInt(3),
                        child_b: ValueSource::NarrowInt(5),
                        has_child: false,
                    },
                ],
                ops: vec![
                    AggregateOp::ReadMod {
                        inst: 0,
                        slot: SlotSelector::PrimaryA,
                        divisor: 0,
                    },
                    AggregateOp::ReadMod {
                        inst: 0,
                        slot: SlotSelector::ChildOrOpt,
                        divisor: 1,
                    },
                    AggregateOp::BranchRead {
                        inst_a: 0,
                        inst_b: 2,
                        slot: SlotSelector::ChildOrOpt,
                    },
                    AggregateOp::SharedHelperCall {
                        inst: 0,
                        slot_b: false,
                    },
                    AggregateOp::SharedHelperCall {
                        inst: 1,
                        slot_b: true,
                    },
                    AggregateOp::CompareSlots {
                        inst_a: 0,
                        slot_a_b: false,
                        inst_b: 1,
                        slot_b_b: true,
                    },
                ],
                overflow_origins: false,
                permuted: false,
            }),
        },
        Witness {
            partition: "mutually-recursive-origin-overflow-multi-field",
            case: Case::Aggregate(AggregateCase {
                shape: AggregateShape::MutuallyRecursive,
                wit_signature: WitSignature::MultiFieldRecord,
                instances: vec![
                    InstanceSpec {
                        a: ValueSource::RuntimeParseInt(ExactIntSeed::TwoPow54Plus3),
                        b: ValueSource::FloatFraction(1),
                        child_a: ValueSource::ConstParseInt(ExactIntSeed::TwoPow53Plus1),
                        child_b: ValueSource::NarrowInt(-5),
                        has_child: true,
                    },
                    InstanceSpec {
                        a: ValueSource::FloatFraction(0),
                        b: ValueSource::ConstParseInt(ExactIntSeed::I64MaxDec),
                        child_a: ValueSource::NarrowInt(3),
                        child_b: ValueSource::FloatFraction(-2),
                        has_child: false,
                    },
                ],
                ops: vec![
                    AggregateOp::ArrayConcatSpread {
                        inst_a: 0,
                        inst_b: 1,
                    },
                    AggregateOp::BranchRead {
                        inst_a: 0,
                        inst_b: 1,
                        slot: SlotSelector::ChildOrOpt,
                    },
                    AggregateOp::MutateSlot {
                        inst: 1,
                        slot_b: false,
                        new_source: ValueSource::RuntimeParseInt(ExactIntSeed::TwoPow53Plus1),
                    },
                    AggregateOp::SharedHelperCall {
                        inst: 1,
                        slot_b: false,
                    },
                ],
                overflow_origins: true,
                permuted: false,
            }),
        },
        Witness {
            partition: "nested-record-option-boundary-special-values",
            case: Case::Aggregate(AggregateCase {
                shape: AggregateShape::NestedRecord,
                wit_signature: WitSignature::OptionRecord,
                instances: vec![
                    InstanceSpec {
                        a: ValueSource::NegativeZero,
                        b: ValueSource::NanValue,
                        child_a: ValueSource::RuntimeParseInt(ExactIntSeed::I64MaxHex),
                        child_b: ValueSource::NarrowInt(9),
                        has_child: true,
                    },
                    InstanceSpec {
                        a: ValueSource::BoundaryInput,
                        b: ValueSource::ConstParseInt(ExactIntSeed::I64MinDec),
                        child_a: ValueSource::FloatFraction(2),
                        child_b: ValueSource::NarrowInt(4),
                        has_child: false,
                    },
                ],
                ops: vec![
                    AggregateOp::InspectSpecial {
                        inst: 0,
                        slot_b: false,
                    },
                    AggregateOp::InspectSpecial {
                        inst: 0,
                        slot_b: true,
                    },
                    AggregateOp::ReadMod {
                        inst: 0,
                        slot: SlotSelector::ChildOrOpt,
                        divisor: 0,
                    },
                    AggregateOp::ReadMod {
                        inst: 1,
                        slot: SlotSelector::PrimaryB,
                        divisor: 0,
                    },
                ],
                overflow_origins: false,
                permuted: false,
            }),
        },
        Witness {
            partition: "record-with-arrays-direct-list-boundary",
            case: Case::Aggregate(AggregateCase {
                shape: AggregateShape::RecordWithArrays,
                wit_signature: WitSignature::DirectList,
                instances: vec![
                    InstanceSpec {
                        a: ValueSource::ConstParseInt(ExactIntSeed::NegTwoPow53Plus1),
                        b: ValueSource::BoundaryInput,
                        child_a: ValueSource::IntArith(0),
                        child_b: ValueSource::FloatFraction(0),
                        has_child: true,
                    },
                    InstanceSpec {
                        a: ValueSource::HelperIntArith(0),
                        b: ValueSource::RuntimeParseInt(ExactIntSeed::TwoPow53Plus3),
                        child_a: ValueSource::NegativeZero,
                        child_b: ValueSource::NarrowInt(1),
                        has_child: false,
                    },
                ],
                ops: vec![
                    AggregateOp::ArrayConcatSpread {
                        inst_a: 0,
                        inst_b: 1,
                    },
                    AggregateOp::BranchRead {
                        inst_a: 0,
                        inst_b: 1,
                        slot: SlotSelector::ChildOrOpt,
                    },
                    AggregateOp::CompareSlots {
                        inst_a: 0,
                        slot_a_b: false,
                        inst_b: 1,
                        slot_b_b: true,
                    },
                ],
                overflow_origins: false,
                permuted: false,
            }),
        },
    ];
    Contract {
        id: "ecma.aggregate.specialization",
        description: "Orthogonal aggregate layout specialization across recursive shapes, exact 64-bit parseInt, origin scaling, and WIT boundaries",
        specification: "https://tc39.es/ecma262/",
        domain: "Product of flat/nested/self-recursive/mutually-recursive/array-bearing records, multi-field/list/option WIT exports, float/NaN/-0.0/int-arith/wide-parseInt value sources, branch joins, shared helpers, array concat/spread, and >32-origin summarization",
        witnesses,
        strategy: aggregate_case_strategy().prop_map(Case::Aggregate).boxed(),
        check: check_equivalence,
    }
}
