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
