use crate::lower_source;
use pliron::{context::Context, op::Op, operation::Operation, printable::Printable};

/// Lower, verify, promote slots, clean up, verify again; return the printed module.
fn lowered(source: &str) -> String {
    let ctx = &mut Context::new();
    let module = lower_source(ctx, source, "case.ts", crate::EntryPoint::Exports)
        .unwrap_or_else(|e| panic!("{e:#}"));
    let op = module.get_operation();
    perry_mir::pipeline::canonicalize(op, ctx).unwrap_or_else(|e| panic!("{}", e.disp(ctx)));
    let printed = Operation::get_op_dyn(op, ctx).disp(ctx).to_string();
    for slot_op in ["perry.local", "perry.load", "perry.store"] {
        assert!(
            !printed.contains(slot_op),
            "{slot_op} survived mem2reg:\n{printed}"
        );
    }
    printed
}

fn rejected(source: &str) -> String {
    let ctx = &mut Context::new();
    format!(
        "{:#}",
        lower_source(ctx, source, "case.ts", crate::EntryPoint::Exports)
            .err()
            .expect("rejection")
    )
}

#[test]
fn scalar_loop_becomes_ssa() {
    let printed = lowered(
        r#"
export function run(x: number): number {
  let acc = 0;
  for (let i = 0; i < x; i++) {
    acc = (acc + i * 3) % 1000003;
  }
  return acc;
}
"#,
    );
    assert!(printed.contains("perry.binary Rem"), "{printed}");
    if std::env::var_os("PERRY_MIR_PRINT").is_some() {
        eprintln!("{printed}");
    }
    assert!(printed.contains("perry.cond_br if"), "{printed}");
}

#[test]
fn calls_branches_and_recursion() {
    let printed = lowered(
        r#"
function fib(n: number): number {
  if (n < 2) {
    return n;
  }
  return fib(n - 1) + fib(n - 2);
}
export function run(x: number): number {
  return x > 0 && x < 30 ? fib(x) : -x;
}
"#,
    );
    assert_eq!(printed.matches("perry.call fib").count(), 3, "{printed}");
}

#[test]
fn while_break_continue_and_strings() {
    let printed = lowered(
        r#"
export function run(x: number): number {
  let text = "";
  let i = 0;
  while (true) {
    i++;
    if (i > x) {
      break;
    }
    if (i % 2 === 0) {
      continue;
    }
    text = text + "x";
  }
  do {
    i--;
  } while (i > 0 && !(i === 5));
  return text.length + i;
}
"#,
    );
    assert!(printed.contains("perry.string_concat"), "{printed}");
    assert!(printed.contains("perry.convert I32ToF64"), "{printed}");
}

#[test]
fn unsupported_forms_are_rejected_with_diagnostics() {
    for (source, expected) in [
        (
            "export function run(x: number): number { return x ** 2; }",
            "operator Pow",
        ),
        (
            "function* count(): Generator<number> { yield 1; } export function run(x: number): number { return x; }",
            "generator function",
        ),
        (
            "export function run(x: number): number { if (x) { return 1; } return 0; }",
            "conditions must be boolean",
        ),
        (
            "export function run(x: number): number { const d: Record<string, number> = {}; return x; }",
            "MIR backend does not support",
        ),
        (
            "interface P { x: number; y?: number } export function run(x: number): number { const p: P = { x }; return p.x; }",
            "optional field",
        ),
    ] {
        let error = rejected(source);
        assert!(error.contains(expected), "{source}: {error}");
    }
}

#[test]
fn records_unify_by_shape_and_arrays_grow() {
    let printed = lowered(
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
  return pair[1];
}
"#,
    );
    // The interface and the `{ y, x }` literal are one record, fields in sorted order.
    assert!(
        printed.contains("perry.record <shape_x_y { x: builtin.fp64 , y: builtin.fp64  }>"),
        "{printed}"
    );
    assert!(!printed.contains("__AnonShape"), "{printed}");
    for op in [
        "perry.alloc Unassigned",
        "perry.array_new Unassigned",
        "perry.array_push",
        "perry.array_get",
        "perry.array_set",
        "perry.field_set",
        "perry.unwrap",
    ] {
        assert!(printed.contains(op), "missing {op}:\n{printed}");
    }
    // Indices stay f64: the bounds and integrality check is part of `perry.array_get`.
    assert!(!printed.contains("perry.convert F64ToI32"), "{printed}");
}

#[test]
fn out_of_bounds_reads_are_undefined_not_traps() {
    let printed = lowered(
        r#"
export function run(n: number): number {
  const xs: number[] = [1, 2];
  const missing = xs[n];
  if (missing === undefined) {
    return -1;
  }
  return xs[n + 5] * 2 + missing;
}
"#,
    );
    for op in [
        "perry.optional <builtin.fp64 >",
        "perry.is_undefined",
        "perry.to_number",
    ] {
        assert!(printed.contains(op), "missing {op}:\n{printed}");
    }
    // Arithmetic coerces undefined to NaN; nothing on this path throws.
    assert!(!printed.contains("perry.unwrap"), "{printed}");
}

#[test]
fn typed_boundaries_unwrap_with_type_error() {
    let printed = lowered(
        r#"
interface Point { x: number; y: number }
function norm(p: Point): number {
  return p.x + p.y;
}
export function run(n: number): string {
  const points: Point[] = [{ x: n, y: n }];
  const names: string[] = ["a"];
  const total = norm(points[n]);
  return "name: " + names[n + total];
}
"#,
    );
    for op in ["perry.to_string", "perry.unwrap"] {
        assert!(printed.contains(op), "missing {op}:\n{printed}");
    }
}

#[test]
fn literal_fields_evaluate_in_source_order() {
    let printed = lowered(
        r#"
interface Point { x: number; y: number }
function trace(v: number): number {
  return v;
}
export function run(n: number): number {
  const p: Point = { y: trace(1), x: trace(2) };
  return p.x - p.y;
}
"#,
    );
    let first = printed.find("perry.call trace").expect("calls");
    let call_args: Vec<_> = printed[first..]
        .lines()
        .filter(|l| l.contains("perry.call trace"))
        .collect();
    assert_eq!(call_args.len(), 2, "{printed}");
    // y's initializer (1) is computed before x's (2); the allocation then orders x before y.
    let one = printed.find("builtin.double 1>").expect("1");
    let two = printed.find("builtin.double 2>").expect("2");
    assert!(one < two, "{printed}");
}

#[test]
fn same_field_names_with_other_types_are_distinct_records() {
    let printed = lowered(
        r#"
interface Inner { v: number; c: Leaf }
interface Leaf { v: number }
interface Outer { v: number; c: Inner }
function depth(o: Outer): number {
  return o.c.c.v + o.c.v + o.v;
}
export function run(n: number): number {
  return depth({ v: n, c: { v: 2, c: { v: 3 } } });
}
"#,
    );
    // Inner and Outer share field names but not field types.
    assert!(printed.contains("shape_c_v {"), "{printed}");
    assert!(printed.contains("shape_c_v_2 {"), "{printed}");
}

#[test]
fn recursive_interfaces_get_a_reserved_name() {
    let printed = lowered(
        r#"
interface Node { value: number; next: Node[] }
function total(node: Node): number {
  let sum = node.value;
  for (let i = 0; i < node.next.length; i++) {
    sum = sum + total(node.next[i]);
  }
  return sum;
}
export function run(n: number): number {
  const leaf: Node = { value: n, next: [] };
  return total({ value: 1, next: [leaf, leaf] });
}
"#,
    );
    assert!(printed.contains("shape_next_value_rec1"), "{printed}");
}

#[test]
#[ignore]
fn dump_hir() {
    let path = std::env::var("PERRY_HIR").expect("PERRY_HIR=<file.ts>");
    let source = std::fs::read_to_string(&path).unwrap();
    let ast = perry_parser::parse_typescript(&source, "case.ts").unwrap();
    let hir = perry_hir::lower_module(&ast, "main", "case.ts").unwrap();
    println!("{:#?}", hir.functions);
    println!("IMPORTS {:#?}", hir.imports);
}

#[test]
fn async_functions_await_promises_and_timers() {
    let printed = lowered(
        r#"
import { setTimeout } from 'node:timers/promises';
async function later(v: number): Promise<number> {
  const a = v + 1;
  await setTimeout(1);
  return a;
}
async function nothing(): Promise<void> {
  await setTimeout(0);
}
export async function run(x: number): Promise<number> {
  const value = await setTimeout(0, x);
  const pending = later(value);
  await nothing();
  return (await pending) + (await pending);
}
"#,
    );
    for op in [
        "perry.async_call later",
        "perry.await",
        "perry.timer",
        "perry.promise <builtin.fp64 >",
        "perry_async",
    ] {
        assert!(printed.contains(op), "missing {op}:\n{printed}");
    }
}

#[test]
fn finalizers_keep_exception_values_live_across_awaits() {
    lowered(
        r#"
import { setTimeout } from 'node:timers/promises';
async function fail(error: Error): Promise<number> {
  await setTimeout(1);
  throw error;
}
export async function run(x: number): Promise<number> {
  const original = new Error("same");
  const pending = fail(original);
  let total = x;
  for (let i = 0; i < 3; i++) {
    try {
      try { total = total + await pending; }
      finally { await setTimeout(1); }
    } catch (e) {
      if (e === original) total = total + 1;
    } finally { await setTimeout(1); total = total + 10; }
  }
  return total;
}
"#,
    );
}

#[test]
fn module_execution_requires_a_command_world() {
    let message = rejected("throw new Error('must execute');");
    assert!(
        message.contains("top-level execution needs a wasi:cli/run command world"),
        "{message}"
    );
}
