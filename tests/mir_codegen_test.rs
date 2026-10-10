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
        Self::from_wasm(wasm)
    }

    fn from_wasm(wasm: Vec<u8>) -> Self {
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

#[test]
fn temporary_records_use_bounded_memory_across_iterations() {
    let mut program = Program::new(
        r#"
interface Point { x: number; y: number }
function make(i: number): Point { return { x: i, y: i * 2 }; }
export function run(n: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const point = make(i);
    if (i % 2 === 0) point.x = point.x + 1;
    sum = (sum + point.x + point.y) % 1000003;
  }
  return sum;
}

"#,
    );
    let mut expected = 0.0;
    for i in 0..200_000 {
        expected = (expected + f64::from(i * 3 + i32::from(i % 2 == 0))) % 1000003.0;
    }
    assert_eq!(program.number("run", 200_000.0), expected);
    let memory = program
        .instance
        .get_memory(&mut program.store, "memory")
        .unwrap();
    assert!(memory.data_size(&program.store) <= 512 * 1024);
}

#[test]
fn unchanged_module_graph_does_not_collect_on_every_export() {
    let source = r#"
interface Cell { value: number; next: Cell | null }
function build(): Cell {
  let head: Cell = { value: 0, next: null };
  for (let i = 1; i < 5000; i++) head = { value: i, next: head };
  return head;
}
const saved: Cell = build();
export function run(n: number): number { return saved.value + n; }
"#;
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(source, "retained.ts").unwrap();
    let mut program = Program::from_wasm(wasm);
    assert_eq!(program.number("run", 1.0), 5000.0);
    let (before, _) = program.call("__perry_gc_counter", &[Val::I32(1)]);
    for i in 0..100 {
        assert_eq!(program.number("run", f64::from(i)), 4999.0 + f64::from(i));
    }
    let (after, _) = program.call("__perry_gc_counter", &[Val::I32(1)]);
    assert_eq!(after[0].unwrap_i64(), before[0].unwrap_i64());
}

#[test]
fn collection_reclaims_overwrites_and_preserves_reachable_cycles() {
    let mut program = Program::new(
        r#"
interface Node { value: number; next: Node | null }
export function run(n: number): number {
  const first: Node = { value: 7, next: null };
  const second: Node = { value: 11, next: first };
  first.next = second;
  const latest: Node[] = [first];
  for (let i = 0; i < n; i++) latest[0] = { value: i, next: null };
  const next = first.next;
  if (next === null) return -1;
  if (next.next !== first) return -2;
  return first.value + next.value + latest[0].value;
}
"#,
    );
    assert_eq!(program.throwing("run", 200_000.0), (200_017.0, 0));
    let memory = program
        .instance
        .get_memory(&mut program.store, "memory")
        .unwrap();
    assert!(
        memory.data_size(&program.store) <= 512 * 1024,
        "{} bytes",
        memory.data_size(&program.store)
    );
}

#[test]
fn completed_async_frames_and_promises_are_collectible() {
    let mut program = Program::new(
        r#"
interface Cell { value: number; next: Cell | null }
async function advance(cell: Cell): Promise<Cell> {
  const pending = Promise.resolve(cell);
  const first = await pending;
  const again = await pending;
  return { value: first.value + again.value, next: null };
}
export async function run(n: number): Promise<number> {
  let current: Cell = { value: 1, next: null };
  for (let i = 0; i < n; i++) {
    const cell: Cell = { value: i, next: current };
    current = await advance(cell);
  }
  return current.value;
}
"#,
    );
    assert_eq!(program.throwing("run", 200_000.0), (399_998.0, 0));
    let memory = program
        .instance
        .get_memory(&mut program.store, "memory")
        .unwrap();
    assert!(
        memory.data_size(&program.store) <= 512 * 1024,
        "completed async objects accumulated: {} bytes",
        memory.data_size(&program.store)
    );
}

#[test]
fn builtin_churn_preserves_shared_views_and_reclaims_temporaries() {
    let mut program = Program::new(
        r#"
export function run(n: number): number {
  const initial = new TextEncoder().encode('é🙂!');
  const retained = initial.subarray(2, 6);
  const prefix = ('prefix-' + n).slice(0, 6);
  const parsed = JSON.parse('{"keep":{"value":17},"drop":[1,2,3]}').keep;
  let total = 0;
  for (let i = 0; i < n; i++) {
    const bytes = new Uint8Array([i, 7, 8]);
    const tail = bytes.subarray(1);
    tail[0] = 9;
    const text = ('é🙂-' + i).slice(1, 3);
    total = (total + text.length + bytes[1]) % 100003;
    const json = JSON.parse('{"value":5}');
    total = (total + json.value) % 100003;
  }
  return total + retained.length + prefix.length + parsed.value;
}
"#,
    );
    assert_eq!(
        program.throwing("run", 200_000.0),
        ((200_000.0 * 16.0) % 100003.0 + 27.0, 0)
    );
    let memory = program
        .instance
        .get_memory(&mut program.store, "memory")
        .unwrap();
    let warmed = memory.data_size(&program.store);
    assert!(
        warmed <= 1024 * 1024,
        "builtin churn retained {warmed} bytes"
    );
    program.throwing("run", 400_000.0);
    assert_eq!(
        memory.data_size(&program.store),
        warmed,
        "builtin memory did not stabilize"
    );
}

#[test]
fn astral_strings_use_scalar_positions_and_keep_append_prefixes() {
    let mut program = Program::new(
        r#"
export function run(n: number): number {
  let text = '🙂é';
  const prefix = text + '!';
  text = prefix;
  for (let i = 0; i < n; i++) { text = text + '漢'; }
  if (prefix !== '🙂é!') { throw new Error('retained prefix changed'); }
  if (text.slice(0, 2) !== '🙂é') { throw new Error('scalar slice'); }
  if (text.charAt(0) !== '🙂') { throw new Error('scalar character'); }
  if (text.codePointAt(text.length) !== undefined) { throw new Error('out of bounds'); }
  return text.length + text.indexOf('!') + text.codePointAt(0);
}
"#,
    );
    assert_eq!(program.throwing("run", 100_000.0), (228_583.0, 0));
}

#[test]
fn record_updates_evaluate_the_receiver_once_and_preserve_prefix_values() {
    let mut program = Program::new(
        r#"
interface Cell { value: number }
function receiver(cell: Cell, visits: number[]): Cell { visits.push(1); return cell; }
export function run(x: number): number {
    const cell: Cell = { value: x };
    const visits: number[] = [];
    const first = receiver(cell, visits).value++;
    const second = --receiver(cell, visits).value;
    return first * 100 + second * 10 + cell.value + visits.length * 1000;
}
"#,
    );
    assert_eq!(program.number("run", 3.0), 2333.0);
}

#[test]
fn inferred_arrays_preserve_optional_elements_in_either_order() {
    let mut program = Program::new(
        r#"
export function run(x: number): number {
    const bytes = new Uint8Array([7]);
    const a = [bytes[x], 11];
    const b = [13, bytes[x]];
    if (a[0] === undefined && b[1] === undefined) { return a[1] + b[0]; }
    return a[0] + b[1];
}
"#,
    );
    assert_eq!(program.number("run", 0.0), 14.0);
    assert_eq!(program.number("run", 1.0), 24.0);
}

#[test]
fn forced_collection_poisoning_preserves_suspended_and_persistent_roots() {
    let source = r#"
interface Cell { value: number; next: Cell | null }
let saved: Cell = { value: 7, next: null };
saved.next = saved;
async function compute(n: number): Promise<number> {
    const bytes = new TextEncoder().encode('é🙂');
    const alias = bytes.subarray(2);
    const payload = JSON.parse('{"value":11}');
    const cell: Cell = { value: n, next: saved };
    const settled = await Promise.all([Promise.resolve(n), Promise.resolve(n + 1)]);
    for (let i = 0; i < n; i++) {
        const temporary: Cell = { value: i, next: null };
        saved.value = temporary.value;
        const text = 'iteration:' + i;
        if (text.length < 2) throw new Error('bad text');
    }
    return cell.value + cell.next.value + alias[0] + payload.value + settled[1];
}
export async function run(n: number): Promise<number> { return await compute(n); }
"#;
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(source, "stress.ts").unwrap();
    let mut program = Program::from_wasm(wasm);
    program.call("__perry_gc_debug", &[Val::I32(3)]);
    for n in [40.0, 80.0, 120.0] {
        assert_eq!(program.throwing("run", n), (3.0 * n + 251.0, 0));
        for counter in [5, 6] {
            let (count, _) = program.call("__perry_gc_counter", &[Val::I32(counter)]);
            assert_eq!(
                count[0].unwrap_i64(),
                0,
                "completed task retained counter {counter}"
            );
        }
    }
    let (collections, _) = program.call("__perry_gc_counter", &[Val::I32(1)]);
    assert!(
        collections[0].unwrap_i64() > 240,
        "safe points did not force collection"
    );
    let (live, _) = program.call("__perry_gc_counter", &[Val::I32(2)]);
    assert!(
        live[0].unwrap_i64() < 128,
        "only module state should survive"
    );
}

#[test]
fn proven_frame_records_reuse_storage_across_calls_and_iterations() {
    let source = r#"
interface Cell { value: number }
export function inspect(cell: Cell): number {
    if (cell.value < 0) throw new RangeError('negative');
    let result = 0;
    for (let i = 0; i < 3; i++) result = result + cell.value;
    return result;
}
export function run(n: number): number {
    let result = 0;
    for (let i = 0; i < n; i++) {
        const cell: Cell = { value: i };
        result = result + inspect(cell);
    }
    return result;
}
"#;
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(source, "frames.ts").unwrap();
    let mut program = Program::from_wasm(wasm);
    program.call("__perry_gc_debug", &[Val::I32(3)]);
    assert_eq!(program.number("run", 40.0), 2340.0);
    program.call("__perry_gc_debug", &[Val::I32(0)]);
    assert_eq!(
        program.number("run", 200_000.0),
        3.0 * 200_000.0 * 199_999.0 / 2.0
    );
    let (allocations, _) = program.call("__perry_gc_counter", &[Val::I32(0)]);
    assert_eq!(
        allocations[0].unwrap_i64(),
        2,
        "one frame per activation, independent of iterations"
    );
    let (live, _) = program.call("__perry_gc_counter", &[Val::I32(2)]);
    assert_eq!(live[0].unwrap_i64(), 0);
    assert!(
        program
            .instance
            .get_memory(&mut program.store, "memory")
            .unwrap()
            .data_size(&program.store)
            <= 512 * 1024
    );
}

#[test]
fn mutable_dates_survive_calls_suspension_and_collection() {
    let source = r#"
let saved: Date = new Date(0);
function set(date: Date, time: number): number { return date.setTime(time); }
export async function run(n: number): Promise<number> {
    const alias = saved;
    const dates: Date[] = [saved];
    for (let i = 0; i < n; i++) {
        const temporary = new Date(i);
        set(temporary, i + 1);
        saved = temporary;
    }
    const result = await Promise.resolve(saved.getTime());
    set(alias, 7);
    return result + dates[0].getTime();
}
"#;
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(source, "dates.ts").unwrap();
    let mut program = Program::from_wasm(wasm);
    let initial = program
        .instance
        .get_memory(&mut program.store, "memory")
        .unwrap()
        .data_size(&program.store);
    program.call("__perry_gc_debug", &[Val::I32(3)]);
    assert_eq!(program.throwing("run", 120.0), (127.0, 0));
    program.call("__perry_gc_debug", &[Val::I32(0)]);
    assert_eq!(program.throwing("run", 200_000.0), (200_007.0, 0));
    let size = program
        .instance
        .get_memory(&mut program.store, "memory")
        .unwrap()
        .data_size(&program.store);
    let (live, _) = program.call("__perry_gc_counter", &[Val::I32(2)]);
    assert!(
        size <= 512 * 1024,
        "initial={initial} size={size} live={live:?}"
    );
}

#[test]
fn temporal_values_survive_collection_and_reclaim_temporary_storage() {
    let source = r#"
interface CalendarValues { date: Temporal.PlainDate; time: Temporal.PlainDateTime; }
let saved: Temporal.Instant = Temporal.Instant.fromEpochMilliseconds(-1);
export async function run(n: number): Promise<number> {
    const original = saved;
    const values: CalendarValues = { date: Temporal.PlainDate.from('2024-02-28'),
        time: Temporal.PlainDateTime.from('2024-02-28T12:30:00.123456789') };
    const dates: Temporal.PlainDate[] = [values.date];
    for (let i = 0; i < n; i++) {
        saved = Temporal.Instant.fromEpochMilliseconds(i);
        const next = values.date.add({ days: 1 });
        if (next.day !== 29 || next.toString() !== '2024-02-29') throw 1;
        if (saved.toString().length === 0) throw 2;
    }
    const settled = await Promise.resolve(saved);
    if (original.toString().length === 0 || dates[0].day !== 28) throw 3;
    if (values.time.nanosecond !== 789) throw 4;
    return settled.epochMilliseconds;
}
"#;
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(source, "temporal.ts").unwrap();
    let mut program = Program::from_wasm(wasm);
    program.call("__perry_gc_debug", &[Val::I32(3)]);
    assert_eq!(program.throwing("run", 120.0), (119.0, 0));
    program.call("__perry_gc_debug", &[Val::I32(0)]);
    for _ in 0..2 {
        assert_eq!(program.throwing("run", 200_000.0), (199_999.0, 0));
        let memory = program
            .instance
            .get_memory(&mut program.store, "memory")
            .unwrap();
        assert!(memory.data_size(&program.store) <= 512 * 1024);
    }
}

#[test]
fn frame_storage_is_released_on_exception_and_finalizer_paths() {
    let source = r#"
interface Cell { value: number }
function inspect(cell: Cell): number {
    let result = 0;
    for (let i = 0; i < 3; i++) result = result + cell.value;
    return result;
}
function fail(n: number): number {
    const cell: Cell = { value: n };
    try { if (n > 0) throw n; return inspect(cell); }
    finally { if (inspect(cell) < 0) throw 77; }
}
export function run(n: number): number {
    let total = 0;
    for (let i = 0; i < n; i++) {
        try { total = total + fail(i); } catch (error) { total = total + 1; }
    }
    return total;
}
"#;
    let wasm =
        perry_wit::compiler::compile_core_with_memory_checks(source, "frame-throws.ts").unwrap();
    let mut program = Program::from_wasm(wasm);
    program.call("__perry_gc_debug", &[Val::I32(3)]);
    assert_eq!(program.throwing("run", 120.0), (119.0, 0));
    for counter in [2, 5, 6] {
        let (count, _) = program.call("__perry_gc_counter", &[Val::I32(counter)]);
        assert_eq!(
            count[0].unwrap_i64(),
            0,
            "completed task retained counter {counter}"
        );
    }
}

#[test]
fn literal_regex_search_allocates_nothing_and_uses_scalar_positions() {
    let source = r#"
export function run(n: number): number {
    const expression = /[a-z]+\d{2}/u;
    const text = '🙂x42';
    let total = 0;
    for (let i = 0; i < n; i++) total = total + text.search(expression) + (expression.test(text) ? 1 : 0);
    return total;
}
"#;
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(source, "regex.ts").unwrap();
    let mut program = Program::from_wasm(wasm);
    assert_eq!(program.number("run", 20_000.0), 40_000.0);
    let (allocations, _) = program.call("__perry_gc_counter", &[Val::I32(0)]);
    assert_eq!(allocations[0].unwrap_i64(), 0);
}

#[test]
fn incremental_decoder_copies_only_pending_bytes_and_survives_collection() {
    let source = r#"
export async function run(n: number): Promise<number> {
    const decoder = new TextDecoder('utf-8', { fatal: true });
    const alias = decoder;
    const head = new Uint8Array([0xf0, 0x9f]);
    const before = decoder.decode(head, { stream: true });
    head.fill(65);
    for (let i = 0; i < n; i++) {
        const garbage = 'temporary:' + i;
        if (garbage.length < 5) throw new Error('bad string');
    }
    await Promise.resolve(n);
    const result = alias.decode(new Uint8Array([0x99, 0x82]));
    return before.length + result.length + result.codePointAt(0);
}
"#;
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(source, "decoder.ts").unwrap();
    let mut program = Program::from_wasm(wasm);
    program.call("__perry_gc_debug", &[Val::I32(3)]);
    assert_eq!(program.throwing("run", 120.0), (128579.0, 0));
    for counter in [2, 5, 6] {
        let (count, _) = program.call("__perry_gc_counter", &[Val::I32(counter)]);
        assert_eq!(
            count[0].unwrap_i64(),
            0,
            "completed decoder task retained counter {counter}"
        );
    }
}

#[test]
fn finite_variant_graphs_survive_forced_collection_and_checked_json() {
    let source = r#"
type Value = { tag: 'empty' } | { tag: 'number', val: number } | { tag: 'text', val: string } | { tag: 'point', val: { x: number, label: string } };
let retained: Value = { tag: 'point', val: { x: 9, label: 'retained' } };
function make(n: number): Value {
    if (n % 3 === 0) return { tag: 'number', val: n };
    if (n % 3 === 1) return { tag: 'text', val: 'value:' + n };
    return { tag: 'point', val: { x: n, label: 'point:' + n } };
}
export function run(n: number): number {
    let value: Value = { tag: 'empty' };
    for (let i = 0; i < n; i++) value = make(i);
    const parsed: Value = JSON.parse('{"tag":"point","val":{"x":7,"label":"json"}}');
    let total = 0;
    if (retained.tag === 'point') total = retained.val.x + retained.val.label.length;
    if (parsed.tag === 'point') total = total + parsed.val.x;
    if (value.tag === 'number') total = total + value.val;
    if (value.tag === 'text') total = total + value.val.length;
    if (value.tag === 'point') total = total + value.val.x;
    if (JSON.stringify(parsed) !== '{"tag":"point","val":{"label":"json","x":7}}') throw new Error('serialization');
    try {
        const bad: Value = JSON.parse('{"tag":"number","val":null}');
        return -1;
    } catch (error) { if (!(error instanceof TypeError)) return -2; }
    try {
        const bad: Value = JSON.parse('{"tag":"unknown"}');
        return -3;
    } catch (error) { if (!(error instanceof TypeError)) return -4; }
    return total;
}
"#;
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(source, "variants.ts").unwrap();
    let mut program = Program::from_wasm(wasm);
    program.call("__perry_gc_debug", &[Val::I32(3)]);
    for n in [3000.0, 3001.0, 3002.0] {
        let expected = 24.0 + if n == 3002.0 { 10.0 } else { n - 1.0 };
        assert_eq!(program.throwing("run", n), (expected, 0));
    }
    let (live, _) = program.call("__perry_gc_counter", &[Val::I32(2)]);
    assert!(
        live[0].unwrap_i64() < 4096,
        "discarded variant payloads remained live"
    );
}

#[test]
fn switches_preserve_tests_fallthrough_and_finalizers() {
    let mut program = Program::new(
        r#"
let trace = 0;
function mark(n: number): number { trace = trace * 10 + n; return n; }
export function run(n: number): number {
    let total = 0;
    for (let i = 0; i < n; i++) {
        try {
            switch (i) {
                case 0: total += 10; break;
                case 1: total += 20;
                case 2: total += 3; continue;
                default: total += 40;
                case 4: total += 5; break;
            }
        } finally { total += 100; }
        total += 1;
    }
    return total;
}
export function dispatch(n: number): number {
    trace = 0;
    let value = 0;
    switch (n) {
        case mark(1): value = 1; break;
        default: value = 4;
        case mark(2): value += 2; break;
        case mark(3): value = 3;
    }
    return trace * 10 + value;
}
"#,
    );
    assert_eq!(program.number("run", 6.0), 735.0);
    for (value, expected) in [(1.0, 11.0), (2.0, 122.0), (3.0, 1233.0), (4.0, 1236.0)] {
        assert_eq!(program.number("dispatch", value), expected);
    }
}

#[test]
fn optional_closed_fields_keep_null_distinct_from_missing_values() {
    let mut program = Program::new(
        r#"
interface Options { label?: string; value?: number | null; next?: Options | null; }
function read(value: Options): number {
    let result = 0;
    if (value.label !== undefined) result += value.label.length;
    if (value.value === undefined) result += 10;
    else if (value.value === null) result += 20;
    else result += value.value;
    const next = value.next;
    if (next !== undefined && next !== null) result += read(next);
    return result;
}
export function run(n: number): number {
    const empty: Options = {};
    const nullable: Options = { value: null, next: { label: 'abc', value: n } };
    const parsed: Options = JSON.parse('{"value":null,"next":{"label":"é"}}');
    if (JSON.stringify(empty) !== '{}') return -1;
    if (JSON.stringify(parsed) !== '{"next":{"label":"é"},"value":null}') return -2;
    empty.label = 'x';
    empty.value = 5;
    empty.label = undefined;
    empty.value = undefined;
    return read(empty) + read(nullable) + read(parsed);
}
"#,
    );
    assert_eq!(program.throwing("run", 7.0), (71.0, 0));
}

#[test]
fn closed_number_string_values_preserve_tags_equality_and_checked_json() {
    let source = r#"
interface Item { value?: number | string | null; }
function text(value: number | string): string {
    if (typeof value === 'string') return value;
    return JSON.stringify(value);
}
function size(value: number | string): number {
    if (typeof value === 'number') { return value + 1; }
    else { return value.length; }
}
function same(a: number | string, b: number | string): boolean { return a === b; }
let retained: Item = {};
export function run(n: number): number {
    const absent: Item = {};
    const nil: Item = {value: null};
    if (typeof absent.value !== 'undefined' || typeof nil.value !== 'object') return -1;
    const values: (number | string)[] = [n, 'é😀'];
    if (text(values[0]) !== '7' || text(values[1]) !== 'é😀') return -2;
    if (!same('abc', 'a' + 'bc') || same(7, '7') || same(NaN, NaN) || !same(0, -0)) return -3;
    const parsed: Item = JSON.parse('{"value":"ok"}');
    if (typeof parsed.value !== 'string') return -4;
    if (JSON.stringify(parsed) !== '{"value":"ok"}') return -5;
    if (JSON.stringify(values) !== '[7,"é😀"]') return -6;
    if (String(values[0]) !== '7' || String(values[1]) !== 'é😀') return -7;
    if (values[0] !== 7 || values[1] !== 'é😀') return -8;
    let sum = 0;
    for (let i = 0; i < 3000; i++) {
        const item: Item = {value: i};
        retained = item;
        if (typeof item.value === 'number') sum += item.value;
    }
    if (retained.value !== 2999) return -9;
    try { const bad: Item = JSON.parse('{"value":true}'); return -10; }
    catch (error) { if (!(error instanceof TypeError)) return -11; }
    return size(values[0]) + size(values[1]) + sum;
}
"#;
    let wasm =
        perry_wit::compiler::compile_core_with_memory_checks(source, "number-string.ts").unwrap();
    let mut program = Program::from_wasm(wasm);
    program.call("__perry_gc_debug", &[Val::I32(3)]);
    assert_eq!(program.throwing("run", 7.0), (4_498_510.0, 0));
    let (live, _) = program.call("__perry_gc_counter", &[Val::I32(2)]);
    assert!(live[0].unwrap_i64() < 4096);
}

#[test]
fn allocation_free_calls_do_not_collect_or_publish_roots() {
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(
        r#"
function leaf(n: number): number { if (n < 0) return -n; return n + 1; }
function step(n: number): number { return leaf(n) + 2; }
export function run(n: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) sum += step(i);
  return sum;
}
"#,
        "no-gc-calls.ts",
    )
    .unwrap();
    let mut program = Program::from_wasm(wasm);
    program.call("__perry_gc_debug", &[Val::I32(3)]);
    assert_eq!(program.number("run", 1000.0), 502_500.0);
    for (index, expected) in [(0, 0), (1, 1), (6, 0), (7, 0)] {
        let (counter, _) = program.call("__perry_gc_counter", &[Val::I32(index)]);
        assert_eq!(counter[0].unwrap_i64(), expected, "counter {index}");
    }
}

#[test]
fn nested_root_stack_growth_preserves_caller_objects_and_unwinds() {
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(
        r#"
interface Cell { value: number }
export function read(cell: Cell): number { if (cell.value < 0) throw new RangeError('negative'); return cell.value; }
function descend(n: number, fail: boolean): number {
  const cell = { value: n };
  if (n === 0) { if (fail) throw new RangeError("bottom"); return read(cell); }
  try { return descend(n - 1, fail) + read(cell); }
  catch (e) { return read(cell) + 1000; }
}
export function run(n: number): number {
  const a = descend(n, false);
  const b = descend(n, true);
  return a + b;
}
"#,
        "root-stack.ts",
    )
    .unwrap();
    let mut program = Program::from_wasm(wasm);
    program.call("__perry_gc_debug", &[Val::I32(3)]);
    for n in [64.0, 120.0, 64.0] {
        assert_eq!(program.number("run", n), n * (n + 1.0) + 1000.0);
        for index in [2, 5, 6] {
            let (counter, _) = program.call("__perry_gc_counter", &[Val::I32(index)]);
            assert_eq!(counter[0].unwrap_i64(), 0, "counter {index}");
        }
    }
}

/// `%` takes `i32.rem_s` inline for `i32` operands and `fmod` otherwise; both agree with
/// ECMAScript (Rust's `f64 %` is `fmod`), bit for bit, including `-0`, NaN and infinities.
#[test]
fn remainder_matches_fmod() {
    let mut program = Program::new(
        r#"
export function rem(a: number, b: number): number { return a % b; }
export function rem7(a: number): number { return a % -7; }
"#,
    );
    let values = [
        0.0,
        -0.0,
        1.0,
        -1.0,
        2.0,
        -4.0,
        7.0,
        -7.0,
        7.5,
        -7.5,
        1e9,
        -1e9,
        2147483647.0,
        -2147483648.0,
        2147483648.0,
        -2147483649.0,
        1e300,
        f64::MIN_POSITIVE,
        f64::from_bits(1),
        -f64::from_bits(1),
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
    ];
    let same =
        |got: f64, want: f64| got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan());
    for a in values {
        let (got, _) = program.call("rem7", &[Val::F64(a.to_bits())]);
        let got = got[0].unwrap_f64();
        assert!(same(got, a % -7.0), "{a} % -7 = {got}");
        for b in values {
            let args = [Val::F64(a.to_bits()), Val::F64(b.to_bits())];
            let (got, _) = program.call("rem", &args);
            let got = got[0].unwrap_f64();
            assert!(same(got, a % b), "{a} % {b} = {got}, want {}", a % b);
        }
    }
}

#[test]
fn non_error_messages_remain_collectible() {
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(
        r#"
export function run(n: number): number {
  let sum = 0;
  try { throw 0; } catch (error) {
    for (let i = 0; i < n; i++) sum += error.message.length;
  }
  return sum;
}
"#,
        "non-error-message.ts",
    )
    .unwrap();
    let mut program = Program::from_wasm(wasm);
    program.call("__perry_gc_debug", &[Val::I32(3)]);
    assert_eq!(program.throwing("run", 1000.0), (0.0, 0));
    program.call("__perry_gc_debug", &[Val::I32(0)]);
    assert_eq!(program.throwing("run", 200_000.0), (0.0, 0));
    assert!(
        program
            .instance
            .get_memory(&mut program.store, "memory")
            .unwrap()
            .data_size(&program.store)
            <= 512 * 1024
    );
    for index in [2, 6] {
        let (counter, _) = program.call("__perry_gc_counter", &[Val::I32(index)]);
        assert_eq!(counter[0].unwrap_i64(), 0);
    }
}

#[test]
fn branch_leaf_inlining_preserves_effects_and_caught_throws() {
    let mut program = Program::new(
        r#"
let order = 0;
function argument(n: number): number { order = order * 10 + n; return n; }
function branch(a: number, b: number): number {
  if (a < b) return a * 100 + b;
  if (a === b) return a + b;
  return b - a;
}
function throwing(n: number): number {
  if (n < 0) throw new RangeError('negative');
  return branch(n, n + 1);
}
export function run(n: number): number {
  order = 0;
  const result = branch(argument(1), argument(2));
  try { throwing(-n); return -1; }
  catch (e) { if (!(e instanceof RangeError)) return -2; }
  return result + order * 1000 + branch(n, n);
}
"#,
    );
    assert_eq!(program.number("run", 7.0), 12_116.0);
}

#[test]
fn promoted_record_arguments_stay_fresh_when_aliases_can_mutate() {
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(
        r#"
interface Cell { value: number }
let saved: Cell = { value: 0 };
function change(): number { saved.value += 10; return 3; }
function read(cell: Cell, n: number): number {
  if (n < 0) return cell.value - n;
  return cell.value + n;
}
function mutated(cell: Cell): number {
  const before = cell.value;
  const middle = change();
  return before + middle + cell.value;
}
function escaped(cell: Cell): Cell { saved = cell; return cell; }
export function run(n: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const cell: Cell = { value: i };
    // A call in a caught region stays a call: promotion can still remove the record.
    try { sum += read(cell, i); } catch (e) { return -1; }
  }
  saved = escaped({ value: 7 });
  return sum + mutated(saved);
}
"#,
        "promoted-record.ts",
    )
    .unwrap();
    let mut program = Program::from_wasm(wasm);
    program.call("__perry_gc_debug", &[Val::I32(3)]);
    assert_eq!(program.number("run", 1000.0), 999_027.0);
    let (allocations, _) = program.call("__perry_gc_counter", &[Val::I32(0)]);
    assert!(allocations[0].unwrap_i64() < 10);
}

#[test]
fn repeated_leaf_calls_keep_a_bounded_compilation_size() {
    let calls = (0..80)
        .map(|i| format!("sum += choose(n, {i});"))
        .collect::<String>();
    let source = format!(
        r#"
function choose(a: number, b: number): number {{
  if (a < b) return a + b;
  if (a > b) return a - b;
  return a * b;
}}
export function run(n: number): number {{ let sum = 0; {calls} return sum; }}
"#
    );
    let mut program = Program::new(&source);
    let expected = (0..80)
        .map(|i| {
            let b = i as f64;
            if 7.0 < b {
                7.0 + b
            } else if 7.0 > b {
                7.0 - b
            } else {
                7.0 * b
            }
        })
        .sum::<f64>();
    assert_eq!(program.number("run", 7.0), expected);
    assert!(
        program.wasm.len() < 15_000,
        "bounded inlining grew the component unexpectedly"
    );
}

#[test]
fn range_proofs_keep_optional_elements_and_invalid_joined_indices() {
    let mut program = Program::new(
        r#"
export function run(n: number): number {
  const values: (number | null | undefined)[] = [null, undefined, 7];
  let score = 0;
  for (let i = 0; i < values.length; i++) {
    const value = values[i];
    if (value === null) score += 10;
    else if (value === undefined) score += 100;
    else score += value;
  }
  const bytes = new Uint8Array([11, 13, 17]);
  let index = 0;
  if (n !== 0) index = n;
  if (index < bytes.length) {
    const value = bytes[index];
    if (value === undefined) return score + 1000;
    return score + value;
  }
  return score + 2000;
}
"#,
    );
    for (index, expected) in [
        (0.0, 128.0),
        (-0.0, 128.0),
        (1.0, 130.0),
        (-1.0, 1117.0),
        (0.5, 1117.0),
        (3.0, 2117.0),
        (f64::NAN, 2117.0),
        (f64::INFINITY, 2117.0),
    ] {
        assert_eq!(program.number("run", index), expected, "index={index}");
    }
}

#[test]
fn dominating_unwraps_do_not_cross_caught_failure_paths() {
    let mut program = Program::new(
        r#"
interface Cell { x: number }
export function caught(n: number): number {
  const cells: Cell[] = [{x:7}]; const value = cells[n]; let sum = 0;
  try { sum += value.x; } catch (e) { sum += 100; }
  try { sum += value.x; } catch (e) { sum += 1000; }
  return sum;
}
export function dominated(n: number): number {
  const cells: Cell[] = [{x:7}]; const value = cells[n];
  const first = value.x; let sum = first;
  for (let i=0; i<100; i++) sum += value.x;
  return sum;
}
"#,
    );
    assert_eq!(program.number("caught", 0.0), 14.0);
    assert_eq!(program.number("caught", 1.0), 1100.0);
    assert_eq!(program.throwing("dominated", 0.0), (707.0, 0));
    assert_eq!(
        program.throwing("dominated", 1.0),
        (0.0, status_code(ErrorKind::TypeError))
    );
}

#[test]
fn owned_appends_preserve_observers_and_scalar_cursors() {
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(
        r#"
let escaped = '';
function keep(value: string): number { escaped = value; return value.length; }
export function run(n: number): number {
  let text = '';
  const saved: string[] = [];
  for (let i = 0; i < n; i++) {
    if (i === 10 || i === 100) saved.push(text);
    if (i === 20) keep(text);
    text = text + 'é😀x';
    if (text[text.length - 2] !== '😀') return -1;
    if (text[text.length - 1] !== 'x') return -2;
  }
  if (saved[0].length !== 30 || saved[1].length !== 300 || escaped.length !== 60) return -3;
  if (saved[0][28] !== '😀' || saved[1][299] !== 'x') return -4;
  const prefix = text.slice(0, 17);
  text = text + '尾';
  if (prefix.length !== 17 || prefix[16] !== '😀') return -5;
  let self = 'é😀';
  for (let i = 0; i < 5; i++) self = self + self;
  if (self.length !== 64 || self[63] !== '😀') return -6;
  let left = 'é' + '😀';
  let right = left + '';
  right = right + 'x';
  if (left !== 'é😀' || right !== 'é😀x') return -7;
  let emptyLeft = '' + left;
  emptyLeft = emptyLeft + 'z';
  if (left !== 'é😀' || emptyLeft !== 'é😀z') return -8;
  let merged = '';
  for (let i = 0; i < 100; i++) {
    const old = merged;
    if (i % 2 === 0) merged = merged + 'a';
    else merged = merged + '😀';
    if (old.length !== i) return -9;
  }
  if (merged.length !== 100 || merged[99] !== '😀') return -10;
  return text.length;
}
"#,
        "owned-append.ts",
    )
    .unwrap();
    let mut program = Program::from_wasm(wasm);
    program.call("__perry_gc_debug", &[Val::I32(3)]);
    for n in [1000.0, 2000.0, 1000.0] {
        assert_eq!(program.throwing("run", n), (3.0 * n + 1.0, 0));
    }
}

#[test]
fn exclusive_utf8_append_keeps_scalar_counts_with_few_allocations() {
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(
        r#"
export function run(n: number): number {
  let text = '';
  for (let i = 0; i < n; i++) text = text + 'é😀x';
  return text.length;
}
"#,
        "scalar-append.ts",
    )
    .unwrap();
    let mut program = Program::from_wasm(wasm);
    program.call("__perry_gc_debug", &[Val::I32(3)]);
    assert_eq!(program.number("run", 1000.0), 3000.0);
    let (allocations, _) = program.call("__perry_gc_counter", &[Val::I32(0)]);
    assert!(
        allocations[0].unwrap_i64() < 100,
        "headers should be reused"
    );
}

#[test]
fn async_dispatch_preserves_cyclic_loop_values() {
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(
        r#"
async function step(n: number): Promise<number> { return await Promise.resolve(n + 1); }
export async function run(n: number): Promise<number> {
  let a = 1; let b = 2; let c = 3;
  const marker = 'é' + String(n) + '😀';
  for (let i = 0; i < n; i++) {
    const old = a; a = b; b = c; c = await step(old + i);
    if (marker[0] !== 'é' || marker[marker.length - 1] !== '😀') throw 1;
  }
  return a * 10000 + b * 100 + c;
}
"#,
        "async-dispatch.ts",
    )
    .unwrap();
    let mut program = Program::from_wasm(wasm);
    program.call("__perry_gc_debug", &[Val::I32(3)]);
    for n in [0, 1, 2, 3, 19, 100] {
        let (mut a, mut b, mut c) = (1, 2, 3);
        for i in 0..n {
            (a, b, c) = (b, c, a + i + 1);
        }
        assert_eq!(
            program.throwing("run", n as f64),
            ((a * 10000 + b * 100 + c) as f64, 0)
        );
    }
}
