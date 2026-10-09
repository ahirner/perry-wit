//! End to end: TypeScript → HIR → MIR → wasm dialect → core Wasm linked with
//! `perry-runtime`, executed in wasmtime with fuel metering (raw exports, no component).
use perry_codegen::status_code;
use perry_mir::attrs::ErrorKind;
use wasmtime::{Config, Engine, Instance, Module, Store, Val};

fn build(source: &str) -> Vec<u8> {
    perry_wit::compile_core(source, "case.ts").unwrap_or_else(|e| panic!("{e:#}"))
}

struct Program {
    store: Store<()>,
    instance: Instance,
    wasm: Vec<u8>,
}

impl Program {
    fn new(source: &str) -> Self {
        let wasm = build(source);
        wasmparser::Validator::new()
            .validate_all(&wasm)
            .unwrap_or_else(|e| panic!("{e}\n{}", wasmprinter::print_bytes(&wasm).unwrap()));
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).unwrap();
        let mut store = Store::new(&engine, ());
        store.set_fuel(u64::MAX).unwrap();
        let module = Module::new(&engine, &wasm).unwrap();
        assert_eq!(module.imports().len(), 0, "the runtime is linked in");
        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        Program {
            store,
            instance,
            wasm,
        }
    }

    /// Call an export; returns its results and the fuel the call consumed.
    fn call(&mut self, name: &str, args: &[Val]) -> (Vec<Val>, u64) {
        let function = self.instance.get_func(&mut self.store, name).unwrap();
        let mut results = vec![Val::I32(0); function.ty(&self.store).results().len()];
        let before = self.store.get_fuel().unwrap();
        function
            .call(&mut self.store, args, &mut results)
            .unwrap_or_else(|e| panic!("{e:?}\n{}", wasmprinter::print_bytes(&self.wasm).unwrap()));
        (results, before - self.store.get_fuel().unwrap())
    }

    fn number(&mut self, name: &str, x: f64) -> f64 {
        let (results, _) = self.call(name, &[Val::F64(x.to_bits())]);
        results[0].unwrap_f64()
    }

    /// `(value, status)` of an export that may throw.
    fn throwing(&mut self, name: &str, x: f64) -> (f64, i32) {
        let (results, _) = self.call(name, &[Val::F64(x.to_bits())]);
        assert_eq!(results.len(), 2, "expected a status result");
        (results[0].unwrap_f64(), results[1].unwrap_i32())
    }
}

#[test]
fn scalar_loops_calls_and_remainder() {
    let mut program = Program::new(
        r#"
function step(acc: number, i: number): number {
  return (acc + i * 3) % 1000003;
}
export function run(x: number): number {
  let acc = 0;
  for (let i = 0; i < x; i++) {
    acc = step(acc, i);
  }
  let k = 0;
  while (true) {
    k++;
    if (k > 10) { break; }
    if (k % 2 === 0) { continue; }
    acc = acc - 1;
  }
  return acc;
}
"#,
    );
    let reference = |x: f64| {
        let mut acc = 0.0;
        let mut i = 0.0;
        while i < x {
            acc = (acc + i * 3.0) % 1000003.0;
            i += 1.0;
        }
        acc - 5.0
    };
    for x in [0.0, 1.0, 10.0, 1000.0] {
        assert_eq!(program.number("run", x), reference(x), "x={x}");
    }
    assert_eq!(program.number("run", 7.5), reference(7.5));
}

#[test]
fn records_arrays_and_holes() {
    let mut program = Program::new(
        r#"
interface Point { x: number; y: number }
function make(i: number): Point {
  return { y: i * 2, x: i };
}
export function run(x: number): number {
  const items: Point[] = [];
  for (let i = 0; i < x; i++) {
    items.push(make(i));
  }
  let sum = 0;
  for (let k = 0; k < items.length; k++) {
    const point = items[k];
    point.x = point.x + 1;
    sum = (sum + point.x + point.y) % 1000003;
  }
  const pair: number[] = [sum, x];
  pair[1] = pair[0];
  pair[2] = 5;
  return pair[1] + pair[2];
}
"#,
    );
    let reference = |x: f64| {
        let mut sum = 0.0;
        let mut i = 0.0;
        while i < x {
            sum = (sum + (i + 1.0) + i * 2.0) % 1000003.0;
            i += 1.0;
        }
        sum + 5.0
    };
    for x in [0.0, 3.0, 100.0, 5000.0] {
        assert_eq!(program.throwing("run", x), (reference(x), 0), "x={x}");
    }
}

#[test]
fn out_of_bounds_reads_are_undefined_and_typed_uses_throw() {
    let mut program = Program::new(
        r#"
interface Point { x: number; y: number }
export function lookup(n: number): number {
  const xs: number[] = [1, 2];
  const missing = xs[n];
  if (missing === undefined) {
    return -1;
  }
  return missing * 10;
}
export function nan(n: number): number {
  const xs: number[] = [1, 2];
  return xs[n] * 2;
}
export function field(n: number): number {
  const points: Point[] = [{ x: 4, y: 5 }];
  const p = points[n];
  return p.y;
}
export function hole(n: number): number {
  const xs: number[] = [1];
  xs[n] = 3;
  return xs.length;
}
"#,
    );
    assert_eq!(program.number("lookup", 1.0), 20.0);
    for missing in [2.0, -1.0, 0.5, 1e10, f64::NAN] {
        assert_eq!(program.number("lookup", missing), -1.0, "index {missing}");
    }
    assert_eq!(program.number("lookup", -0.0), 10.0);
    assert_eq!(program.number("nan", 0.0), 2.0);
    assert!(program.number("nan", 9.0).is_nan());
    // `undefined.y` is a catchable TypeError, reported as a status, not a trap.
    assert_eq!(program.throwing("field", 0.0), (5.0, 0));
    assert_eq!(
        program.throwing("field", 1.0),
        (0.0, status_code(ErrorKind::TypeError))
    );
    // Writing at `length` appends; anything further out is a RangeError.
    assert_eq!(program.throwing("hole", 0.0), (1.0, 0));
    assert_eq!(program.throwing("hole", 1.0), (2.0, 0));
    assert_eq!(
        program.throwing("hole", 3.0).1,
        status_code(ErrorKind::RangeError)
    );
}

/// The P0.1 scaling contracts that fail on the legacy backend, on the MIR backend: fuel for
/// 4n must stay within 6× of fuel for n (warm-up n first, one instance, as in
/// `crates/conformance/src/scaling.rs`). Values are returned directly; the `{ value, trace }`
/// record needs the component boundary (P6.1).
/// Async functions awaiting each other without a host: values live across awaits stay in the
/// frame, loops run through awaits, and a rejection propagates as the export's status.
#[test]
fn async_functions_await_each_other() {
    let mut program = Program::new(
        r#"
async function double(x: number): Promise<number> {
  return x * 2;
}
async function check(x: number): Promise<number> {
  const xs: number[] = [1];
  return xs[x] as number;
}
async function sum(n: number): Promise<number> {
  let total = 0;
  const offset = n * 100;
  for (let i = 0; i < n; i++) {
    total = total + (await double(i));
  }
  return total + offset;
}
export async function run(n: number): Promise<number> {
  const a = await sum(n);
  const b = await double(a);
  return a + b;
}
export async function fails(n: number): Promise<number> {
  const before = n + 1;
  const v = await check(n);
  return before + v;
}
"#,
    );
    // sum(4) = 2 * (0 + 1 + 2 + 3) + 400 = 412; run = 412 + 824.
    assert_eq!(program.throwing("run", 4.0), (1236.0, 0));
    assert_eq!(program.throwing("run", 0.0), (0.0, 0));
    assert_eq!(program.throwing("fails", 0.0), (2.0, 0));
    assert_eq!(
        program.throwing("fails", 3.0).1,
        status_code(ErrorKind::TypeError)
    );
}

/// `throw`, `try`/`catch`/`finally` and exception objects, in sync and async functions.
#[test]
fn exceptions_are_thrown_caught_and_cleaned_up() {
    let mut program = Program::new(
        r#"
function check(x: number): number {
  if (x < 0) { throw new RangeError("negative"); }
  if (x > 100) { throw x; }
  return x;
}
function classify(x: number): number {
  try {
    check(x);
    const xs: number[] = [1];
    return xs[x] as number;
  } catch (e) {
    if (e instanceof RangeError) { return e.message.length * 1000; }
    if (e instanceof TypeError) { return -1; }
    if (e instanceof Error) { return -2; }
    return -3;
  }
}
function cleanup(x: number): number {
  let log = 0;
  for (let i = 0; i < 3; i++) {
    try {
      if (i === 1) { continue; }
      if (x === i) { throw new Error("stop"); }
      log = log * 10 + 1;
    } catch {
      log = log * 10 + 2;
      break;
    } finally {
      log = log * 10 + 3;
    }
  }
  return log;
}
function rethrows(x: number): number {
  try {
    try { throw new TypeError("inner"); } finally { x = x + 1; }
  } catch (e) {
    if (e instanceof TypeError && e.message === e.message) { return x; }
  }
  return 0;
}
async function failing(x: number): Promise<number> {
  if (x > 1) { throw new TypeError("async"); }
  return x;
}
async function awaits(x: number): Promise<number> {
  try {
    return await failing(x);
  } catch (e) {
    if (e instanceof TypeError) { return 70; }
    return 71;
  } finally {
    x = 0;
  }
}
export function runClassify(x: number): number { return classify(x); }
export function runCleanup(x: number): number { return cleanup(x); }
export function runRethrows(x: number): number { return rethrows(x); }
export function uncaught(x: number): number { return check(x); }
export async function runAwaits(x: number): Promise<number> { return await awaits(x); }
"#,
    );
    assert_eq!(program.number("runClassify", 0.0), 1.0);
    assert_eq!(program.number("runClassify", 1.0), -1.0);
    assert_eq!(program.number("runClassify", -5.0), 8000.0);
    assert_eq!(program.number("runClassify", 500.0), -3.0);
    // Digits: 1 normal end, 2 catch, 3 finally; `continue` and `break` run `finally` too.
    assert_eq!(program.number("runCleanup", 9.0), 13313.0);
    assert_eq!(program.number("runCleanup", 2.0), 13323.0);
    assert_eq!(program.number("runCleanup", 0.0), 23.0);
    assert_eq!(program.number("runRethrows", 4.0), 5.0);
    assert_eq!(program.throwing("uncaught", 7.0), (7.0, 0));
    assert_eq!(
        program.throwing("uncaught", -1.0).1,
        status_code(ErrorKind::RangeError)
    );
    assert_eq!(
        program.throwing("uncaught", 101.0).1,
        status_code(ErrorKind::Value)
    );
    assert_eq!(program.throwing("runAwaits", 1.0), (1.0, 0));
    assert_eq!(program.throwing("runAwaits", 5.0), (70.0, 0));
}

/// `Promise.all` and `Promise.race` over async functions, and `typeof`.
#[test]
fn promise_combinators_and_typeof() {
    let mut program = Program::new(
        r#"
async function value(x: number): Promise<number> { return x; }
async function fail(x: number): Promise<number> { throw new RangeError("no"); }
export async function all(x: number): Promise<number> {
  const values = await Promise.all([value(x), value(x + 1), value(2)]);
  const empty: Promise<number>[] = [];
  const none = await Promise.all(empty);
  return (values[0] as number) * 100 + (values[1] as number) * 10 + (values[2] as number) + none.length;
}
export async function race(x: number): Promise<number> {
  const first = value(x);
  await first;
  return await Promise.race([first, value(x + 5)]);
}
export async function rejects(x: number): Promise<number> {
  try {
    await Promise.all([value(x), fail(x)]);
    return 0;
  } catch (e) {
    if (e instanceof RangeError) { return 1; }
    return 2;
  }
}
export function types(x: number): number {
  const xs: number[] = [x];
  const text = JSON.stringify(xs);
  const maybe = xs[3];
  let score = 0;
  if (typeof x === "number") { score = score + 1; }
  if (typeof text === "string") { score = score + 10; }
  if (typeof maybe === "undefined") { score = score + 100; }
  if (typeof JSON.parse("[1]") === "object") { score = score + 1000; }
  return score;
}
"#,
    );
    assert_eq!(program.throwing("all", 3.0), (342.0, 0));
    assert_eq!(program.throwing("race", 4.0), (4.0, 0));
    assert_eq!(program.throwing("rejects", 1.0), (1.0, 0));
    assert_eq!(program.number("types", 1.0), 1111.0);
}

#[test]
fn scaling_contracts_are_linear() {
    let workloads = [
        (
            "retained_records",
            r#"
interface Point { x: number; y: number }
export function run(x: number): number {
  const items: Point[] = [];
  for (let i = 0; i < x; i++) {
    items.push({ x: i, y: i * 2 });
  }
  let sum = 0;
  for (let k = 0; k < items.length; k++) {
    const point = items[k];
    sum = (sum + point.x + point.y) % 1000003;
  }
  return sum;
}
"#,
        ),
        (
            "number_push",
            r#"
export function run(x: number): number {
  const items: number[] = [];
  for (let i = 0; i < x; i++) {
    items.push(i * 3);
  }
  let sum = 0;
  for (let k = 0; k < items.length; k++) {
    sum = (sum + items[k]) % 1000003;
  }
  return sum;
}
"#,
        ),
        (
            "temporary_records",
            r#"
interface Point { x: number; y: number }
function make(i: number): Point {
  return { x: i, y: i * 2 };
}
export function run(x: number): number {
  let sum = 0;
  for (let i = 0; i < x; i++) {
    const point = make(i);
    sum = (sum + point.x + point.y) % 1000003;
  }
  return sum;
}
"#,
        ),
    ];
    let expected = |x: u32| (0..x).fold(0u64, |sum, i| (sum + 3 * u64::from(i)) % 1000003) as f64;
    for (name, source) in workloads {
        let mut program = Program::new(source);
        let mut fuel = Vec::new();
        for n in [512u32, 512, 2048, 100_000] {
            let (results, used) = program.call("run", &[Val::F64(f64::from(n).to_bits())]);
            assert_eq!(results[0].unwrap_f64(), expected(n), "{name} n={n}");
            fuel.push(used);
        }
        let ratio = fuel[2] as f64 / fuel[1] as f64;
        let per_item = fuel[3] as f64 / 100_000.0;
        println!(
            "{name}: fuel n=512 {} 4n {} ratio {ratio:.2}, {per_item:.1}/item at 1e5",
            fuel[1], fuel[2]
        );
        assert!(ratio <= 6.0, "{name}: fuel ratio {ratio:.2} > 6 ({fuel:?})");
    }
}

#[test]
#[ignore]
fn dump() {
    let source = std::env::var("PERRY_DUMP").expect("PERRY_DUMP=<file.ts>");
    let wasm = build(&std::fs::read_to_string(source).unwrap());
    println!(
        "{} bytes\n{}",
        wasm.len(),
        wasmprinter::print_bytes(&wasm).unwrap()
    );
}
