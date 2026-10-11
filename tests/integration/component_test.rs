//! Component exports end to end: TypeScript compiled against a WIT world, instantiated with
//! wasmtime's component model, called with WIT values (canonical ABI lift and lower).
use anyhow::Result;
use perry_wit::compile_for_world;
use wasmtime::{
    Engine, ResourceLimiter, Store,
    component::{Component, Linker, Val},
};

/// Records the largest linear memory the guest asks for.
#[derive(Default)]
struct Peak(usize);

#[test]
fn unbound_exports_do_not_require_async_hosts() -> Result<()> {
    let mut component = Instance::compile(
        "package test:reachability; world boundary {export run:func()->u32;}",
        r#"
        function initial(n:number):number { if(n <= 0) return 7; return initial(n - 1) + 1; }
        let count = initial(3);
        export function run():number { return ++count; }
        export async function unused():Promise<void> { console.log('not in this world'); }
        "#,
    )?;
    assert_eq!(component.call("run", &[])?, Val::U32(11));
    assert_eq!(component.call("run", &[])?, Val::U32(12));
    Ok(())
}

#[test]
fn indirect_parameters_lift_nested_values_and_release_storage() -> Result<()> {
    let mut component = Instance::compile(
        "package test:indirect; world boundary {record input {a:string,b:string,c:string,d:string,e:string,f:string,g:string,h:string,i:option<string>,n:u64} export run:func(value:input)->string;}",
        "import type { Input } from './types/world'; export function run(value:Input):string {return value.a+value.b+value.c+value.d+value.e+value.f+value.g+value.h+value.i+String(value.n);}",
    )?;
    let value = record([
        ("a", Val::String("a".into())),
        ("b", Val::String("b".into())),
        ("c", Val::String("c".into())),
        ("d", Val::String("d".into())),
        ("e", Val::String("e".into())),
        ("f", Val::String("f".into())),
        ("g", Val::String("g".into())),
        ("h", Val::String("h".into())),
        ("i", Val::Option(Some(Box::new(Val::String("🙂".into()))))),
        ("n", Val::U64(18446744073709551615)),
    ]);
    for _ in 0..1000 {
        assert_eq!(
            component.call("run", std::slice::from_ref(&value))?,
            Val::String("abcdefgh🙂18446744073709551615".into())
        );
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

const WIT: &str = r#"
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
"#;

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
    let wit = r#"package test:integers;
world boundary {
  record packet { signed: s64, unsigned: u64, optional: option<u64> }
  export echo: func(value: packet) -> result<packet, string>;
  export extremes: func() -> list<s64>;
  export describe-wide: func(value: u64) -> string;
  export throw-wide: func(value: s64) -> result<s64, s64>;
}"#;
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
    let mut instance = Instance::compile(wit, source)?;
    for optional in [None, Some(Box::new(Val::U64(9_007_199_254_740_993)))] {
        let packet = Val::Record(vec![
            ("signed".into(), Val::S64(i64::MIN)),
            ("unsigned".into(), Val::U64(u64::MAX)),
            ("optional".into(), Val::Option(optional)),
        ]);
        assert_eq!(
            instance.call("echo", std::slice::from_ref(&packet))?,
            Val::Result(Ok(Some(Box::new(packet))))
        );
    }
    assert_eq!(
        instance.call("extremes", &[])?,
        Val::List(vec![
            Val::S64(i64::MIN),
            Val::S64(-1),
            Val::S64(0),
            Val::S64(i64::MAX)
        ])
    );
    assert_eq!(
        instance.call("describe-wide", &[Val::U64(u64::MAX)])?,
        Val::String("bigint:18446744073709551615".into())
    );
    assert_eq!(
        instance.call("throw-wide", &[Val::S64(9_007_199_254_740_993)])?,
        Val::Result(Err(Some(Box::new(Val::S64(-9_007_199_254_740_993)))))
    );
    Ok(())
}

impl Instance {
    fn new() -> Result<Self> {
        Self::compile(WIT, SOURCE)
    }

    fn compile(wit: &str, source: &str) -> Result<Self> {
        let mut resolve = wit_parser::Resolve::default();
        let package = resolve.push_str("boundary.wit", wit)?;
        let world = resolve.select_world(&[package], Some("boundary"))?;
        let compiled = compile_for_world(source, "boundary.ts", &resolve, world, true)?;
        let engine = Engine::default();
        let component = Component::new(&engine, compiled.component.unwrap().stripped)?;
        let mut store = Store::new(&engine, Peak::default());
        store.limiter(|peak| peak);
        let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
        Ok(Instance { store, instance })
    }

    fn call(&mut self, name: &str, args: &[Val]) -> Result<Val> {
        let function = self
            .instance
            .get_func(&mut self.store, name)
            .unwrap_or_else(|| panic!("missing export {name}"));
        let mut results = [Val::Bool(false)];
        function.call(&mut self.store, args, &mut results)?;
        let [result] = results;
        Ok(result)
    }
}

#[test]
fn canonical_inputs_survive_their_call_and_collectible_churn() -> Result<()> {
    let mut component = Instance::compile(
        r#"
package test:owners;
world boundary {
    export remember: func(text: string, bytes: list<u8>) -> u32;
    export recall: func(n: u32) -> string;
    export bytes: func() -> list<u8>;
}
"#,
        r#"
let saved = "";
let data = new Uint8Array(0);
export function remember(text: string, bytes: Uint8Array): number {
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
    assert_eq!(
        component.call(
            "remember",
            &[
                Val::String("!hé🙂".into()),
                Val::List(vec![Val::U8(1), Val::U8(2), Val::U8(3)])
            ]
        )?,
        Val::U32(2)
    );
    assert_eq!(
        component.call("recall", &[Val::U32(200_000)])?,
        Val::String("hé🙂:255:temporary:199999".into())
    );
    assert_eq!(
        component.call("bytes", &[])?,
        Val::List(vec![Val::U8(255), Val::U8(3)])
    );
    Ok(())
}

#[test]
fn utf8_strings_lower_to_nested_byte_lists() -> Result<()> {
    let mut component = Instance::compile(
        r#"
package test:bytes;
world boundary {
    record payload { text: list<u8>, parts: list<list<u8>> }
    export encode: func(text: string) -> payload;
}

"#,
        r#"
interface Payload { text: string; parts: string[] }
export function encode(text: string): Payload { return { text, parts: [text, "é🙂"] }; }
"#,
    )?;
    let bytes = |s: &str| Val::List(s.bytes().map(Val::U8).collect());
    assert_eq!(
        component.call("encode", &[Val::String("a✓".into())])?,
        record([
            ("text", bytes("a✓")),
            ("parts", Val::List(vec![bytes("a✓"), bytes("é🙂")]))
        ])
    );
    Ok(())
}

/// A WIT record value from `(field, value)` pairs.
fn record<const N: usize>(fields: [(&str, Val); N]) -> Val {
    Val::Record(fields.map(|(name, value)| (name.to_owned(), value)).into())
}

#[test]
fn records_strings_lists_and_scalars_cross_the_boundary() -> Result<()> {
    let mut component = Instance::new()?;
    assert_eq!(
        component.call(
            "scale",
            &[
                record([("x", Val::Float64(1.5)), ("y-coord", Val::Float64(-2.0))]),
                Val::Float64(4.0)
            ]
        )?,
        record([("x", Val::Float64(6.0)), ("y-coord", Val::Float64(-8.0))])
    );
    assert_eq!(
        component.call(
            "describe",
            &[
                Val::String("sum ✓".into()),
                Val::List(vec![
                    Val::Float64(1.0),
                    Val::Float64(-2.5),
                    Val::Float64(4.0)
                ])
            ]
        )?,
        record([
            ("count", Val::U32(3)),
            ("total", Val::Float64(2.5)),
            ("label", Val::String("sum ✓!".into())),
            (
                "positive",
                Val::List(vec![Val::Bool(true), Val::Bool(false), Val::Bool(true)])
            ),
        ])
    );
    // `.length` counts Unicode scalars: both "🙂" and "é" have length one.
    assert_eq!(
        component.call(
            "count",
            &[Val::List(vec![
                Val::String("ab".into()),
                Val::String("🙂".into()),
                Val::String("é".into()),
                Val::String(String::new()),
            ])]
        )?,
        Val::U32(4)
    );
    assert_eq!(
        component.call("greet", &[Val::String("wörld".into())])?,
        Val::String("héllo wörld".into())
    );
    assert_eq!(
        component.call("negate", &[Val::Bool(true)])?,
        Val::Bool(false)
    );
    assert_eq!(
        component.call("wide", &[Val::S64(-5), Val::U8(200), Val::Float32(0.5)])?,
        Val::Float64(195.5)
    );
    assert_eq!(
        component.call(
            "nested",
            &[Val::List(vec![
                Val::List(vec![Val::Float64(1.0), Val::Float64(2.0)]),
                Val::List(vec![]),
                Val::List(vec![Val::Float64(-3.0)]),
            ])]
        )?,
        Val::List(vec![
            Val::Float64(3.0),
            Val::Float64(0.0),
            Val::Float64(-3.0)
        ])
    );
    Ok(())
}

/// Every call is one task: repeated calls on one instance release their memory.
#[test]
fn repeated_calls_do_not_grow_memory() -> Result<()> {
    let mut component = Instance::new()?;
    let numbers = Val::List((0..20_000).map(|i| Val::Float64(f64::from(i))).collect());
    let warm = [Val::String("warm".into()), numbers.clone()];
    for _ in 0..3 {
        component.call("describe", &warm)?;
    }
    let before = component.store.data().0;
    let again = [Val::String("again".into()), numbers];
    for _ in 0..50 {
        component.call("describe", &again)?;
    }
    assert_eq!(component.store.data().0, before, "memory grew across tasks");
    Ok(())
}

#[test]
fn tagged_results_options_and_enums_cross_nested_boundaries() -> Result<()> {
    let mut component = Instance::compile(
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
"#,
        r#"
import type { Answer, Nothing, Packet } from './types/world';
export function inspect(value: Packet): Packet { return value; }
export function make(ok: boolean): Answer { if (ok) return { ok: true, value: 42 }; return { ok: false, error: "failed" }; }
export function finish(ok: boolean): Nothing { if (ok) return { ok: true }; return { ok: false }; }
"#,
    )?;
    let ok = Val::Result(Ok(Some(Box::new(Val::Float64(42.0)))));
    let err = Val::Result(Err(Some(Box::new(Val::String("failed".into())))));
    for note in [
        Val::Option(None),
        Val::Option(Some(Box::new(Val::String("note".into())))),
    ] {
        let packet = record([
            ("answer", err.clone()),
            ("color", Val::Enum("blue".into())),
            ("note", note),
            ("history", Val::List(vec![ok.clone(), err.clone()])),
        ]);
        assert_eq!(
            component.call("inspect", std::slice::from_ref(&packet))?,
            packet
        );
    }
    assert_eq!(component.call("make", &[Val::Bool(true)])?, ok);
    assert_eq!(component.call("make", &[Val::Bool(false)])?, err);
    assert_eq!(
        component.call("finish", &[Val::Bool(true)])?,
        Val::Result(Ok(None))
    );
    assert_eq!(
        component.call("finish", &[Val::Bool(false)])?,
        Val::Result(Err(None))
    );
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
    assert_eq!(
        component.call("run", &[Val::Bool(false)])?,
        Val::Result(Ok(Some(Box::new(Val::String("done".into())))))
    );
    assert_eq!(
        component.call("run", &[Val::Bool(true)])?,
        Val::Result(Err(Some(Box::new(Val::String("failed".into())))))
    );
    assert_eq!(
        component.call("nothing", &[Val::Bool(false)])?,
        Val::Result(Ok(None))
    );
    assert_eq!(
        component.call("nothing", &[Val::Bool(true)])?,
        Val::Result(Err(None))
    );
    Ok(())
}

#[test]
fn recursive_wit_input_accepts_strings_and_mutable_bytes() -> Result<()> {
    let mut component = Instance::compile(
        r#"
package test:input;
world boundary {
    record payload { text: list<u8>, parts: list<list<u8>> }
    export encode: func(text: string, bytes: list<u8>, text-first: bool) -> payload;
}
"#,
        r#"
import type { Payload, PayloadInput, WitInput } from './types/world';
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
    let bytes = |s: &str| Val::List(s.bytes().map(Val::U8).collect());
    for text_first in [true, false] {
        assert_eq!(
            component.call(
                "encode",
                &[
                    Val::String("é🙂".into()),
                    bytes("xyz"),
                    Val::Bool(text_first)
                ]
            )?,
            record([
                ("text", bytes(if text_first { "é🙂" } else { "xyz" })),
                ("parts", Val::List(vec![bytes("é🙂"), bytes("xyz")]))
            ])
        );
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
    let got = component.call("decode", &[Val::String(good.into())])?;
    assert_eq!(got, Val::Result(Ok(Some(Box::new(Val::String(r#"{"rows":[{"active":true,"count":99,"label":"é"}],"title":"ok"}:{"rows":[{"active":true,"count":1,"label":"é"}],"title":"ok"}"#.into()))))));
    for invalid in [
        "null",
        "[]",
        "{}",
        r#"{"rows":{},"title":"x"}"#,
        r#"{"rows":[{"count":null,"label":"x","active":true}],"title":"x"}"#,
        r#"{"rows":[{"count":1,"label":false,"active":true}],"title":"x"}"#,
    ] {
        assert_eq!(
            component.call("decode", &[Val::String(invalid.into())])?,
            Val::Result(Err(Some(Box::new(Val::String("invalid schema".into()))))),
            "{invalid}"
        );
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
                    first.value += 100;
                    total = sum(first) + sum(second);
                }
                return String(total);
            } catch (error) { return error.name; }
        }
    "#,
    )?;
    let leaf = serde_json::json!({"value": 2, "children": [], "next": null});
    let tree =
        serde_json::json!({"value": 3, "children": [leaf.clone(), leaf.clone()], "next": leaf});
    assert_eq!(
        component.call("decode", &[Val::String(tree.to_string()), Val::U32(1000)])?,
        Val::String("118".into())
    );
    let invalid = serde_json::json!({"value": 3, "children": [{"value": "wrong", "children": [], "next": null}], "next": null});
    assert_eq!(
        component.call("decode", &[Val::String(invalid.to_string()), Val::U32(1)])?,
        Val::String("TypeError".into())
    );
    Ok(())
}

#[test]
fn heterogeneous_variants_keep_only_the_active_payload() -> Result<()> {
    let mut component = Instance::compile(
        r#"
        package test:variant-values;
        world boundary {
            record point { x: f64, label: string }
            variant value { empty, number(f64), text(string), point(point), bytes(list<u8>) }
            record packet { values: list<value>, optional: option<value> }
            export echo: func(values: list<value>) -> list<value>;
            export inspect: func(value: packet) -> packet;
            export make: func(which: u32) -> value;
            export describe: func(value: value) -> string;
        }
        "#,
        r#"
        import type { Packet, Value, WitInput } from './types/world';
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
        Val::Variant("empty".into(), None),
        Val::Variant("number".into(), Some(Box::new(Val::Float64(42.5)))),
        Val::Variant("text".into(), Some(Box::new(Val::String("héllo".into())))),
        Val::Variant(
            "point".into(),
            Some(Box::new(record([
                ("x", Val::Float64(7.0)),
                ("label", Val::String("seven".into())),
            ]))),
        ),
        Val::Variant(
            "bytes".into(),
            Some(Box::new(Val::List(
                b"byte text".iter().copied().map(Val::U8).collect(),
            ))),
        ),
    ];
    for (index, (value, expected)) in values
        .iter()
        .zip(["none", "43.5", "héllo!", "seven:7", "byte text"])
        .enumerate()
    {
        assert_eq!(component.call("make", &[Val::U32(index as u32)])?, *value);
        assert_eq!(
            component.call("describe", std::slice::from_ref(value))?,
            Val::String(expected.into())
        );
    }
    let packet = record([
        ("values", Val::List(values.clone())),
        ("optional", Val::Option(Some(Box::new(values[2].clone())))),
    ]);
    assert_eq!(
        component.call("inspect", std::slice::from_ref(&packet))?,
        packet
    );
    assert_eq!(
        component.call("echo", &[Val::List(values.clone())])?,
        Val::List(values)
    );
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
        assert_eq!(
            component.call("signed", &[Val::Float64(value as f64)])?,
            Val::Result(Ok(Some(Box::new(Val::S64(value)))))
        );
    }
    let wide = u64::MAX - 2047;
    assert_eq!(
        component.call("unsigned", &[Val::Float64(wide as f64)])?,
        Val::Result(Ok(Some(Box::new(Val::U64(wide)))))
    );
    for value in [0.5, f64::NAN, f64::INFINITY, 18446744073709551616.0] {
        assert_eq!(
            component.call("signed", &[Val::Float64(value)])?,
            Val::Result(Err(Some(Box::new(Val::String("range".into())))))
        );
    }
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
    let wit = r#"package test:generics;
world boundary {
  record page { data: list<f64>, next: option<string> }
  export echo: func(value: page) -> result<page, string>;
  export parsed: func() -> page;
  export stringify: func(value: page) -> string;
}"#;
    let source = r#"
interface Page<T> {data:T[];next:string|null}
type Result<T>={ok:true;value:T}|{ok:false;error:string};
export function echo(value:Page<number>):Result<Page<number>> {return {ok:true,value};}
export function parsed():Page<number> {return JSON.parse('{"extra":true,"data":[1,2],"next":null}');}
export function stringify(value:Page<number>):string {return JSON.stringify(value);}
"#;
    let mut instance = Instance::compile(wit, source)?;
    let page = Val::Record(vec![
        (
            "data".into(),
            Val::List(vec![Val::Float64(1.0), Val::Float64(2.0)]),
        ),
        ("next".into(), Val::Option(None)),
    ]);
    assert_eq!(instance.call("parsed", &[])?, page);
    assert_eq!(
        instance.call("echo", std::slice::from_ref(&page))?,
        Val::Result(Ok(Some(Box::new(page.clone()))))
    );
    assert_eq!(
        instance.call("stringify", &[page])?,
        Val::String("{\"data\":[1,2],\"next\":null}".into())
    );
    Ok(())
}
