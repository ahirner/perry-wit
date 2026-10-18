//! TypeScript emission for [`AggregateCase`]; the Rust model in the parent module mirrors it.
use super::{
    AggregateCase, AggregateOp, AggregateShape, ExactIntSeed, Extra, SlotSelector, ValueSource,
    int_arith_factor, non_zero_divisor,
};
use crate::emit::{CHECKSUM_TS, Emitter};

impl ValueSource {
    pub(super) fn to_ts_expr(self) -> String {
        match self {
            Self::FloatFraction(k) => {
                let v = f64::from(k) * 2.0 + 0.5;
                format!("({v:?})")
            }
            Self::NegativeZero => "(-0.0)".to_string(),
            Self::NanValue => "parseInt(rtStr(\"invalid\"), 10)".to_string(),
            Self::NarrowInt(n) => format!("({n})"),
            Self::BoundaryInput => "x".to_string(),
            Self::IntArith(k) => {
                let factor = int_arith_factor(k);
                format!("(rtStr(\"abc\").length * {factor})")
            }
            Self::HelperIntArith(k) => {
                let factor = int_arith_factor(k);
                format!("helperIntArith({factor})")
            }
            Self::ConstParseInt(seed) => {
                let (s, radix) = seed.text_and_radix();
                format!("parseInt(\"{s}\", {radix})")
            }
            Self::RuntimeParseInt(seed) => {
                let (s, radix) = seed.text_and_radix();
                format!("parseInt(rtStr(\"{s}\"), rtRadix({radix}))")
            }
        }
    }
}

impl SlotSelector {
    /// The emitted helper reading this slot.
    fn reader(self) -> &'static str {
        match self {
            Self::PrimaryA => "readSlotA",
            Self::PrimaryB => "readSlotB",
            Self::ChildOrOpt => "readSlotC",
        }
    }

    /// The selector of primary slot `b` when `slot_b`, else `a`.
    fn primary(slot_b: bool) -> Self {
        if slot_b {
            Self::PrimaryB
        } else {
            Self::PrimaryA
        }
    }
}

impl AggregateShape {
    /// Interface declarations, dependencies first.
    fn type_defs(self) -> &'static [&'static str] {
        match self {
            Self::FlatRecord => &["interface Shape { a: number; b: number; opt?: number; }"],
            Self::NestedRecord => &[
                "interface Inner { a: number; b: number; }",
                "interface Shape { inner: Inner; opt?: number; }",
            ],
            Self::SelfRecursive => &["interface Shape { a: number; b: number; next?: Shape; }"],
            Self::MutuallyRecursive => &[
                "interface PeerShape { ca: number; cb: number; back?: Shape; }",
                "interface Shape { a: number; b: number; peer?: PeerShape; }",
            ],
            Self::RecordWithArrays => &["interface Shape { items: number[]; extra?: number[]; }"],
        }
    }

    /// Paths of the two primary slots of `s`.
    fn primaries(self) -> (&'static str, &'static str) {
        match self {
            Self::NestedRecord => ("s.inner.a", "s.inner.b"),
            Self::RecordWithArrays => ("s.items[0]", "s.items[1]"),
            _ => ("s.a", "s.b"),
        }
    }

    /// The optional child of `s` and the path of its first slot.
    fn child(self) -> (&'static str, &'static str) {
        match self {
            Self::FlatRecord | Self::NestedRecord => ("s.opt", "s.opt"),
            Self::SelfRecursive => ("s.next", "s.next.a"),
            Self::MutuallyRecursive => ("s.peer", "s.peer.ca"),
            Self::RecordWithArrays => ("s.extra", "s.extra[0]"),
        }
    }

    /// A `Shape` literal with primaries `a`, `b` and, when given, child slots.
    fn literal(self, a: &str, b: &str, child: Option<(&str, &str)>) -> String {
        let primaries = match self {
            Self::NestedRecord => format!("inner: {{ a: {a}, b: {b} }}"),
            Self::RecordWithArrays => format!("items: [{a}, {b}]"),
            _ => format!("a: {a}, b: {b}"),
        };
        let child = match (self, child) {
            (_, None) => String::new(),
            (Self::FlatRecord | Self::NestedRecord, Some((ca, _))) => format!(", opt: {ca}"),
            (Self::SelfRecursive, Some((ca, cb))) => format!(", next: {{ a: {ca}, b: {cb} }}"),
            (Self::MutuallyRecursive, Some((ca, cb))) => {
                format!(", peer: {{ ca: {ca}, cb: {cb} }}")
            }
            (Self::RecordWithArrays, Some((ca, cb))) => format!(", extra: [{ca}, {cb}]"),
        };
        format!("{{ {primaries}{child} }}")
    }
}

impl AggregateCase {
    /// Emit the standalone TypeScript source for this case.
    pub fn source(&self) -> String {
        let mut e = Emitter::default();
        e.ordered(
            self.shape.type_defs().iter().map(|def| def.to_string()),
            self.permuted,
        );
        e.ordered(self.helpers(), self.permuted);
        let interface = self.wit_signature.ts_interface();
        let output = match &interface {
            Some(interface) => {
                e.line(format_args!("{interface}"));
                "Observation"
            }
            None => "number[]",
        };
        e.block(
            format_args!("export function run(x: number): {output}"),
            |e| {
                self.emit_statements(e);
                self.emit_return(e);
            },
        );
        e.finish()
    }

    fn helpers(&self) -> Vec<String> {
        let shape = self.shape;
        let (a, b) = shape.primaries();
        let (guard, child) = shape.child();
        vec![
            "function rtStr(s: string): string { return s.slice(0); }".into(),
            "function rtRadix(r: number): number { return r; }".into(),
            "function helperIntArith(k: number): number { return rtStr(\"abc\").length * k; }"
                .into(),
            format!(
                "function makeArithShape(k: number): Shape {{ const v = rtStr(\"abc\").length * k; return {}; }}",
                shape.literal("v", "v", Some(("v", "v")))
            ),
            format!("function readSlotA(s: Shape): number {{ return {a}; }}"),
            format!("function readSlotB(s: Shape): number {{ return {b}; }}"),
            format!(
                "function readSlotC(s: Shape): number {{ return {guard} !== undefined ? {child} : {a}; }}"
            ),
            format!(
                "function writeSlot(s: Shape, slotB: boolean, v: number): void {{ if (slotB) {{ {b} = v; }} else {{ {a} = v; }} }}"
            ),
            "function sharedHelper(s: Shape, slotB: boolean): number { return slotB ? readSlotB(s) : readSlotA(s); }".into(),
            "function inspectSpecial(v: number): number { if (Number.isNaN(v)) return -99; if (v === 0 && (1 / v) === -Infinity) return -88; return (v % 2) + 0; }".into(),
            CHECKSUM_TS.into(),
        ]
    }

    fn emit_statements(&self, e: &mut Emitter) {
        e.line(format_args!("const trace: number[] = [];"));
        e.ordered(
            self.instances.iter().enumerate().map(|(index, spec)| {
                let child = spec
                    .has_child
                    .then(|| (spec.child_a.to_ts_expr(), spec.child_b.to_ts_expr()));
                let literal = self.shape.literal(
                    &spec.a.to_ts_expr(),
                    &spec.b.to_ts_expr(),
                    child.as_ref().map(|(ca, cb)| (ca.as_str(), cb.as_str())),
                );
                format!("const inst{index}: Shape = {literal};")
            }),
            self.permuted,
        );
        let n = self.instances.len();
        for (index, &op) in self.ops.iter().enumerate() {
            emit_op(e, index, op, n);
        }
        if self.overflow_origins {
            let item = self.shape.literal(
                &ValueSource::RuntimeParseInt(ExactIntSeed::TwoPow53Plus1).to_ts_expr(),
                &ValueSource::FloatFraction(0).to_ts_expr(),
                None,
            );
            e.line(format_args!(
                "const many: Shape[] = [{}];",
                [item.as_str(); 4].join(", ")
            ));
            e.line(format_args!("trace.push(((readSlotA(many[3]) % 2) + 0));"));
        }
    }

    /// Returns the checksummed trace through this case's WIT signature.
    fn emit_return(&self, e: &mut Emitter) {
        match self.wit_signature.extras() {
            None => e.line(format_args!("return [checksum(0, trace)].concat(trace);")),
            Some(extras) => {
                let last = self.instances.len() - 1;
                let fields = extras
                    .iter()
                    .map(|&extra| {
                        let value = match extra {
                            Extra::Head => "readSlotA(inst0) + 0".to_string(),
                            Extra::Tail => format!("readSlotB(inst{last}) + 0"),
                            Extra::Maybe if self.instances[0].has_child => {
                                "readSlotC(inst0) + 0".to_string()
                            }
                            // Written out so the Node oracle sees the field and records -66.
                            Extra::Maybe => "undefined".to_string(),
                        };
                        format!("{}: {value}, ", extra.name())
                    })
                    .collect::<String>();
                e.line(format_args!(
                    "return {{ value: checksum(0, trace), {fields}trace }};"
                ));
            }
        }
    }
}

/// Emits one operation over the `n` instances; `index` keeps its locals unique.
fn emit_op(e: &mut Emitter, index: usize, op: AggregateOp, n: usize) {
    match op {
        AggregateOp::ReadMod {
            inst,
            slot,
            divisor,
        } => {
            let value = format!("rm_{index}");
            e.line(format_args!(
                "const {value}: number = {}(inst{});",
                slot.reader(),
                inst % n
            ));
            e.line(format_args!(
                "trace.push((({value} % ({})) + 0));",
                non_zero_divisor(divisor)
            ));
            emit_json_probe(e, &value);
        }
        AggregateOp::BranchRead {
            inst_a,
            inst_b,
            slot,
        } => {
            let reader = slot.reader();
            let value = format!("brv_{index}");
            e.line(format_args!(
                "const br_{index}: Shape = x >= 0 ? inst{} : inst{};",
                inst_a % n,
                inst_b % n
            ));
            e.line(format_args!(
                "const {value}: number = {reader}(br_{index});"
            ));
            e.line(format_args!("trace.push((({value} % 2) + 0));"));
            emit_json_probe(e, &value);
            e.line(format_args!(
                "const har_{index}: Shape = makeArithShape(2);"
            ));
            e.line(format_args!(
                "trace.push(JSON.stringify({reader}(har_{index})) === \"6\" ? 2 : 0);"
            ));
        }
        AggregateOp::MutateSlot {
            inst,
            slot_b,
            new_source,
        } => {
            let i = inst % n;
            e.line(format_args!(
                "writeSlot(inst{i}, {slot_b}, {});",
                new_source.to_ts_expr()
            ));
            e.line(format_args!(
                "trace.push((({}(inst{i}) % 2) + 0));",
                SlotSelector::primary(slot_b).reader()
            ));
        }
        AggregateOp::ArrayConcatSpread { inst_a, inst_b } => {
            let (a, b) = (inst_a % n, inst_b % n);
            e.line(format_args!(
                "const comb_{index}: number[] = [readSlotA(inst{a}), readSlotB(inst{a})].concat([readSlotA(inst{b})], [...[readSlotB(inst{b})]]);"
            ));
            e.block(
                format_args!("for (let k = 0; k < comb_{index}.length; k += 1)"),
                |e| e.line(format_args!("trace.push(((comb_{index}[k] % 2) + 0));")),
            );
        }
        AggregateOp::SharedHelperCall { inst, slot_b } => e.line(format_args!(
            "trace.push(((sharedHelper(inst{}, {slot_b}) % 2) + 0));",
            inst % n
        )),
        AggregateOp::CompareSlots {
            inst_a,
            slot_a_b,
            inst_b,
            slot_b_b,
        } => {
            let left = format!(
                "{}(inst{})",
                SlotSelector::primary(slot_a_b).reader(),
                inst_a % n
            );
            let right = format!(
                "{}(inst{})",
                SlotSelector::primary(slot_b_b).reader(),
                inst_b % n
            );
            e.line(format_args!("trace.push({left} < {right} ? 1 : 0);"));
            e.line(format_args!("trace.push({left} === {right} ? 1 : 0);"));
        }
        AggregateOp::InspectSpecial { inst, slot_b } => e.line(format_args!(
            "trace.push(inspectSpecial({}(inst{})));",
            SlotSelector::primary(slot_b).reader(),
            inst % n
        )),
    }
}

/// Traces whether `JSON.stringify(value)` prints `2^53 + 1` exactly (1), `6` (2), or else (0).
fn emit_json_probe(e: &mut Emitter, value: &str) {
    e.line(format_args!(
        "const {value}_json: string = JSON.stringify({value});"
    ));
    e.line(format_args!(
        "trace.push({value}_json === \"9007199254740993\" ? 1 : ({value}_json === \"6\" ? 2 : 0));"
    ));
}
