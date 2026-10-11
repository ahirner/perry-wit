//! End to end: TypeScript → HIR → MIR → wasm dialect → core Wasm linked with
//! `perry-runtime`, executed in wasmtime with fuel metering (raw exports, no component):
//! control flow, exceptions, async functions, records and arrays, and code size.
use perry_codegen::status_code;
use perry_mir::attrs::ErrorKind;
use wasmtime::Val;

use crate::core_program::Program;

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
fn unreachable_functions_do_not_add_code_or_host_requirements() {
    let source = r#"
function recur(n: number): number { if (n <= 0) return 2; return recur(n - 1) + 1; }
async function next(n: number): Promise<number> { return recur(n); }
export async function run(n: number): Promise<number> { return await next(n); }
"#;
    let extra = r#"
function unusedCycle(n: number): number { return unusedOther(n); }
function unusedOther(n: number): number { return unusedCycle(n); }
async function unusedFetch(): Promise<string> { return await (await fetch('https://example.test')).text(); }
function unusedLog(): void { console.log('unreachable'); }
"#;
    let build = |source: &str| perry_wit::compile_core(source, "case.ts").expect("source compiles");
    let core = build(source);
    assert_eq!(core, build(&format!("{source}{extra}")));
    let mut program = Program::from_wasm(core);
    assert_eq!(program.number("run", 5.0), 7.0);
}

#[test]
fn linked_function_names_identify_executing_frames() {
    let mut program = Program::new(
        "export function spin(n: number): string { let text = ''; while (n > 0) { text += String(n); n--; } return text; }",
    );
    program.store.set_fuel(50_000).unwrap();
    let function = program
        .instance
        .get_func(&mut program.store, "spin")
        .unwrap();
    let error = function
        .call(
            &mut program.store,
            &[Val::F64(1_000.0_f64.to_bits())],
            &mut [Val::I32(0)],
        )
        .unwrap_err();
    let trace = error
        .downcast_ref::<wasmtime::WasmBacktrace>()
        .expect("fuel trap has guest frames");
    assert!(
        trace
            .frames()
            .iter()
            .any(|frame| frame.func_name() == Some("spin")),
        "{error:#}"
    );
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

/// `Promise.all` over async functions, including an empty list, and `typeof`. Race and
/// rejection propagation are conformance contracts (`ecma.promise.race`, `ecma.error.values`).
#[test]
fn promise_all_and_typeof() {
    let mut program = Program::new(
        r#"
async function value(x: number): Promise<number> { return x; }
export async function all(x: number): Promise<number> {
  const values = await Promise.all([value(x), value(x + 1), value(2)]);
  const empty: Promise<number>[] = [];
  const none = await Promise.all(empty);
  return (values[0] as number) * 100 + (values[1] as number) * 10 + (values[2] as number) + none.length;
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
    assert_eq!(program.number("types", 1.0), 1111.0);
}

#[test]
#[ignore]
fn dump() -> anyhow::Result<()> {
    let source = std::env::var("PERRY_DUMP").expect("PERRY_DUMP=<file.ts>");
    let wasm = perry_wit::compile_core(&std::fs::read_to_string(&source)?, &source)?;
    println!("{} bytes\n{}", wasm.len(), wasmprinter::print_bytes(&wasm)?);
    Ok(())
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
    if (JSON.stringify(parsed) !== '{"value":null,"next":{"label":"é"}}') return -2;
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
