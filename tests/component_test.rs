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
export function wide(a: number, b: number, c: number): number {
  return a + b + c;
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

impl Instance {
    fn new() -> Result<Self> {
        let mut resolve = wit_parser::Resolve::default();
        let package = resolve.push_str("boundary.wit", WIT)?;
        let world = resolve.select_world(&[package], Some("boundary"))?;
        let compiled = compile_for_world(SOURCE, "boundary.ts", &resolve, world, true)?;
        let engine = Engine::default();
        let component = Component::new(&engine, compiled.stripped.unwrap())?;
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
        Ok(results[0].clone())
    }
}

fn record(fields: &[(&str, Val)]) -> Val {
    Val::Record(
        fields
            .iter()
            .map(|(n, v)| (n.to_string(), v.clone()))
            .collect(),
    )
}

#[test]
fn records_strings_lists_and_scalars_cross_the_boundary() -> Result<()> {
    let mut component = Instance::new()?;
    assert_eq!(
        component.call(
            "scale",
            &[
                record(&[("x", Val::Float64(1.5)), ("y-coord", Val::Float64(-2.0))]),
                Val::Float64(4.0)
            ]
        )?,
        record(&[("x", Val::Float64(6.0)), ("y-coord", Val::Float64(-8.0))])
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
        record(&[
            ("count", Val::U32(3)),
            ("total", Val::Float64(2.5)),
            ("label", Val::String("sum ✓!".into())),
            (
                "positive",
                Val::List(vec![Val::Bool(true), Val::Bool(false), Val::Bool(true)])
            ),
        ])
    );
    // `.length` counts UTF-16 code units: "🙂" is 2, "é" is 1.
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
        Val::U32(5)
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
    let big: Vec<Val> = (0..20_000).map(|i| Val::Float64(f64::from(i))).collect();
    for _ in 0..3 {
        component.call(
            "describe",
            &[Val::String("warm".into()), Val::List(big.clone())],
        )?;
    }
    let before = component.store.data().0;
    for _ in 0..50 {
        component.call(
            "describe",
            &[Val::String("again".into()), Val::List(big.clone())],
        )?;
    }
    assert_eq!(component.store.data().0, before, "memory grew across tasks");
    Ok(())
}

#[test]
fn synchronous_exports_drain_detached_continuations_before_lifting_results() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "sync-jobs.wit",
        "package test:jobs; world jobs { export run: func(n: u32) -> list<f64>; }",
    )?;
    let world = resolve.select_world(&[package], Some("jobs"))?;
    let source = r#"
async function ready(): Promise<number> { return 0; }
async function append(trace: number[], tag: number): Promise<void> {
  trace.push(tag);
  await ready();
  trace.push(tag+1000);
  await ready();
  trace.push(tag+2000);
}
export function run(n: number): number[] {
  const trace: number[] = [];
  for(let i=0; i<n; i++) append(trace,i);
  trace.push(-1);
  return trace;
}
"#;
    let compiled = compile_for_world(source, "sync-jobs.ts", &resolve, world, true)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, Peak::default());
    store.limiter(|peak| peak);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(u32,), (Vec<f64>,)>(&mut store, "run")?;
    let n = 1000;
    let expected: Vec<f64> = (0..n)
        .chain(std::iter::once(-1))
        .chain(1000..1000 + n)
        .chain(2000..2000 + n)
        .map(f64::from)
        .collect();
    let mut peak = 0;
    for round in 0..10 {
        let (trace,) = run.call(&mut store, (n as u32,))?;
        assert_eq!(trace, expected);
        if round == 0 {
            peak = store.data().0;
        } else {
            assert_eq!(store.data().0, peak, "detached frames leaked task memory");
        }
    }
    Ok(())
}

#[test]
fn byte_views_share_storage_copies_are_independent_and_tasks_release_both() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "bytes.wit",
        "package test:bytes; world bytes { export run: func(input: list<u8>) -> list<u8>; }",
    )?;
    let world = resolve.select_world(&[package], Some("bytes"))?;
    let source = r#"
function verify(input: Uint8Array): Uint8Array {
  const buffer = new ArrayBuffer(input.length + 6);
  const whole = new Uint8Array(buffer);
  const view = new Uint8Array(buffer, 2, input.length);
  const empty = new Uint8Array();
  const literal = Uint8Array.of(257, -1);
  if(literal.length !== 2 || literal[0] !== 1 || literal[1] !== 255) throw 5;
  if(empty.length !== 0 || whole[0] !== 0 || whole[1] !== 0) throw 1;
  if(view.buffer !== buffer || view.byteOffset !== 2 || view.byteLength !== input.length) throw 2;
  view.set(input);
  const copy = new Uint8Array(view);
  const from = Uint8Array.from(view);
  const sliced = view.slice();
  const sub = view.subarray(-3);
  if(sub.buffer !== buffer || copy.buffer === buffer || from.buffer === buffer) throw 3;
  view[0] = 999;
  if(input.length > 0 && (copy[0] !== input[0] || sliced[0] !== input[0] || from[0] !== input[0])) throw 4;
  view.set(copy);
  return whole.subarray(2, input.length + 2);
}
export function run(input: Uint8Array): Uint8Array { return verify(input); }
"#;
    let compiled = compile_for_world(source, "bytes.ts", &resolve, world, true)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, Peak::default());
    store.limiter(|peak| peak);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(&[u8],), (Vec<u8>,)>(&mut store, "run")?;
    assert_eq!(run.call(&mut store, (&[],))?.0, Vec::<u8>::new());
    let input: Vec<u8> = (0..50_000).map(|i| i as u8).collect();
    let mut peak = 0;
    for round in 0..20 {
        assert_eq!(run.call(&mut store, (&input,))?.0, input);
        if round == 2 {
            peak = store.data().0;
        }
        if round > 2 {
            assert_eq!(store.data().0, peak, "byte storage leaked across tasks");
        }
    }
    Ok(())
}

#[test]
fn parsed_json_keeps_variants_aliases_and_checked_scalar_boundaries() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "json.wit",
        "package test:json; world json { export run: func(input: string) -> string; }",
    )?;
    let world = resolve.select_world(&[package], Some("json"))?;
    let source = r#"
type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };
function scalar(value: JsonValue): number {
  if(typeof value !== 'number') throw new TypeError('expected number');
  return value;
}

function array(value: JsonValue): JsonValue[] {
  if(!Array.isArray(value)) throw new TypeError('expected array');
  return value;
}
export function run(input: string): string {
  const parsed = JSON.parse(input);
  if(typeof parsed === 'number') return JSON.stringify(scalar(parsed) + 1);
  if(Array.isArray(parsed)) {
    const items = array(parsed);
    const alias = parsed[0];
    alias.value = 7;
    if(alias !== parsed[0]) throw 1;
    items.push(9);
    return JSON.stringify(parsed);
  }
  return JSON.stringify(parsed);
}
"#;
    let compiled = compile_for_world(source, "json.ts", &resolve, world, true)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, Peak::default());
    store.limiter(|peak| peak);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    for (input, expected) in [
        ("3", "4"),
        ("null", "null"),
        ("true", "true"),
        ("\"é🙂\"", "\"é🙂\""),
        (
            r#"{"b":1,"2":2,"a":3,"b":4,"1":5}"#,
            r#"{"1":5,"2":2,"b":4,"a":3}"#,
        ),
        (r#"[{"value":1}]"#, r#"[{"value":7},9]"#),
    ] {
        assert_eq!(run.call(&mut store, (input,))?.0, expected);
    }
    Ok(())
}

#[test]
fn native_json_storage_preserves_order_aliases_and_types() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "native.wit",
        r#"
package test:native;
world native {
  record pair { second: f64, first: f64 }
  export run: func(input: pair) -> string;
}"#,
    )?;
    let world = resolve.select_world(&[package], Some("native"))?;
    let source = r#"
type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };
interface Pair { first: number; second: number }
interface Cycle { value: number; children: Cycle[] }
function flag(value: boolean): boolean { return value; }
export function run(input: Pair): string {
  const pair: Pair = {second:2,first:1};
  const other: Pair = {first:11,second:12};
  const values = [1,2];
  const flags = [false,false];
  const native = {title:'é🙂',pair:pair,values:values,flags:flags};
  const json: JsonValue = native;
  const captured = json.pair;
  json.pair.second = 9;
  pair.first = 7;
  if(captured !== pair || json.pair.first !== 7) throw 1;
  json.pair = other;
  if(json.pair !== other || captured !== pair || native.pair !== other) throw 2;
  values.push(3);
  json.values[0] = 5;
  json.values[3] = 6;
  values.push(8);
  json.flags[1] = true;
  if(!Array.isArray(json.values) || json.values.length !== 5 || !flag(flags[1])) throw 3;
  let checked = 0;
  try { json.pair.first = 'wrong'; } catch(e) { if(e instanceof TypeError) checked = checked + 1; }
  try { json.values[0] = false; } catch(e) { if(e instanceof TypeError) checked = checked + 1; }
  try { json.flags[0] = 1; } catch(e) { if(e instanceof TypeError) checked = checked + 1; }
  if(checked !== 3 || other.first !== 11 || values[0] !== 5 || flag(flags[0])) throw 4;
  const cycle: Cycle = {value:1,children:[]};
  const children: Cycle[] = cycle.children;
  children.push(cycle);
  try { JSON.stringify(cycle); } catch(e) { if(e instanceof TypeError) checked = checked + 1; }
  if(checked !== 4) throw 5;
  return JSON.stringify([input,pair,other,native]);
}

"#;
    let compiled = compile_for_world(source, "native.ts", &resolve, world, true)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, Peak::default());
    store.limiter(|peak| peak);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_func(&mut store, "run").unwrap();
    let input = Val::Record(vec![
        ("second".into(), Val::Float64(20.0)),
        ("first".into(), Val::Float64(10.0)),
    ]);
    let mut output = [Val::String(String::new())];
    for _ in 0..20 {
        run.call(&mut store, std::slice::from_ref(&input), &mut output)?;
        assert_eq!(output[0], Val::String(r#"[{"second":20,"first":10},{"second":9,"first":7},{"first":11,"second":12},{"title":"é🙂","pair":{"first":11,"second":12},"values":[5,2,3,6,8],"flags":[false,true]}]"#.into()));
    }
    Ok(())
}

#[test]
fn parsed_json_typed_views_check_mutations_and_cross_wit() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "views.wit",
        r#"
package test:views;
world views {
  record item { name: string, values: list<f64> }
  export run: func(input: string) -> item;
  export invalid: func(input: string) -> bool;
  export invalid-result: func() -> item;
}"#,
    )?;
    let world = resolve.select_world(&[package], Some("views"))?;
    let source = r#"
type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };
interface Item { name: string; values: number[] }
interface Tree { value: number; children: Tree[] }
function item(value: JsonValue): Item { return value as unknown as Item; }
function numbers(value: JsonValue): number[] { return value as unknown as number[]; }
function jsonArray(value: JsonValue): JsonValue[] { return value as unknown as JsonValue[]; }
function tree(value: JsonValue): Tree { return value as unknown as Tree; }
export function invalidResult(): Item {
  const parsed = JSON.parse('{"name":"x","values":[1]}');
  const typed = item(parsed);
  parsed.name = 0;
  return typed;
}
export function invalid(input: string): boolean {
  try { item(JSON.parse(input)); } catch(e) { return e instanceof TypeError; }
  return false;
}
export function run(input: string): Item {
  const parsed = JSON.parse(input);
  const typed = item(parsed);
  const again = item(parsed);
  if(typed !== again || typed !== parsed || typed.values !== again.values) throw 1;
  const old = typed.values;
  old.push(3);
  typed.values[0] = 7;
  if(parsed.values[2] !== 3 || parsed.values[0] !== 7) throw 2;
  parsed.values[1] = 9;
  if(typed.values[1] !== 9) throw 3;
  if(typed.values[99] !== undefined || typed.values[-1] !== undefined) throw 4;
  const empty = numbers(JSON.parse('[]'));
  if(empty[0] !== undefined) throw 5;
  empty.push(1);
  empty[1] = 2;
  if(empty.length !== 2) throw 6;
  let checked = 0;
  parsed.values[0] = 'wrong';
  try { old[0]; } catch(e) { if(e instanceof TypeError) checked = checked + 1; }
  try { new Uint8Array(old); } catch(e) { if(e instanceof TypeError) checked = checked + 1; }
  parsed.values[0] = 7;
  const bytes = new Uint8Array(old);
  if(bytes[0] !== 7 || bytes[1] !== 9 || bytes[2] !== 3) throw 7;
  parsed.name = false;
  try { typed.name; } catch(e) { if(e instanceof TypeError) checked = checked + 1; }
  typed.name = 'restored';
  try { old[10] = 1; } catch(e) { if(e instanceof RangeError) checked = checked + 1; }
  const native = {name:'native',values:[4,5]};
  const container = {item:native};
  const holder: JsonValue = container;
  holder.item = parsed;
  if(container.item !== typed) throw 8;
  const shared = jsonArray(holder.item.values);
  shared.push(11);
  if(old.length !== 4) throw 9;
  const nativeJson: JsonValue = native;
  const nativeView = jsonArray(nativeJson.values);
  nativeView[0] = 8;
  try { nativeView.push('wrong'); } catch(e) { if(e instanceof TypeError) checked = checked + 1; }
  if(native.values[0] !== 8 || native.values.length !== 2) throw 10;
  const recursive = JSON.parse('{"value":1,"children":[]}');
  recursive.children[0] = recursive;
  const cycle = tree(recursive);
  if(cycle.children[0] !== cycle) throw 11;
  const values: JsonValue = [21,22];
  typed.values = numbers(values);
  if(old.length !== 4 || typed.values.length !== 2 || parsed.values !== values) throw 12;
  if(checked !== 5) throw 13;
  if(JSON.stringify(typed) !== '{"values":[21,22],"extra":true,"name":"restored"}') throw 14;
  return typed;
}
"#;
    let compiled = compile_for_world(source, "views.ts", &resolve, world, true)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, Peak::default());
    store.limiter(|peak| peak);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_func(&mut store, "run").unwrap();
    let invalid = instance.get_typed_func::<(&str,), (bool,)>(&mut store, "invalid")?;
    for input in ["null", "[]", "{}", r#"{"name":"x","values":[1,false]}"#] {
        assert!(invalid.call(&mut store, (input,))?.0);
    }
    let mut output = [Val::Bool(false)];
    let input = Val::String(r#"{"values":[1,2],"extra":true,"name":"parsed"}"#.into());
    for _ in 0..20 {
        run.call(&mut store, std::slice::from_ref(&input), &mut output)?;
        assert_eq!(
            output[0],
            Val::Record(vec![
                ("name".into(), Val::String("restored".into())),
                (
                    "values".into(),
                    Val::List(vec![Val::Float64(21.0), Val::Float64(22.0)])
                ),
            ])
        );
    }
    let invalid_result = instance.get_func(&mut store, "invalid-result").unwrap();
    let failure = invalid_result
        .call(&mut store, &[], &mut output)
        .unwrap_err();
    assert!(
        format!("{failure:#}").contains("unreachable"),
        "{failure:#}"
    );
    Ok(())
}

#[test]
fn json_dictionary_mutations_preserve_views_and_missing_values() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "dictionary.wit",
        "package test:dictionary; world dictionary { export run: func(count: u32, text: string) -> string; }",
    )?;
    let world = resolve.select_world(&[package], Some("dictionary"))?;
    let source = r#"
type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };
type Anchor = { anchor: number };
function anchor(value: JsonValue): Anchor { return value as unknown as Anchor; }
function json(value: JsonValue): JsonValue { return value; }
function array(value: JsonValue): JsonValue[] { return value as unknown as JsonValue[]; }
export function run(count: number, text: string): string {
  const root = JSON.parse('{"anchor":1,"z":2,"z":3,"00":4,"4294967295":5}');
  const first = anchor(root);
  const second = anchor(root);
  const holder = { item: first };
  const packed = json(holder).item;
  for(let i = 0; i < count; i++) root[i] = i;
  if(first !== second || packed !== root || first.anchor !== 1) throw 1;
  for(let i = 0; i < count; i++) {
    if(root[i] !== i || !Object.hasOwn(root, i)) throw 2;
    if(!delete root[i] || Object.hasOwn(root, i)) throw 3;
  }
  root.missing = undefined;
  if(root.missing !== undefined || !Object.hasOwn(root, 'missing')) throw 4;
  const snapshot = Object.keys(root);
  if(!delete root.anchor) throw 5;
  let stale = false;
  try { first.anchor; } catch(e) { stale = e instanceof TypeError; }
  if(!stale) throw 6;
  first.anchor = 7;
  root.text = text;
  root.nested = { a: undefined, b: [undefined, null, { c: undefined, d: 9 }] };
  if(packed !== root || second.anchor !== 7) throw 7;
  const dense = JSON.parse('[]');
  dense[0] = undefined;
  if(!Object.hasOwn(dense, 0) || Object.keys(dense).length !== 1) throw 8;
  let hole = false;
  try { delete dense[0]; } catch(e) { hole = e instanceof RangeError; }
  if(!hole || !delete dense[4]) throw 9;
  let invalid = false;
  try { array(dense); } catch(e) { invalid = e instanceof TypeError; }
  if(!invalid) throw 10;
  const typed: JsonValue[] = [null];
  const view = json(typed);
  let fixed = false;
  try { view[0] = undefined; } catch(e) { fixed = e instanceof TypeError; }
  if(!fixed || typed[0] !== null) throw 11;
  const native = { n: 1 };
  const nativeView = json(native);
  let closed = false;
  try { delete nativeView.n; } catch(e) { closed = e instanceof TypeError; }
  if(!closed || native.n !== 1) throw 12;
  let nullKeys = false;
  try { Object.keys(undefined); } catch(e) { nullKeys = e instanceof TypeError; }
  if(!nullKeys) throw 13;
  if(Object.keys('é🙂').length !== 3 || !Object.hasOwn('🙂', 1)) throw 14;
  if(Object.keys(native).length !== 1 || !Object.hasOwn(native, 'n')) throw 15;
  let prototype = false;
  try { root.__proto__ = 1; } catch(e) { prototype = e instanceof TypeError; }
  if(!prototype || Object.hasOwn(root, '__proto__')) throw 16;
  const assignment = (root.assignment = 'ok');
  if(assignment + '!' !== 'ok!') throw 17;
  const indexed = (root['indexed'] = true);
  if(!indexed) throw 18;
  delete root.assignment;
  delete root.indexed;
  root.snapshot = snapshot;
  root.current = Object.keys(root);
  return JSON.stringify(root);
}
"#;
    let compiled = compile_for_world(source, "dictionary.ts", &resolve, world, true)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, Peak::default());
    store.limiter(|peak| peak);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(u32, &str), (String,)>(&mut store, "run")?;
    let text = "é🙂".repeat(8_192);
    let expected = format!(
        r#"{{"z":3,"00":4,"4294967295":5,"anchor":7,"text":{},"nested":{{"b":[null,null,{{"d":9}}]}},"snapshot":["anchor","z","00","4294967295","missing"],"current":["z","00","4294967295","missing","anchor","text","nested","snapshot"]}}"#,
        serde_json::to_string(&text)?,
    );
    for _ in 0..3 {
        assert_eq!(run.call(&mut store, (256, &text))?.0, expected);
    }
    let peak = store.data().0;
    for _ in 0..20 {
        assert_eq!(run.call(&mut store, (256, &text))?.0, expected);
        assert_eq!(
            store.data().0,
            peak,
            "dictionary storage must be released with its task"
        );
    }
    Ok(())
}

#[test]
fn typed_dictionaries_keep_native_slots_and_checked_json_aliases() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("typed-dictionary.wit", "package test:typed-dictionary; world dictionaries { export run: func(count: u32, text: string) -> string; }")?;
    let world = resolve.select_world(&[package], Some("dictionaries"))?;
    let source = r#"
type JsonValue = null | boolean | number | string | JsonValue[] | {[key:string]:JsonValue};
interface Numbers { [key:string]: number }
type Tree = { [key: string]: Tree };
type Point = {x:number;y:number};
function numbers(value: JsonValue): Numbers { return value as unknown as Numbers; }
function tree(value: JsonValue): Tree { return value as unknown as Tree; }
function point(value: Numbers): Point { return value as unknown as Point; }
export function run(count:number, text:string):string {
  const native: Record<string, number> = {last:-0,first:2};
  if(1/native.last !== -1/0 || native.absent !== undefined) throw 1;
  const missing = native.absent + 1;
  if(missing === missing) throw 2;
  const keys = Object.keys(native);
  const values = Object.values(native);
  for(let i=0;i<count;i++) native[i] = i;
  for(let i=0;i<count;i++) { if(native[i] !== i || !delete native[i]) throw 3; }
  const packed: JsonValue = native;
  packed.first = 5;
  let mismatch=false;
  try { packed.first='bad'; } catch(e) { mismatch=e instanceof TypeError; }
  if(!mismatch || native.first !== 5 || keys[0] !== 'last' || values[1] !== 2) throw 4;
  const strings: {[key:string]:string} = {};
  strings.text=text;
  if(strings.absent+'!' !== 'undefined!') throw 5;
  const booleans:Record<string,boolean>={flag:false};
  if(booleans.flag !== false || booleans.absent !== undefined) throw 6;
  const arrays:Record<string,number[]>={first:[1,2]};
  const arrayValues=Object.values(arrays);
  arrays.first.push(3);
  if(arrayValues[0] !== arrays.first || arrayValues[0].length !== 3) throw 7;
  const bytes:Record<string,Uint8Array>={first:new Uint8Array([8,9])};
  if(!Object.hasOwn(bytes,'first') || Object.keys(bytes).length !== 1 || Object.values(bytes)[0][1] !== 9) throw 8;
  const optional:Record<string,number|undefined>={missing:undefined,present:3};
  if(!Object.hasOwn(optional,'missing') || optional.missing !== undefined) throw 9;
  const optionalValues=Object.values(optional);
  if(optionalValues[0] !== undefined || optionalValues[1]+2 !== 5) throw 10;
  const parsed=JSON.parse('{"first":1}');
  const checked=numbers(parsed);
  const again=numbers(parsed);
  checked.second=2;
  if(checked !== again || checked !== parsed || parsed.second !== 2) throw 11;
  parsed.first='bad';
  let stale=false;
  try { checked.first; } catch(e) { stale=e instanceof TypeError; }
  if(!stale) throw 12;
  parsed.first=undefined;
  stale=false;
  try { checked.first; } catch(e) { stale=e instanceof TypeError; }
  if(!stale || !delete checked.first || checked.first !== undefined) throw 13;
  checked.first=7;
  const record={existing:1};
  const recordJson:JsonValue=record;
  const closed=numbers(recordJson);
  closed.existing=2;
  let fixed=false;
  try { closed.added=3; } catch(e) { fixed=e instanceof TypeError; }
  if(!fixed || record.existing !== 2) throw 14;
  const coordinate:Numbers={y:4,x:3};
  const projected=point(coordinate);
  projected.x=6;
  if(coordinate.x !== 6 || projected !== coordinate) throw 15;
  const cyclic:Tree={};
  cyclic.self=cyclic;
  if(cyclic.self !== cyclic) throw 16;
  let cycle=false;
  try { JSON.stringify(cyclic); } catch(e) { cycle=e instanceof TypeError; }
  if(!cycle) throw 17;
  const parsedCycle=JSON.parse('{}');
  const recursive=tree(parsedCycle);
  recursive.self=recursive;
  if(recursive.self !== recursive) throw 18;
  delete recursive.self;
  return JSON.stringify([native,strings,booleans,arrays,optional,checked,projected,recursive]);
}
"#;
    let compiled = compile_for_world(source, "typed-dictionary.ts", &resolve, world, true)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, Peak::default());
    store.limiter(|peak| peak);
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(u32, &str), (String,)>(&mut store, "run")?;
    let text = "é🙂".repeat(8_192);
    let expected = format!(
        r#"[{{"last":0,"first":5}},{{"text":{}}},{{"flag":false}},{{"first":[1,2,3]}},{{"present":3}},{{"second":2,"first":7}},{{"y":4,"x":6}},{{}}]"#,
        serde_json::to_string(&text)?
    );
    for _ in 0..3 {
        assert_eq!(run.call(&mut store, (128, &text))?.0, expected);
    }
    let peak = store.data().0;
    for _ in 0..20 {
        assert_eq!(run.call(&mut store, (128, &text))?.0, expected);
        assert_eq!(
            store.data().0,
            peak,
            "typed dictionary storage leaked across tasks"
        );
    }
    Ok(())
}
