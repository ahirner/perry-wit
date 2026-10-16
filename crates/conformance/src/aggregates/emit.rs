use super::{
    AggregateCase, AggregateOp, AggregateShape, ExactIntSeed, InstanceSpec, SlotSelector,
    ValueSource, WitSignature, int_arith_factor, non_zero_divisor,
};

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

impl AggregateCase {
    /// Emit the standalone TypeScript source for this case.
    pub fn source(&self) -> String {
        let mut out = String::new();
        self.emit_type_defs(&mut out);
        self.emit_helpers(&mut out);
        self.emit_run_function(&mut out);
        out
    }

    fn emit_type_defs(&self, out: &mut String) {
        let mut defs = Vec::new();
        match self.shape {
            AggregateShape::FlatRecord => {
                defs.push(
                    "interface Shape {\n  a: number;\n  b: number;\n  opt?: number;\n}\n"
                        .to_string(),
                );
            }
            AggregateShape::NestedRecord => {
                defs.push("interface Inner {\n  a: number;\n  b: number;\n}\n".to_string());
                defs.push("interface Shape {\n  inner: Inner;\n  opt?: number;\n}\n".to_string());
            }
            AggregateShape::SelfRecursive => {
                defs.push(
                    "interface Shape {\n  a: number;\n  b: number;\n  next?: Shape;\n}\n"
                        .to_string(),
                );
            }
            AggregateShape::MutuallyRecursive => {
                defs.push(
                    "interface PeerShape {\n  ca: number;\n  cb: number;\n  back?: Shape;\n}\n"
                        .to_string(),
                );
                defs.push(
                    "interface Shape {\n  a: number;\n  b: number;\n  peer?: PeerShape;\n}\n"
                        .to_string(),
                );
            }
            AggregateShape::RecordWithArrays => {
                defs.push(
                    "interface Shape {\n  items: number[];\n  extra?: number[];\n}\n".to_string(),
                );
            }
        }
        if self.permuted {
            for d in defs.iter().rev() {
                out.push_str(d);
                out.push('\n');
            }
        } else {
            for d in &defs {
                out.push_str(d);
                out.push('\n');
            }
        }
    }

    fn emit_helpers(&self, out: &mut String) {
        let get_a = match self.shape {
            AggregateShape::NestedRecord => "s.inner.a",
            AggregateShape::RecordWithArrays => "s.items[0]",
            _ => "s.a",
        };
        let get_b = match self.shape {
            AggregateShape::NestedRecord => "s.inner.b",
            AggregateShape::RecordWithArrays => "s.items[1]",
            _ => "s.b",
        };
        let get_c = match self.shape {
            AggregateShape::FlatRecord | AggregateShape::NestedRecord => {
                format!("(s.opt !== undefined ? s.opt : {get_a})")
            }
            AggregateShape::SelfRecursive => {
                format!("(s.next !== undefined ? s.next.a : {get_a})")
            }
            AggregateShape::MutuallyRecursive => {
                format!("(s.peer !== undefined ? s.peer.ca : {get_a})")
            }
            AggregateShape::RecordWithArrays => {
                format!("(s.extra !== undefined ? s.extra[0] : {get_a})")
            }
        };
        let set_a = match self.shape {
            AggregateShape::NestedRecord => "s.inner.a = v;",
            AggregateShape::RecordWithArrays => "s.items[0] = v;",
            _ => "s.a = v;",
        };
        let set_b = match self.shape {
            AggregateShape::NestedRecord => "s.inner.b = v;",
            AggregateShape::RecordWithArrays => "s.items[1] = v;",
            _ => "s.b = v;",
        };
        let arith_shape_body = match self.shape {
            AggregateShape::FlatRecord => "{ a: v, b: v, opt: v }",
            AggregateShape::NestedRecord => "{ inner: { a: v, b: v }, opt: v }",
            AggregateShape::SelfRecursive => "{ a: v, b: v, next: { a: v, b: v } }",
            AggregateShape::MutuallyRecursive => "{ a: v, b: v, peer: { ca: v, cb: v } }",
            AggregateShape::RecordWithArrays => "{ items: [v, v], extra: [v, v] }",
        };

        let helpers = [
            "function rtStr(s: string): string {\n  return s.slice(0);\n}\n".to_string(),
            "function rtRadix(r: number): number {\n  return r;\n}\n".to_string(),
            "function helperIntArith(k: number): number {\n  return rtStr(\"abc\").length * k;\n}\n"
                .to_string(),
            format!(
                "function makeArithShape(k: number): Shape {{\n  const v = rtStr(\"abc\").length * k;\n  return {arith_shape_body};\n}}\n"
            ),
            format!("function readSlotA(s: Shape): number {{\n  return {get_a};\n}}\n"),
            format!("function readSlotB(s: Shape): number {{\n  return {get_b};\n}}\n"),
            format!("function readSlotC(s: Shape): number {{\n  return {get_c};\n}}\n"),
            format!(
                "function writeSlot(s: Shape, slotB: boolean, v: number): void {{\n  if (slotB) {{\n    {set_b}\n  }} else {{\n    {set_a}\n  }}\n}}\n"
            ),
            "function sharedHelper(s: Shape, slotB: boolean): number {\n  return slotB ? readSlotB(s) : readSlotA(s);\n}\n"
                .to_string(),
            "function inspectSpecial(v: number): number {\n  if (Number.isNaN(v)) return -99;\n  if (v === 0 && (1 / v) === -Infinity) return -88;\n  return (v % 2) + 0;\n}\n"
                .to_string(),
            "function finalizeAgg(trace: number[]): number {\n  let sum = 0;\n  for (let i = 0; i < trace.length; i += 1) {\n    const item = trace[i];\n    const term = Number.isNaN(item) ? -55 : (item + 0);\n    sum += term * ((i % 5) + 1);\n  }\n  return sum;\n}\n"
                .to_string(),
        ];

        if self.permuted {
            for h in helpers.iter().rev() {
                out.push_str(h);
                out.push('\n');
            }
        } else {
            for h in &helpers {
                out.push_str(h);
                out.push('\n');
            }
        }
    }

    fn emit_instance_expr(&self, spec: &InstanceSpec) -> String {
        let a = spec.a.to_ts_expr();
        let b = spec.b.to_ts_expr();
        let ca = spec.child_a.to_ts_expr();
        let cb = spec.child_b.to_ts_expr();
        match self.shape {
            AggregateShape::FlatRecord => {
                if spec.has_child {
                    format!("{{ a: {a}, b: {b}, opt: {ca} }}")
                } else {
                    format!("{{ a: {a}, b: {b} }}")
                }
            }
            AggregateShape::NestedRecord => {
                if spec.has_child {
                    format!("{{ inner: {{ a: {a}, b: {b} }}, opt: {ca} }}")
                } else {
                    format!("{{ inner: {{ a: {a}, b: {b} }} }}")
                }
            }
            AggregateShape::SelfRecursive => {
                if spec.has_child {
                    format!("{{ a: {a}, b: {b}, next: {{ a: {ca}, b: {cb} }} }}")
                } else {
                    format!("{{ a: {a}, b: {b} }}")
                }
            }
            AggregateShape::MutuallyRecursive => {
                if spec.has_child {
                    format!("{{ a: {a}, b: {b}, peer: {{ ca: {ca}, cb: {cb} }} }}")
                } else {
                    format!("{{ a: {a}, b: {b} }}")
                }
            }
            AggregateShape::RecordWithArrays => {
                if spec.has_child {
                    format!("{{ items: [{a}, {b}], extra: [{ca}, {cb}] }}")
                } else {
                    format!("{{ items: [{a}, {b}] }}")
                }
            }
        }
    }

    fn emit_core_statements(&self, out: &mut String, indent: &str) {
        out.push_str(&format!("{indent}const trace: number[] = [];\n"));

        let decls: Vec<String> = self
            .instances
            .iter()
            .enumerate()
            .map(|(idx, spec)| {
                let expr = self.emit_instance_expr(spec);
                format!("{indent}const inst{idx}: Shape = {expr};\n")
            })
            .collect();

        if self.permuted {
            for d in decls.iter().rev() {
                out.push_str(d);
            }
        } else {
            for d in &decls {
                out.push_str(d);
            }
        }

        let n = self.instances.len();
        for (op_idx, &op) in self.ops.iter().enumerate() {
            match op {
                AggregateOp::ReadMod {
                    inst,
                    slot,
                    divisor,
                } => {
                    let i = inst % n;
                    let d = non_zero_divisor(divisor);
                    let read = match slot {
                        SlotSelector::PrimaryA => format!("readSlotA(inst{i})"),
                        SlotSelector::PrimaryB => format!("readSlotB(inst{i})"),
                        SlotSelector::ChildOrOpt => format!("readSlotC(inst{i})"),
                    };
                    let rm_var = format!("rm_{op_idx}");
                    let rm_json = format!("rmj_{op_idx}");
                    out.push_str(&format!("{indent}const {rm_var}: number = {read};\n"));
                    out.push_str(&format!("{indent}trace.push((({rm_var} % ({d})) + 0));\n"));
                    out.push_str(&format!(
                        "{indent}const {rm_json}: string = JSON.stringify({rm_var});\n"
                    ));
                    out.push_str(&format!(
                        "{indent}trace.push({rm_json} === \"9007199254740993\" ? 1 : ({rm_json} === \"6\" ? 2 : 0));\n"
                    ));
                }
                AggregateOp::BranchRead {
                    inst_a,
                    inst_b,
                    slot,
                } => {
                    let ia = inst_a % n;
                    let ib = inst_b % n;
                    let br_var = format!("br_{op_idx}");
                    let brv_var = format!("brv_{op_idx}");
                    let brj_var = format!("brj_{op_idx}");
                    let har_var = format!("har_{op_idx}");
                    let read_fn = match slot {
                        SlotSelector::PrimaryA => "readSlotA",
                        SlotSelector::PrimaryB => "readSlotB",
                        SlotSelector::ChildOrOpt => "readSlotC",
                    };
                    out.push_str(&format!(
                        "{indent}const {br_var}: Shape = x >= 0 ? inst{ia} : inst{ib};\n"
                    ));
                    out.push_str(&format!(
                        "{indent}const {brv_var}: number = {read_fn}({br_var});\n"
                    ));
                    out.push_str(&format!("{indent}trace.push((({brv_var} % 2) + 0));\n"));
                    out.push_str(&format!(
                        "{indent}const {brj_var}: string = JSON.stringify({brv_var});\n"
                    ));
                    out.push_str(&format!(
                        "{indent}trace.push({brj_var} === \"9007199254740993\" ? 1 : ({brj_var} === \"6\" ? 2 : 0));\n"
                    ));
                    out.push_str(&format!(
                        "{indent}const {har_var}: Shape = makeArithShape(2);\n"
                    ));
                    out.push_str(&format!(
                        "{indent}trace.push(JSON.stringify({read_fn}({har_var})) === \"6\" ? 2 : 0);\n"
                    ));
                }
                AggregateOp::MutateSlot {
                    inst,
                    slot_b,
                    new_source,
                } => {
                    let i = inst % n;
                    let expr = new_source.to_ts_expr();
                    out.push_str(&format!("{indent}writeSlot(inst{i}, {slot_b}, {expr});\n"));
                    let read = if slot_b {
                        format!("readSlotB(inst{i})")
                    } else {
                        format!("readSlotA(inst{i})")
                    };
                    out.push_str(&format!("{indent}trace.push((({read} % 2) + 0));\n"));
                }
                AggregateOp::ArrayConcatSpread { inst_a, inst_b } => {
                    let ia = inst_a % n;
                    let ib = inst_b % n;
                    let arr_var = format!("comb_{op_idx}");
                    out.push_str(&format!(
                        "{indent}const {arr_var}: number[] = [readSlotA(inst{ia}), readSlotB(inst{ia})].concat([readSlotA(inst{ib})], [...[readSlotB(inst{ib})]]);\n"
                    ));
                    out.push_str(&format!(
                        "{indent}for (let k = 0; k < {arr_var}.length; k += 1) {{\n{indent}  trace.push((({arr_var}[k] % 2) + 0));\n{indent}}}\n"
                    ));
                }
                AggregateOp::SharedHelperCall { inst, slot_b } => {
                    let i = inst % n;
                    out.push_str(&format!(
                        "{indent}trace.push(((sharedHelper(inst{i}, {slot_b}) % 2) + 0));\n"
                    ));
                }
                AggregateOp::CompareSlots {
                    inst_a,
                    slot_a_b,
                    inst_b,
                    slot_b_b,
                } => {
                    let ia = inst_a % n;
                    let ib = inst_b % n;
                    let ra = if slot_a_b {
                        format!("readSlotB(inst{ia})")
                    } else {
                        format!("readSlotA(inst{ia})")
                    };
                    let rb = if slot_b_b {
                        format!("readSlotB(inst{ib})")
                    } else {
                        format!("readSlotA(inst{ib})")
                    };
                    out.push_str(&format!("{indent}trace.push({ra} < {rb} ? 1 : 0);\n"));
                    out.push_str(&format!("{indent}trace.push({ra} === {rb} ? 1 : 0);\n"));
                }
                AggregateOp::InspectSpecial { inst, slot_b } => {
                    let i = inst % n;
                    let r = if slot_b {
                        format!("readSlotB(inst{i})")
                    } else {
                        format!("readSlotA(inst{i})")
                    };
                    out.push_str(&format!("{indent}trace.push(inspectSpecial({r}));\n"));
                }
            }
        }

        if self.overflow_origins {
            let spec = InstanceSpec {
                a: ValueSource::RuntimeParseInt(ExactIntSeed::TwoPow53Plus1),
                b: ValueSource::FloatFraction(0),
                child_a: ValueSource::NarrowInt(1),
                child_b: ValueSource::NarrowInt(2),
                has_child: false,
            };
            let overflow_expr = self.emit_instance_expr(&spec);
            out.push_str(&format!("{indent}const many: Shape[] = [\n"));
            for _ in 0..36 {
                out.push_str(&format!("{indent}  {overflow_expr},\n"));
            }
            out.push_str(&format!("{indent}];\n"));
            out.push_str(&format!(
                "{indent}trace.push(((readSlotA(many[35]) % 2) + 0));\n"
            ));
        }
    }

    fn emit_run_function(&self, out: &mut String) {
        let n = self.instances.len();
        let last = n - 1;
        match self.wit_signature {
            WitSignature::ObservationRecord => {
                out.push_str(
                    "export interface Observation {\n  value: number;\n  trace: number[];\n}\n\n",
                );
                out.push_str("export function run(x: number): Observation {\n");
                self.emit_core_statements(out, "  ");
                out.push_str("  return { value: finalizeAgg(trace), trace };\n}\n");
            }
            WitSignature::MultiFieldRecord => {
                out.push_str(
                    "export interface Observation {\n  value: number;\n  head: number;\n  tail: number;\n  trace: number[];\n}\n\n",
                );
                out.push_str("export function run(x: number): Observation {\n");
                self.emit_core_statements(out, "  ");
                out.push_str(&format!(
                    "  const head = readSlotA(inst0) + 0;\n  const tail = readSlotB(inst{last}) + 0;\n  return {{ value: finalizeAgg(trace), head, tail, trace }};\n}}\n"
                ));
            }
            WitSignature::DirectList => {
                out.push_str("export function run(x: number): number[] {\n");
                self.emit_core_statements(out, "  ");
                out.push_str(
                    "  const value = finalizeAgg(trace);\n  return [value].concat(trace);\n}\n",
                );
            }
            WitSignature::OptionRecord => {
                out.push_str(
                    "export interface Observation {\n  value: number;\n  maybe?: number;\n  trace: number[];\n}\n\n",
                );
                out.push_str("export function run(x: number): Observation {\n");
                self.emit_core_statements(out, "  ");
                if self.instances[0].has_child {
                    out.push_str(
                        "  const maybe = readSlotC(inst0) + 0;\n  return { value: finalizeAgg(trace), maybe, trace };\n}\n",
                    );
                } else {
                    out.push_str("  return { value: finalizeAgg(trace), trace };\n}\n");
                }
            }
        }
    }
}
