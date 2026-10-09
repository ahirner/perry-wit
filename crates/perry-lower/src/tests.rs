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
            .unwrap_or_else(|| panic!("expected rejection: {source}"))
    )
}

#[test]
fn text_codecs_preserve_typed_receivers_and_stream_options() {
    let printed = lowered(
        r#"
type Decoder = TextDecoder;
function encoder(): TextEncoder { return new (globalThis.TextEncoder)(); }
function decode(d: Decoder, input: Uint8Array): string { return d.decode(input,{stream:true}); }
export function run(input: string): string {
    const e=encoder();
    const d:Decoder=new (TextDecoder as typeof TextDecoder)('UTF8',{fatal:true,ignoreBOM:false});
    const bytes=e.encode(input);
    return decode(d,bytes)+d.decode()+e.encoding+d.encoding;
}
"#,
    );
    for intrinsic in [
        "TextEncoderNew",
        "TextEncode",
        "TextDecoderNew",
        "TextDecode",
    ] {
        assert!(printed.contains(intrinsic), "{printed}");
    }
}

#[test]
fn text_codec_options_reject_forms_the_frontend_cannot_preserve() {
    for source in [
        "new TextDecoder('utf8',{get fatal(){return true;}})",
        "new TextDecoder('utf8',{...{fatal:true}})",
        "new TextDecoder('utf8',{['fatal']:true})",
        "new TextDecoder().decode(new Uint8Array(0),{__proto__:null})",
    ] {
        let error = rejected(&format!("export function run():void{{{source};}}"));
        assert!(error.contains("builtin options"), "{error}");
    }
}

#[test]
fn string_optional_results_remain_optional_in_inferred_records() {
    let printed = lowered(
        r#"
export function run(input: string, index: number): string {
    const result = { point: input.codePointAt(index), at: input.at(index), nested: { value: input[index] } };
    return JSON.stringify(result);
}
"#,
    );
    assert!(
        printed.contains("point: perry.optional <builtin.fp64"),
        "{printed}"
    );
    assert!(
        printed.contains("at: perry.optional <perry.string"),
        "{printed}"
    );
    assert!(
        printed.contains("value: perry.optional <perry.string"),
        "{printed}"
    );
    assert_eq!(printed.matches("perry.unwrap").count(), 1, "{printed}");
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
            "export function run(x: number): number { const d: Record<string, number> = { bad: true }; return x; }",
            "value type differs",
        ),
        (
            "interface P { x: number; y: number|undefined } export function run(x: number): number { const p: P = { x }; return p.x; }",
            "missing required field",
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
fn recursive_records_get_a_reserved_name() {
    for declaration in [
        "interface Node { value: number; next: Node[] }",
        "type Node = { value: number; next: Node[] };",
    ] {
        let body = r#"
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
"#;
        let printed = lowered(&format!("{declaration}\n{body}"));
        assert!(printed.contains("shape_next_value_rec1"), "{printed}");
    }
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

#[test]
fn cli_intrinsics_keep_their_types_through_roundtrip() {
    let printed = lowered(
        r#"
export function values(): string[] {
  console.log("typed");
  console.error("host effects");
  const missing = process.env.PERRY_MISSING;
  if (missing !== undefined) console.log(missing);
  const now = performance.now();
  if (Date.now() < now) throw new Error("clock");
  return Object.keys(process.env);
}
"#,
    );
    use pliron::{irfmt::parsers::spaced, operation::verify_operation, parsable::parse_from_str};
    let ctx = &mut Context::new();
    let parsed = parse_from_str(spaced(Operation::top_level_parser()), ctx, &printed)
        .unwrap_or_else(|e| panic!("{}", e.disp(ctx)));
    verify_operation(parsed, ctx).unwrap_or_else(|e| panic!("{}", e.disp(ctx)));
    let invalid = printed.replace("perry.intrinsic Stdout", "perry.intrinsic MonotonicNow");
    assert_ne!(invalid, printed);
    let ctx = &mut Context::new();
    let parsed = parse_from_str(spaced(Operation::top_level_parser()), ctx, &invalid)
        .unwrap_or_else(|e| panic!("{}", e.disp(ctx)));
    assert!(verify_operation(parsed, ctx).is_err());
}

#[test]
fn json_values_narrow_to_native_scalars_and_shared_array_storage() {
    let printed = lowered(
        r#"
type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };
function array(value: JsonValue): JsonValue[] {
  if(!Array.isArray(value)) throw new TypeError('array required');
  return value;
}
export function run(text: string): number {
  const value: JsonValue = JSON.parse(text);
  const items = array(value);
  items.push(8);
  const item = items[0];
  if(typeof item !== 'number') throw new TypeError('number required');
  return item + 1;
}
"#,
    );
    for operation in [
        "perry.json_unpack",
        "JsonAsNumber",
        "perry.array_push",
        "perry.binary Add",
    ] {
        assert!(printed.contains(operation), "{operation}: {printed}");
    }
    let invalid = rejected(
        "type JsonValue = any; export function run(value: JsonValue): number { return 1; }",
    );
    assert!(invalid.contains("Any"), "{invalid}");
    let invalid = rejected(
        "export function run(): string { const stored = {__proto__: {x:1}, x:2}; return JSON.stringify(stored); }",
    );
    assert!(
        invalid.contains("prototype setters") || invalid.contains("only direct calls"),
        "{invalid}"
    );
}

#[test]
fn dictionaries_keep_native_types_and_recursive_aliases() {
    let printed = lowered(
        r#"
interface Numbers { [key: string]: number }
type Tree = { [key: string]: Tree };
function recursive(): Tree { const tree: Tree = {}; tree.self = tree; return tree; }
export function run(x: number): number {
  const values: Numbers = {first:x};
  const alias: Record<string, number> = values;
  alias.second = x + 1;
  const snapshot = Object.values(values);
  delete values.first;
  if(Object.hasOwn(values, 'first')) throw 1;
  const tree = recursive();
  if(tree.self !== tree) throw 2;
  return snapshot[0] + values.second;
}
"#,
    );
    for expected in [
        "perry.dictionary_new",
        "perry.dictionary Set",
        "perry.dictionary Get",
        "perry.enumerate Values",
        "perry.dictionary Delete",
        "perry.dictionary Has",
        "perry.dictionary <builtin.fp64",
        "<@dictionary_Tree",
    ] {
        assert!(printed.contains(expected), "missing {expected}: {printed}");
    }
    for declaration in [
        "type Dictionary = { [key: string]: number; required: number };",
        "type Dictionary = { required: number; [key: string]: number };",
        "interface Dictionary { [key: string]: number; required: number }",
    ] {
        let source = format!(
            "{declaration} export function run(): number {{ const d:Dictionary={{required:1,extra:2}};return d.required; }}"
        );
        assert!(lowered(&source).contains("perry.dictionary"));
        let error = rejected(&format!(
            "{declaration} export function run():number {{const d:Dictionary={{extra:1}};return d.required;}}"
        ));
        assert!(error.contains("missing required field"), "{error}");
    }
    let incompatible = rejected(
        "type Dictionary={ [key:string]:number; label:string };export function run(value:Dictionary):void{}",
    );
    assert!(
        incompatible.contains("incompatible with its index signature"),
        "{incompatible}"
    );
    for declaration in [
        "type Dictionary = { [key: number]: string };",
        "interface Dictionary { [key: number]: string }",
    ] {
        let invalid = rejected(&format!(
            "{declaration} export function run(): number {{ return 0; }}"
        ));
        assert!(invalid.contains("string key"), "{invalid}");
    }
}

#[test]
fn accepted_subset_exclusions_have_compile_time_diagnostics() {
    for (expression, diagnostic) in [
        ("JSON.parse('1',(_key,value)=>value)", "JSON.parse revivers"),
        (
            "JSON.stringify({x:1},(_key,value)=>value)",
            "JSON.stringify replacers and formatting",
        ),
        (
            "JSON.stringify({x:1},['x'])",
            "JSON.stringify replacers and formatting",
        ),
        (
            "JSON.stringify({x:1},null,2)",
            "JSON.stringify replacers and formatting",
        ),
        ("'a'.charCodeAt(0)", "charCodeAt is a UTF-16 code-unit API"),
        (
            "String.fromCharCode(65)",
            "fromCharCode is a UTF-16 code-unit API",
        ),
        ("new RegExp('a+')", "dynamic RegExp"),
        (
            "String.fromCharCode()",
            "fromCharCode is a UTF-16 code-unit API",
        ),
        (
            "String['fromCharCode']()",
            "fromCharCode is a UTF-16 code-unit API",
        ),
    ] {
        let error = rejected(&format!("export function run():void {{ {expression}; }}"));
        assert!(error.contains(diagnostic), "{expression}: {error}");
    }
}

#[test]
fn optional_record_fields_retain_type_presence_and_creation_order() {
    for declaration in [
        "interface Item { value:number; next?:Item; label?:string }",
        "type Item={value:number;next?:Item;label?:string};",
    ] {
        let printed = lowered(&format!(
            "{declaration} export function run():number {{ const item:Item={{value:1}}; item.next={{value:2}}; item.label=undefined; delete item.label; return item.next.value; }}"
        ));
        assert!(printed.contains("label?: perry.optional"), "{printed}");
        assert!(printed.contains("perry.record Delete"), "{printed}");
    }
}

#[test]
fn inline_dictionary_annotations_retain_index_and_named_constraints() {
    for source in [
        "export function run():number {const d:{[key:string]:number;count:number}={count:1};d.extra=2;return d.count+d.extra;}",
        "export function run(d:{[key:string]:number;count:number}):number {d.extra=2;return d.count+d.extra;}",
        "type Nested={inner:{[key:string]:number;count:number}};export function run(d:Nested):number{return d.inner.count;}",
    ] {
        let printed = lowered(source);
        let compact = printed.split_whitespace().collect::<String>();
        assert!(
            compact.contains("perry.dictionary<builtin.fp64{\"count\":builtin.fp64}>"),
            "{printed}"
        );
    }
}

#[test]
fn enumeration_retains_native_tuple_slots_and_inferred_values() {
    for source in [
        "export function run():number {const d:Record<string,number>={a:2};const entries=Object.entries(d);return entries[0][1]+3;}",
        "export function run():number {const d:Record<string,Uint8Array>={a:new Uint8Array([2])};const entries=Object.entries(d);return entries[0][1][0];}",
        "export function run():number {const d={a:2,b:3};const values=Object.values(d);return values[0]+values[1];}",
        "export function run():number {const pair:[string,number]=['a',2];pair[1]=3;return pair[1];}",
        "export function run():number {const entries:[string,number][]=Object.entries({a:2});return entries[0][1];}",
    ] {
        let printed = lowered(source);
        assert!(
            printed.contains("perry.enumerate") || printed.contains("perry.tuple_new"),
            "{printed}"
        );
        assert!(
            !printed.contains("perry.json_pack"),
            "native enumeration boxed: {printed}"
        );
    }
    for source in [
        "type Recursive=[Recursive];export function run(input:Recursive):number{return 0;}",
        "export function run():number {const pair:[string,number]=['a',2];pair[0]=3;return 0;}",
        "export function run():number {const values=Object.values({bytes:new Uint8Array([2]),n:1});return values.length;}",
    ] {
        rejected(source);
    }
}

#[test]
fn filesystem_options_reject_unpreserved_frontend_forms() {
    for options in [
        "{get bigint(){return false;}}",
        "{...{bigint:false}}",
        "{['bigint']:false}",
        "{__proto__:null}",
        "options",
    ] {
        let error = rejected(&format!(
            "import {{stat as metadata}} from 'fs/promises'; export async function run():Promise<void>{{const options={{bigint:false}};await metadata('input',{options});}}"
        ));
        assert!(error.contains("builtin options"), "{error}");
    }
}
