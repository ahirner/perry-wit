//! Component exports end to end: TypeScript compiled against a WIT world, instantiated with
//! wasmtime's component model, called with WIT values (canonical ABI lift and lower).
use crate::world::Compiler::Standard;
use anyhow::Result;
use perry_wit::compile_for_world;
use wasmtime::{
    Engine, ResourceLimiter, Store,
    component::{Component, ComponentNamedList, ComponentType, Lift, Linker, Lower, Val},
};

macro_rules! boundary {
    ($wit:literal) => {
        const WIT: &str = $wit;
        wasmtime::component::bindgen!({ inline: $wit, world: "boundary", additional_derives: [PartialEq] });
    };
}

/// Records the largest linear memory the guest asks for.
#[derive(Default)]
struct Peak(usize);

#[test]
fn included_worlds_share_types_and_preserve_selected_exports() -> Result<()> {
    let wit = r#"
package test:world-selection;
interface shared {
  record value { count: u32, label: string }
}
world base {
  use shared.{value};
  export snapshot: func(count: u32) -> value;
}
world composed {
  include base;
  export double: func(count: u32) -> u32;
}
"#;
    let source = "
interface Value { count: number; label: string }
export function snapshot(count: number): Value { return { count, label: 'snapshot' }; }
export function double(count: number): number { return count * 2; }
";
    let engine = Engine::default();
    for world in ["base", "composed"] {
        let bytes = Standard.component(wit, world, "selection.ts", source)?;
        let (mut store, instance) = crate::world::instantiate(&engine, &bytes)?;
        let snapshot = instance.get_func(&mut store, "snapshot").unwrap();
        let mut result = [Val::Bool(false)];
        snapshot.call(&mut store, &[Val::U32(7)], &mut result)?;
        assert_eq!(
            result[0],
            Val::Record(vec![
                ("count".into(), Val::U32(7)),
                ("label".into(), Val::String("snapshot".into()))
            ])
        );
        let double = instance.get_func(&mut store, "double");
        assert_eq!(double.is_some(), world == "composed");
        if let Some(double) = double {
            double.call(&mut store, &[Val::U32(7)], &mut result)?;
            assert_eq!(result[0], Val::U32(14));
        }
    }
    Ok(())
}

#[test]
fn unbound_exports_do_not_require_async_hosts() -> Result<()> {
    let mut component = Instance::compile(
        "package test:reachability; world boundary {export run:func()->result<u32>;}",
        r#"
        function initial(n:number):number { if(n <= 0) return 7; return initial(n - 1) + 1; }
        let count = initial(3);
        export function run():number { return ++count; }
        export async function unused():Promise<void> { console.log('not in this world'); }
        "#,
    )?;
    component.check("run", (), &Ok::<u32, ()>(11))?;
    component.check("run", (), &Ok::<u32, ()>(12))?;
    Ok(())
}

#[test]
fn indirect_parameters_lift_nested_values_and_release_storage() -> Result<()> {
    boundary!(
        "package test:indirect; world boundary {record input {a:string,b:string,c:string,d:string,e:string,f:string,g:string,h:string,i:option<string>,n:u64} export run:func(value:input)->string;}"
    );
    let mut component = Instance::compile(
        WIT,
        "import type { Input } from './types/world'; export function run(value:Input):string {return value.a+value.b+value.c+value.d+value.e+value.f+value.g+value.h+value.i+String(value.n);}",
    )?;
    let value = Input {
        a: "a".into(),
        b: "b".into(),
        c: "c".into(),
        d: "d".into(),
        e: "e".into(),
        f: "f".into(),
        g: "g".into(),
        h: "h".into(),
        i: Some("🙂".into()),
        n: u64::MAX,
    };
    let expected = "abcdefgh🙂18446744073709551615".to_owned();
    for _ in 0..1000 {
        component.check("run", (&value,), &expected)?;
    }
    assert!(component.store.data().0 <= 131072);
    Ok(())
}

impl ResourceLimiter for Peak {
    fn memory_growing(
        &mut self,
        _: usize,
        desired: usize,
        _: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.0 = self.0.max(desired);
        Ok(true)
    }

    fn table_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

boundary!(
    r#"
package test:boundary;
world boundary {
  record point { x: f64, y-coord: f64 }
  record summary { count: u32, total: f64, label: string, positive: list<bool> }
  export scale: func(p: point, factor: f64) -> point;
  export describe: func(name: string, values: list<f64>) -> summary;
  export count: func(items: list<string>) -> u32;
  export greet: func(name: string) -> string;
  export negate: func(flag: bool) -> bool;
  export wide: func(a: s64, b: u8, c: f32) -> f64;
  export nested: func(rows: list<list<f64>>) -> list<f64>;
}
"#
);

const SOURCE: &str = r#"
interface Point { x: number; yCoord: number }
interface Summary { count: number; total: number; label: string; positive: boolean[] }
export function scale(p: Point, factor: number): Point {
  return { x: p.x * factor, yCoord: p.yCoord * factor };
}
export function describe(name: string, values: number[]): Summary {
  let total = 0;
  const positive: boolean[] = [];
  for (let i = 0; i < values.length; i++) {
    total = total + values[i];
    positive.push(values[i] > 0);
  }
  return { count: values.length, total: total, label: name + "!", positive: positive };
}
export function count(items: string[]): number {
  let n = 0;
  for (let i = 0; i < items.length; i++) {
    n = n + items[i].length;
  }
  return n;
}
export function greet(name: string): string {
  return "héllo " + name;
}
export function negate(flag: boolean): boolean {
  return !flag;
}
export function wide(a: bigint, b: number, c: number): number {
  return Number(a) + b + c;
}
export function nested(rows: number[][]): number[] {
  const sums: number[] = [];
  for (let i = 0; i < rows.length; i++) {
    const row = rows[i];
    let sum = 0;
    for (let j = 0; j < row.length; j++) {
      sum = sum + row[j];
    }
    sums.push(sum);
  }
  return sums;
}
"#;

struct Instance {
    store: Store<Peak>,
    instance: wasmtime::component::Instance,
}

#[test]
fn exact_integer_transport_in_nested_results_and_options() -> Result<()> {
    boundary!(
        r#"package test:integers;
world boundary {
  record packet { signed: s64, unsigned: u64, optional: option<u64> }
  export echo: func(value: packet) -> result<packet, string>;
  export extremes: func() -> list<s64>;
  export describe-wide: func(value: u64) -> string;
  export throw-wide: func(value: s64) -> result<s64, s64>;
}"#
    );
    let source = r#"
interface Packet { signed: bigint; unsigned: bigint; optional: bigint | undefined }
type Answer = { ok: true; value: Packet } | { ok: false; error: string };
export function echo(value: Packet): Answer {
  if (value.signed === -9223372036854775808n && value.unsigned === 18446744073709551615n) {
    return { ok: true, value: value };
  }
  return { ok: false, error: 'wrong bits' };
}
export function extremes(): bigint[] { return [-9223372036854775808n, -1n, 0n, 9223372036854775807n]; }
export function describeWide(value: bigint): string { return typeof value + ':' + String(value); }
export function throwWide(value: bigint): bigint { throw -value; }
"#;
    let mut instance = Instance::compile(WIT, source)?;
    for optional in [None, Some(9_007_199_254_740_993)] {
        let packet = Packet {
            signed: i64::MIN,
            unsigned: u64::MAX,
            optional,
        };
        assert_eq!(
            instance.call::<_, Result<Packet, String>>("echo", (&packet,))?,
            Ok(packet)
        );
    }
    instance.check("extremes", (), &vec![i64::MIN, -1, 0, i64::MAX])?;
    instance.check(
        "describe-wide",
        (u64::MAX,),
        &"bigint:18446744073709551615".to_owned(),
    )?;
    instance.check(
        "throw-wide",
        (9_007_199_254_740_993i64,),
        &Err::<i64, _>(-9_007_199_254_740_993i64),
    )?;
    Ok(())
}

impl Instance {
    fn new() -> Result<Self> {
        Self::compile(WIT, SOURCE)
    }

    fn compile(wit: &str, source: &str) -> Result<Self> {
        let engine = Engine::default();
        let component = Component::new(
            &engine,
            Standard.component(wit, "boundary", "boundary.ts", source)?,
        )?;
        let mut store = Store::new(&engine, Peak::default());
        store.limiter(|peak| peak);
        let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
        Ok(Instance { store, instance })
    }

    fn check<P, R>(&mut self, name: &str, args: P, expected: &R) -> Result<()>
    where
        P: ComponentNamedList + Lower,
        R: ComponentType + Lift + std::fmt::Debug + PartialEq,
    {
        assert_eq!(&self.call::<P, R>(name, args)?, expected, "{name}");
        Ok(())
    }

    fn call<P, R>(&mut self, name: &str, args: P) -> Result<R>
    where
        P: ComponentNamedList + Lower,
        R: ComponentType + Lift,
    {
        let run = self
            .instance
            .get_typed_func::<P, (R,)>(&mut self.store, name)?;
        Ok(run.call(&mut self.store, args)?.0)
    }
}

#[test]
fn canonical_inputs_survive_their_call_and_collectible_churn() -> Result<()> {
    let mut component = Instance::compile(
        r#"
package test:owners;
world boundary {
    export remember: func(text: string, bytes: list<u8>) -> result<u32>;
    export recall: func(n: u32) -> result<string>;
    export bytes: func() -> result<list<u8>>;
}
"#,
        r#"
const items: { text: string }[] = [];
for (let i = 0; i < 200; i++) items.push({ text: "item " + i });
let saved = "";
let data = new Uint8Array(0);
export function remember(text: string, bytes: Uint8Array): number {
    if (items.length !== 200) throw new Error("init missing");
    saved = text.slice(1); data = bytes.subarray(1); data[0] = 255;
    return data.length;
}
export function recall(n: number): string {
    let last = "";
    for (let i = 0; i < n; i++) last = "temporary:" + i;
    return saved + ":" + data[0] + ":" + last;
}
export function bytes(): Uint8Array { return data; }
"#,
    )?;
    component.check("remember", ("!hé🙂", &[1u8, 2, 3][..]), &Ok::<u32, ()>(2))?;
    component.check(
        "recall",
        (200_000u32,),
        &Ok::<_, ()>("hé🙂:255:temporary:199999".to_owned()),
    )?;
    component.check("bytes", (), &Ok::<_, ()>(vec![255u8, 3]))?;
    Ok(())
}

#[test]
fn utf8_strings_lower_to_nested_byte_lists() -> Result<()> {
    boundary!(
        r#"
package test:bytes;
world boundary {
    record payload { text: list<u8>, parts: list<list<u8>> }
    export encode: func(text: string) -> payload;
}

"#
    );
    let mut component = Instance::compile(
        WIT,
        r#"
interface Payload { text: string; parts: string[] }
export function encode(text: string): Payload { return { text, parts: [text, "é🙂"] }; }
"#,
    )?;
    component.check(
        "encode",
        ("a✓",),
        &Payload {
            text: "a✓".as_bytes().to_vec(),
            parts: vec!["a✓".as_bytes().to_vec(), "é🙂".as_bytes().to_vec()],
        },
    )?;
    Ok(())
}

/// A WIT record value from `(field, value)` pairs, and repeated calls on one instance release their memory.
#[test]
fn records_strings_lists_and_scalars_cross_the_boundary() -> Result<()> {
    let mut component = Instance::new()?;
    component.check(
        "scale",
        (
            &Point {
                x: 1.5,
                y_coord: -2.0,
            },
            4.0,
        ),
        &Point {
            x: 6.0,
            y_coord: -8.0,
        },
    )?;
    component.check(
        "describe",
        ("sum ✓", &[1.0, -2.5, 4.0][..]),
        &Summary {
            count: 3,
            total: 2.5,
            label: "sum ✓!".into(),
            positive: vec![true, false, true],
        },
    )?;
    // `.length` counts Unicode scalars: both "🙂" and "é" have length one.
    component.check("count", (&["ab", "🙂", "é", ""][..],), &4u32)?;
    component.check("greet", ("wörld",), &"héllo wörld".to_owned())?;
    component.check("negate", (true,), &false)?;
    component.check("wide", (-5i64, 200u8, 0.5f32), &195.5f64)?;
    component.check(
        "nested",
        (&[vec![1.0, 2.0], vec![], vec![-3.0]][..],),
        &vec![3.0, 0.0, -3.0],
    )?;
    let numbers: Vec<_> = (0..20_000).map(f64::from).collect();
    for _ in 0..3 {
        component.call::<_, Summary>("describe", ("warm", &numbers))?;
    }
    let before = component.store.data().0;
    for _ in 0..50 {
        component.call::<_, Summary>("describe", ("again", &numbers))?;
    }
    assert_eq!(component.store.data().0, before, "memory grew across tasks");
    Ok(())
}

#[test]
fn tagged_results_options_and_enums_cross_nested_boundaries() -> Result<()> {
    boundary!(
        r#"
package test:sums;
world boundary {
    type answer = result<f64, string>;
    type nothing = result;
    enum color { red, blue }
    record packet { answer: answer, color: color, note: option<string>, history: list<answer> }
    export inspect: func(value: packet) -> packet;
    export make: func(ok: bool) -> answer;
    export finish: func(ok: bool) -> nothing;
}
"#
    );
    let mut component = Instance::compile(
        WIT,
        r#"
import type { Answer, Nothing, Packet } from './types/world';
export function inspect(value: Packet): Packet { return value; }
export function make(ok: boolean): Answer { if (ok) return { ok: true, value: 42 }; return { ok: false, error: "failed" }; }
export function finish(ok: boolean): Nothing { if (ok) return { ok: true }; return { ok: false }; }
"#,
    )?;
    let ok = Ok::<_, String>(42.0);
    let err = Err::<f64, _>("failed".to_owned());
    for note in [None, Some("note".to_owned())] {
        let packet = Packet {
            answer: err.clone(),
            color: Color::Blue,
            note,
            history: vec![ok.clone(), err.clone()],
        };
        component.check("inspect", (&packet,), &packet)?;
    }
    component.check("make", (true,), &ok)?;
    component.check("make", (false,), &err)?;
    component.check("finish", (true,), &Ok::<(), ()>(()))?;
    component.check("finish", (false,), &Err::<(), ()>(()))?;
    Ok(())
}

#[test]
fn completion_results_map_returns_and_checked_throws() -> Result<()> {
    let mut component = Instance::compile(
        r#"
package test:completion;
world boundary {
    export run: func(fail: bool) -> result<string, string>;
    export nothing: func(fail: bool) -> result;
}
"#,
        r#"
export function run(fail: boolean): string { if (fail) throw "failed"; return "done"; }
export function nothing(fail: boolean): void { if (fail) throw 3; }
"#,
    )?;
    component.check("run", (false,), &Ok::<_, String>("done".to_owned()))?;
    component.check("run", (true,), &Err::<String, _>("failed".to_owned()))?;
    component.check("nothing", (false,), &Ok::<(), ()>(()))?;
    component.check("nothing", (true,), &Err::<(), ()>(()))?;
    Ok(())
}

#[test]
fn recursive_wit_input_accepts_strings_and_mutable_bytes() -> Result<()> {
    boundary!(
        r#"
package test:input;
world boundary {
    record payload { text: list<u8>, parts: list<list<u8>> }
    export encode: func(text: string, bytes: list<u8>, text-first: bool) -> payload;
    export regular: func(bytes: list<u8>) -> payload;
}
"#
    );
    let mut component = Instance::compile(
        WIT,
        r#"
import type { Payload, PayloadInput, WitInput } from './types/world';
interface Ordinary { text: Uint8Array; parts: Uint8Array[] }
function ordinary(value: Ordinary): WitInput<Payload> { return value; }
export function regular(bytes: Uint8Array): PayloadInput {
    const record: Ordinary = {text: bytes, parts: [bytes]};
    return ordinary(record);
}
function make(text: string, bytes: Uint8Array, textFirst: boolean): WitInput<Payload> {
    let selected: WitInput<Uint8Array> = bytes;
    if (textFirst) selected = text;
    return { text: selected, parts: [text, bytes] };
}
export function encode(text: string, bytes: Uint8Array, textFirst: boolean): PayloadInput {
    return make(text, bytes, textFirst);
}
"#,
    )?;
    let bytes = |text: &str| text.as_bytes().to_vec();
    component.check(
        "regular",
        ("é🙂".as_bytes(),),
        &Payload {
            text: bytes("é🙂"),
            parts: vec![bytes("é🙂")],
        },
    )?;
    for text_first in [true, false] {
        component.check(
            "encode",
            ("é🙂", b"xyz".as_slice(), text_first),
            &Payload {
                text: bytes(if text_first { "é🙂" } else { "xyz" }),
                parts: vec![bytes("é🙂"), bytes("xyz")],
            },
        )?;
    }
    Ok(())
}

#[test]
fn typed_json_decodes_closed_native_records_and_dense_arrays() -> Result<()> {
    let mut component = Instance::compile(
        r#"
package test:json;
world boundary { export decode: func(text: string) -> result<string, string>; }
"#,
        r#"
interface Row { count: number; label: string; active: boolean; optional: number | null | undefined }
interface Document { rows: Row[]; title: string }
export function decode(text: string): string {
    try {
        const raw = JSON.parse(text);
        const decoded: Document = raw;
        if (decoded.rows.length > 0) decoded.rows[0].count = 99;
        const again: Document = raw;
        return JSON.stringify(decoded) + ":" + JSON.stringify(again);
    } catch (e) { throw "invalid schema"; }
}
"#,
    )?;
    let good = r#"{"rows":[{"count":1,"label":"é","active":true}],"title":"ok"}"#;
    component.check("decode", (good,), &Ok::<_, String>(r#"{"rows":[{"count":99,"label":"é","active":true}],"title":"ok"}:{"rows":[{"count":1,"label":"é","active":true}],"title":"ok"}"#.to_owned()))?;
    let invalid_schema = Err::<String, _>("invalid schema".to_owned());
    for invalid in [
        "null",
        "[]",
        "{}",
        r#"{"rows":{},"title":"x"}"#,
        r#"{"rows":[{"count":null,"label":"x","active":true}],"title":"x"}"#,
        r#"{"rows":[{"count":1,"label":false,"active":true}],"title":"x"}"#,
    ] {
        component.check("decode", (invalid,), &invalid_schema)?;
    }
    Ok(())
}

#[test]
fn recursive_json_schemas_produce_independent_checked_native_graphs() -> Result<()> {
    let mut component = Instance::compile(
        r#"
        package test:recursive-json;
        world boundary { export decode: func(text: string, iterations: u32) -> string; }
    "#,
        r#"
        interface Node { value: number; children: Node[]; next: Node | null }
        function perry_json_parse_0(n:number):number {return n;}
        function perry_json_cursor_1(n:number):number {return n;}
        function perry_json_decode_3(n:number):number {return n;}
        function sum(node: Node): number {
            let value = node.value;
            for (let i = 0; i < node.children.length; i++) value += sum(node.children[i]);
            if (node.next !== null) value += sum(node.next);
            return value;
        }
        export function decode(text: string, iterations: number): string {
            try {
                let total = 0;
                for (let i = 0; i < iterations; i++) {
                    const first: Node = JSON.parse(text);
                    const second: Node = JSON.parse(text);
                    const snapshot = JSON.parse(text);
                    const view: Node = snapshot;
                    if (sum(view) !== sum(second)) throw new Error('snapshot differs');
                    first.value += 100;
                    total = sum(first) + sum(second);
                }
                return String(perry_json_parse_0(perry_json_cursor_1(perry_json_decode_3(total))));
            } catch (error) { return error.name; }
        }
    "#,
    )?;
    let leaf = serde_json::json!({"value": 2, "children": [], "next": null});
    let tree =
        serde_json::json!({"value": 3, "children": [leaf.clone(), leaf.clone()], "next": leaf});
    component.check("decode", (&tree.to_string(), 1000u32), &"118".to_owned())?;
    let invalid = serde_json::json!({"value": 3, "children": [{"value": "wrong", "children": [], "next": null}], "next": null});
    component.check(
        "decode",
        (&invalid.to_string(), 1u32),
        &"TypeError".to_owned(),
    )?;
    Ok(())
}

#[test]
fn heterogeneous_variants_keep_only_the_active_payload() -> Result<()> {
    boundary!(
        r#"
        package test:variant-values;
        world boundary {
            record point { x: f64, label: string }
            variant value { empty, number(f64), text(string), point(point), bytes(list<u8>) }
            record packet { values: list<value>, optional: option<value> }
            variant narrow { small(f32), word(u32) }
            variant outer { narrow(narrow), wide(f64), empty }
            export joined: func(value: outer) -> outer;
            export echo: func(values: list<value>) -> list<value>;
            export inspect: func(value: packet) -> packet;
            export make: func(which: u32) -> value;
            export describe: func(value: value) -> result<string>;
        }
        "#
    );
    let mut component = Instance::compile(
        WIT,
        r#"
        import type { Outer, Packet, Value, WitInput } from './types/world';
        export function joined(value: Outer): Outer { return value; }
        export function echo(values: Value[]): Value[] { return values; }
        export function inspect(value: Packet): Packet { return value; }
        export function make(which: number): WitInput<Value> {
            if (which === 0) return { tag: 'empty' };
            if (which === 1) return { tag: 'number', val: 42.5 };
            if (which === 2) return { tag: 'text', val: 'héllo' };
            if (which === 3) return { tag: 'point', val: { x: 7, label: 'seven' } };
            return { tag: 'bytes', val: 'byte text' };
        }
        export function describe(value: Value): string {
            switch (value.tag) {
                case 'empty': return 'none';
                case 'number': return String(value.val + 1);
                case 'text': return value.val + '!';
                case 'point': return value.val.label + ':' + value.val.x;
                default: return new TextDecoder().decode(value.val);
            }
        }
        "#,
    )?;
    let values = vec![
        Value::Empty,
        Value::Number(42.5),
        Value::Text("héllo".into()),
        Value::Point(Point {
            x: 7.0,
            label: "seven".into(),
        }),
        Value::Bytes(b"byte text".to_vec()),
    ];
    for (index, (value, expected)) in values
        .iter()
        .zip(["none", "43.5", "héllo!", "seven:7", "byte text"])
        .enumerate()
    {
        component.check("make", (index as u32,), value)?;
        component.check("describe", (value,), &Ok::<_, ()>(expected.to_owned()))?;
    }
    let packet = Packet {
        optional: Some(values[2].clone()),
        values,
    };
    component.check("inspect", (&packet,), &packet)?;
    component.check("echo", (&packet.values,), &packet.values)?;
    for value in [
        Outer::Narrow(Narrow::Small(-0.0)),
        Outer::Narrow(Narrow::Small(-13.25)),
        Outer::Narrow(Narrow::Word(0x8000_0001)),
        Outer::Wide(-0.0),
        Outer::Wide(-29.5),
        Outer::Empty,
    ] {
        let actual: Outer = component.call("joined", (&value,))?;
        assert_eq!(actual, value);
        match (actual, value) {
            (Outer::Narrow(Narrow::Small(actual)), Outer::Narrow(Narrow::Small(expected))) => {
                assert_eq!(actual.to_bits(), expected.to_bits());
            }
            (Outer::Wide(actual), Outer::Wide(expected)) => {
                assert_eq!(actual.to_bits(), expected.to_bits());
            }
            _ => {}
        }
    }
    Ok(())
}

#[test]
fn numeric_bigint_conversion_preserves_transport_bits_and_checks_domain() -> Result<()> {
    let mut component = Instance::compile(
        "package test:bigint-conversion; world boundary { export signed: func(value: f64) -> result<s64, string>; export unsigned: func(value: f64) -> result<u64, string>; }",
        r#"
        export function signed(value: number): bigint {
            try { return BigInt(value); }
            catch (error) { if (error instanceof RangeError) throw 'range'; throw 'unexpected'; }
        }
        export function unsigned(value: number): bigint { return signed(value); }
        "#,
    )?;
    for value in [0i64, -1, 9007199254740992, i64::MIN] {
        component.check("signed", (value as f64,), &Ok::<_, String>(value))?;
    }
    let wide = u64::MAX - 2047;
    component.check("unsigned", (wide as f64,), &Ok::<_, String>(wide))?;
    let range_error = Err::<i64, _>("range".to_owned());
    for value in [0.5, f64::NAN, f64::INFINITY, 18446744073709551616.0] {
        component.check("signed", (value,), &range_error)?;
    }
    Ok(())
}

#[test]
fn bigint_arithmetic_is_exact_beyond_transport_widths() -> Result<()> {
    let wit = r#"package test:bigint-arithmetic;
world boundary {
    export describe: func(a: s64, b: u64) -> string;
    export powers: func(n: u32, seed: u64) -> string;
    export unsigned: func(a: u64, b: u64) -> u64;
    export signed: func(a: s64, b: s64) -> s64;
    export rounding: func(offset: u64) -> f64;
    export probe: func(input: string) -> string;
    export truncation: func(value: u64) -> string;
    export literals: func() -> string;
}"#;
    let source = r#"
import type { u64 } from 'perry:types';
function sum(a: bigint, b: bigint): bigint { return a + b; }
function difference(a: bigint, b: bigint): bigint { return a - b; }
export function describe(a: bigint, b: bigint): string {
    const total = sum(a, b);
    const delta = difference(a, b);
    if (difference(total, b) !== a || sum(delta, b) !== a) throw 'inverse';
    if ((delta < total) !== (b > 0n)) throw 'order';
    return String(total) + ':' + String(delta) + ':' + String(-total) + ':' + String(a) + ':' + String(b);
}
export function powers(n: number, seed: bigint): string {
    let value = seed;
    for (let i = 0; i < n; i++) value = sum(value, value);
    const below = difference(value, 1n);
    if (sum(below, 1n) !== value || difference(value, value) !== 0n) throw 'carry';
    return String(value) + ':' + String(below) + ':' + String(Number(value));
}
export function unsigned(a: bigint, b: bigint): bigint { return difference(sum(a, b), b); }
export function signed(a: bigint, b: bigint): bigint { return sum(difference(a, b), b); }
export function rounding(offset: bigint): number {
    const wide = 18446744073709551616n;
    return Number(sum(wide, offset));
}
export function probe(input: string): string {
    const a: bigint = BigInt(input.length);
    const b: bigint = 1n;
    return String(a + b) + ':' + String(a - b);
}
export function truncation(value: bigint): string {
    const total = value + 1n;
    const wrapped = total as u64;
    return String(total) + ':' + String(wrapped);
}
export function literals(): string {
    const wide = 340_282_366_920_938_463_463_374_607_431_768_211_456n;
    if (wide !== 0x100000000000000000000000000000000n) throw 'hex';
    if (18446744073709551616n !== 0o2000000000000000000000n) throw 'octal';
    if (18446744073709551616n !== 0b10000000000000000000000000000000000000000000000000000000000000000n) throw 'binary';
    if (wide - 1n !== 340282366920938463463374607431768211455n) throw 'carry';
    return String(wide) + ':' + String(-wide);
}
"#;
    let mut instance = Instance::compile(wit, source)?;
    instance.check(
        "literals",
        (),
        &"340282366920938463463374607431768211456:-340282366920938463463374607431768211456"
            .to_owned(),
    )?;
    for a in [i64::MIN, -9007199254740993, -1, 0, 1, i64::MAX] {
        for b in [0, 1, 9007199254740993, u64::MAX] {
            let (left, right) = (i128::from(a), i128::from(b));
            instance.check(
                "describe",
                (a, b),
                &format!(
                    "{}:{}:{}:{a}:{b}",
                    left + right,
                    left - right,
                    -(left + right)
                ),
            )?;
        }
    }
    for value in [0, 1, 9007199254740993, u64::MAX] {
        instance.check("unsigned", (value, u64::MAX), &value)?;
    }
    for value in [i64::MIN, -1, 0, i64::MAX] {
        instance.check("signed", (value, i64::MAX), &value)?;
    }
    for (power, expected) in [
        (
            64u32,
            "18446744073709551616:18446744073709551615:18446744073709552000",
        ),
        (
            128,
            "340282366920938463463374607431768211456:340282366920938463463374607431768211455:3.402823669209385e+38",
        ),
    ] {
        instance.check("powers", (power, 1u64), &expected.to_owned())?;
    }
    // 2^1536 exceeds the bounded number-formatting utility's capacity and overflows Number.
    let huge: String = instance.call("powers", (1536u32, 1u64))?;
    let (power, below) = huge
        .strip_suffix(":Infinity")
        .and_then(|s| s.split_once(':'))
        .ok_or_else(|| anyhow::anyhow!("invalid large bigint result"))?;
    assert_eq!(power.len(), 463);
    assert_eq!(&power[..power.len() - 1], &below[..below.len() - 1]);
    assert!(power.ends_with('6') && below.ends_with('5'));
    for offset in [0u64, 2048, 2049, 6144] {
        instance.check(
            "rounding",
            (offset,),
            &((u128::from(u64::MAX) + 1 + u128::from(offset)) as f64),
        )?;
    }
    for (input, expected) in [("", "1:-1"), ("abc", "4:2")] {
        instance.check("probe", (input,), &expected.to_owned())?;
    }
    instance.check(
        "truncation",
        (u64::MAX,),
        &"18446744073709551616:0".to_owned(),
    )?;
    Ok(())
}

#[test]
fn bigint_boundary_rejects_results_outside_transport_range() -> Result<()> {
    let mut instance = Instance::compile(
        "package test:bigint-overflow; world boundary { export unsigned: func(value: u64) -> u64; export signed: func(value: s64) -> s64; export constant: func() -> u64; export reinterpret: func(value: s64) -> u64; }",
        "export function unsigned(value: bigint): bigint { return value + 1n; } export function signed(value: bigint): bigint { return value - 1n; } export function constant(): bigint { return 18446744073709551616n; } export function reinterpret(value: bigint): bigint { return value; }",
    )?;
    assert!(instance.call::<_, u64>("unsigned", (u64::MAX,)).is_err());
    assert!(instance.call::<_, i64>("signed", (i64::MIN,)).is_err());
    assert!(instance.call::<_, u64>("constant", ()).is_err());
    // A bigint kept in an `i64` register is checked like one on the heap.
    assert!(instance.call::<_, u64>("reinterpret", (-1i64,)).is_err());
    Ok(())
}

#[test]
fn canonical_byte_payloads_reuse_input_storage_in_both_directions() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("reuse.wit", "package test:reuse; world reuse {export text:func(value:string)->list<u8>;export bytes:func(value:list<u8>)->list<u8>;}")?;
    let world = resolve.select_world(&[package], Some("reuse"))?;
    let compiled = compile_for_world(
        "export function text(value:string):string {return value;} export function bytes(value:Uint8Array):Uint8Array {if(value.length>0)value[0]=65;return value;}",
        "reuse.ts",
        &resolve,
        world,
        false,
    )?;
    let engine = Engine::default();
    let module = wasmtime::Module::new(&engine, compiled.core)?;
    let mut store = Store::new(&engine, ());
    let instance = wasmtime::Instance::new(&mut store, &module, &[])?;
    let memory = instance.get_memory(&mut store, "memory").unwrap();
    let allocate =
        instance.get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, "cabi_realloc")?;
    for name in ["text", "bytes"] {
        let run = instance.get_typed_func::<(i32, i32), i32>(&mut store, name)?;
        let post = instance.get_typed_func::<i32, ()>(&mut store, &format!("cabi_post_{name}"))?;
        for text in ["ASCII", "Grüße 漢字😀"] {
            let input = allocate.call(&mut store, (0, 0, 1, text.len() as i32))?;
            memory.write(&mut store, input as usize, text.as_bytes())?;
            let output = run.call(&mut store, (input, text.len() as i32))? as usize;
            let data = memory.data(&store);
            let returned =
                u32::from_le_bytes(data[output..output + 4].try_into().unwrap()) as usize;
            let len = u32::from_le_bytes(data[output + 4..output + 8].try_into().unwrap()) as usize;
            assert_eq!(
                returned, input as usize,
                "{name} made an extra guest payload copy"
            );
            assert_eq!(len, text.len());
            let mut expected = text.as_bytes().to_vec();
            if name == "bytes" {
                expected[0] = b'A';
            }
            assert_eq!(&data[returned..returned + len], expected);
            post.call(&mut store, output as i32)?;
        }
    }
    Ok(())
}

#[test]
fn generic_pages_and_results_use_concrete_wit_records() -> Result<()> {
    boundary!(
        r#"package test:generics;
world boundary {
  record page { data: list<f64>, next: option<string> }
  export echo: func(value: page) -> result<page, string>;
  export parsed: func() -> result<page>;
  export stringify: func(value: page) -> result<string>;
}"#
    );
    let source = r#"
interface Page<T> {data:T[];next:string|null}
type Result<T>={ok:true;value:T}|{ok:false;error:string};
export function echo(value:Page<number>):Result<Page<number>> {return {ok:true,value};}
export function parsed():Page<number> {return JSON.parse('{"extra":true,"data":[1,2],"next":null}');}
export function stringify(value:Page<number>):string {return JSON.stringify(value);}
"#;
    let mut instance = Instance::compile(WIT, source)?;
    let page = Page {
        data: vec![1.0, 2.0],
        next: None,
    };
    assert_eq!(
        instance.call::<_, Result<Page, ()>>("parsed", ())?.as_ref(),
        Ok(&page)
    );
    assert_eq!(
        instance
            .call::<_, Result<Page, String>>("echo", (&page,))?
            .as_ref(),
        Ok(&page)
    );
    instance.check(
        "stringify",
        (&page,),
        &Ok::<_, ()>("{\"data\":[1,2],\"next\":null}".to_owned()),
    )?;
    Ok(())
}

#[test]
fn static_modules_resolve_aliases_namespaces_and_reexports() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("left.ts"),
        "export function value(x: number): number { const values = new Map<string, number>(); values.set('x', x); return values.get('x') + 1; }",
    )?;
    std::fs::write(
        directory.path().join("right.ts"),
        "export function value(x: number): number { const values = new Set<number>([x]); return values.size * x * 2; }",
    )?;
    let entry = directory.path().join("main.ts");
    let entry = entry.to_string_lossy();
    let source = "import { value as left } from './left'; import * as right from './right.js'; \
                  export function run(x: number): number { return right.value(left(x)); } \
                  export { value as reexported } from './left.js';";
    let mut program =
        crate::core_program::Program::from_wasm(perry_wit::compile_core(source, &entry)?);
    for marker in ["perry_constructor:", "$$perry", "arrayRecord"] {
        assert!(
            !program
                .wasm
                .windows(marker.len())
                .any(|bytes| bytes == marker.as_bytes())
        );
    }
    assert_eq!(program.number("run", 5.0), 12.0);
    assert_eq!(program.number("reexported", 5.0), 6.0);
    Ok(())
}
