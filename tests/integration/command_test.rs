//! Commands: top-level statements (with top-level `await`) implement `wasi:cli/run`. Console
//! output drains through WASI P3 streams, `process.exitCode`/`process.exit` and uncaught
//! exceptions decide the result, and exceptions are values `try`/`catch` can inspect.
use crate::world::Compiler::Standard;
use anyhow::Result;
use wasmtime::component::Component;
use wasmtime_wasi::{FsPerms, WasiCtx, p3::bindings::Command};

use crate::engine::async_engine;
use crate::output_capture;
use crate::wasi_host::{self, Host};
use output_capture::MemoryOutput;

const WIT: &str = "package test:command; world command { include wasi:cli/command@0.3.0; }";

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

async fn execute(source: &str) -> Result<Run> {
    execute_in(source, None).await
}

/// Run with `root` preopened as `/` when given.
async fn execute_in(source: &str, root: Option<&std::path::Path>) -> Result<Run> {
    execute_output(source, root, 1 << 20).await
}

async fn execute_output(
    source: &str,
    root: Option<&std::path::Path>,
    output_limit: usize,
) -> Result<Run> {
    let bytes = Standard.component(WIT, "command", "main.ts", source)?;
    let engine = async_engine()?;
    let component = Component::new(&engine, bytes)?;
    let (stdout, stderr) = (MemoryOutput::new(output_limit), MemoryOutput::new(1 << 20));
    let mut builder = WasiCtx::builder();
    builder.stdout(stdout.clone()).stderr(stderr.clone());
    if let Some(root) = root {
        builder.preopened_dir(root, "/", FsPerms::ReadWrite)?;
    }
    let mut store = wasi_host::store(&engine, Host::new(builder.build()));
    let linker = wasi_host::linker(&engine)?;
    let command = Command::instantiate_async(&mut store, &component, &linker).await?;
    let result = store
        .run_concurrent(async move |store| command.wasi_cli_run().call_run(store).await)
        .await??;
    let text = |output: &MemoryOutput| String::from_utf8(output.contents().to_vec()).unwrap();
    Ok(Run {
        ok: result.is_ok(),
        stdout: text(&stdout),
        stderr: text(&stderr),
    })
}

#[tokio::test(flavor = "current_thread")]
async fn callable_instances_preserve_arguments_identity_captures_and_exceptions() -> Result<()> {
    let run = execute(
        r#"
function failure(fn) { try { fn(); return "none"; } catch(e) { return String(e); } }
function twice(f,x) { return f(f(x)); }
function inc(n) { return n+1; }
function apply(f:(n:number)=>number, x:number):number { return f(x); }
function add(a,b) { return a+b; }
function upper(s) { return s.toUpperCase(); }
const alias=inc;
console.log(failure(()=>{throw new Error("boom");}),failure(()=>1),twice(n=>n+1,1));
console.log(alias===inc, alias===add, apply(alias,2), [1,2].map(inc).join(","));
console.log([1,2,3].reduce((acc,v)=>add(acc,v),0), [3,1,2].sort((a,b)=>add(a,-b)).join(","),
    "a-b".replace(/[a-z]/g,m=>upper(m)));
function check(condition,message) { if(!condition) throw new Error(message); }
function contract(value) {
    let checks=0;
    function verify(condition,message) { check(condition,message); checks++; }
    const again=verify;
    again(true,"ok");
    const first=failure(()=>again(value>2,"small"));
    return first+" "+checks;
}
console.log(contract(3),contract(1));
"#,
    )
    .await?;
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(
        run.stdout,
        "Error: boom none 3\ntrue false 3 2,3\n6 1,2,3 A-B\nnone 2 Error: small 1\n"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn console_timers_and_exceptions() -> Result<()> {
    let run = execute(
        r#"
import { setTimeout } from 'node:timers/promises';
async function later(value: number) {
  await setTimeout(1);
  if (value < 0) { throw new RangeError("negative " + value); }
  return value * 2;
}
console.log("hello", 1.5, true, -3);
console.error("to stderr");
let total = 0;
for (let i = 0; i < 3; i++) {
  total = total + (await later(i));
  console.log("step " + i);
}
try {
  await later(-1);
} catch (e) {
  if (e instanceof RangeError) console.log("caught " + e.message);
} finally {
  console.log("finally " + total);
}
"#,
    )
    .await?;
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(
        run.stdout,
        "hello 1.5 true -3\nstep 0\nstep 1\nstep 2\ncaught negative -1\nfinally 6\n"
    );
    assert_eq!(run.stderr, "to stderr\n");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn nested_exits_and_uncaught_exceptions_decide_the_command_result() -> Result<()> {
    // `process.exitCode` and a top-level `process.exit` are `node.process.exit_status` contracts.
    // `process.exit` unwinds without running `catch` or `finally`.
    let run = execute(r#"
function stop() { process.exit(0); return 1; }
try { console.log("a"); stop(); } catch { console.log("caught"); } finally { console.log("finally"); }
console.log("unreachable");
"#)
    .await?;
    assert!(run.ok);
    assert_eq!(run.stdout, "a\n");

    let run = execute(r#"console.log("x"); throw new TypeError("bad");"#).await?;
    assert!(!run.ok);
    assert_eq!(run.stdout, "x\n");
    assert_eq!(run.stderr, "Uncaught TypeError: bad\n");

    let run = execute("throw 42;").await?;
    assert!(!run.ok);
    assert_eq!(run.stderr, "Uncaught 42\n");
    Ok(())
}

/// With exported functions, the top-level statements initialize the module once: `run` and
/// every export wait for that initialization, whichever entry point the host calls first.
#[tokio::test(flavor = "current_thread")]
async fn commands_and_exports_share_one_initialization() -> Result<()> {
    let world = |export: &str| {
        format!(
            "package test:dual; world dual {{ include wasi:cli/command@0.3.0; export double: {export}(x: s32) -> s32; }}"
        )
    };
    for (export, source, results, output) in [
        (
            "async func",
            r#"
import { setTimeout } from 'node:timers/promises';
let calls = 0;
await setTimeout(1);
console.log("initialized");
export async function double(x: number) {
  calls += 1;
  return x * 2 + calls;
}
"#,
            (3, 4),
            "initialized\n",
        ),
        (
            "async func",
            r#"
const body = new Response(new Uint8Array([3, 4])).body;
let boot = 0;
if (body !== null) {
  for await (const chunk of body) {
    for (let i = 0; i < chunk.length; i++) boot += chunk[i];
  }
}
let calls = 0;
export async function double(x: number) {
  calls += 1;
  return x * 2 + boot + calls;
}
"#,
            (10, 11),
            "",
        ),
        (
            "func",
            "let total = [1, 2, 3].reduce((a, b) => a + b, 34); export function double(x: number) { total += x; return total; }",
            (41, 42),
            "",
        ),
    ] {
        let bytes = Standard.component(&world(export), "dual", "dual.ts", source)?;
        let engine = async_engine()?;
        let component = Component::new(&engine, bytes)?;
        let stdout = MemoryOutput::new(1 << 10);
        let mut builder = WasiCtx::builder();
        builder.stdout(stdout.clone());
        let mut store = wasi_host::store(&engine, Host::new(builder.build()));
        let instance = wasi_host::linker(&engine)?
            .instantiate_async(&mut store, &component)
            .await?;
        let command = Command::new(&mut store, &instance)?;
        let double = instance.get_typed_func::<(i32,), (i32,)>(&mut store, "double")?;
        assert_eq!(
            double.call_async(&mut store, (1,)).await?.0,
            results.0,
            "{export}"
        );
        let result = store
            .run_concurrent(async move |store| command.wasi_cli_run().call_run(store).await)
            .await??;
        assert!(result.is_ok(), "{export}");
        assert_eq!(
            double.call_async(&mut store, (1,)).await?.0,
            results.1,
            "{export}"
        );
        assert_eq!(String::from_utf8(stdout.contents().to_vec())?, output);
    }

    let error = |export: &str, source: &str| {
        let message = format!(
            "{:#}",
            Standard
                .component(&world(export), "dual", "dual.ts", source)
                .unwrap_err()
        );
        assert!(!message.contains("error[internal]:"), "{message}");
        message
    };
    let pending = error(
        "async func",
        "import { readFile } from 'node:fs/promises'; let text = ''; async function load(): Promise<void> { text = await readFile('a.txt', 'utf8'); } load(); export async function double(x: number): Promise<number> { return x + text.length; }",
    );
    assert!(
        pending.contains("top-level statements leave asynchronous work pending"),
        "{pending}"
    );
    for source in [
        "import { setTimeout } from 'node:timers/promises'; await setTimeout(1); export function double(x: number): number { return x * 2; }",
        "const body = new Response(new Uint8Array([1])).body; if (body !== null) { for await (const _ of body) {} } export function double(x: number): number { return x * 2; }",
    ] {
        let synchronous = error("func", source);
        assert!(
            synchronous.contains("double requires asynchronous module initialization"),
            "{synchronous}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn node_fs_reports_node_errors_and_resolves_paths_like_posix() -> Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("fixture.txt"), "é\0abc")?;
    let run = execute_in(
        r#"
import { readFile, writeFile, stat, readdir, mkdir, unlink, rmdir } from 'node:fs/promises';
async function message(pending: Promise<number>) {
  try { await pending; } catch (e) { if (e instanceof Error) console.log(e.message); }
}


async function text(path: string) { await readFile(path, 'utf8'); return 0; }
async function made(path: string) { await mkdir(path); return 0; }
async function removed(path: string) { await rmdir(path); return 0; }
await message(text('missing.txt'));
await message(made('.'));
await message(removed('nope/deeper'));
await message(text('sub/../fixture.txt'));
await mkdir('sub');
console.log(await readFile('./sub/../fixture.txt', { encoding: 'utf8' }) === "é\0abc");
await writeFile('/sub/out.bin', new Uint8Array([104, 195, 169, 255]));
console.log(await readFile('sub/out.bin', 'utf8'), (await readFile('sub/out.bin')).byteLength);
const info = await stat('sub');
console.log(info.isDirectory(), info.isFile(), (await readdir('/sub')).length);
await unlink('sub/out.bin');
await rmdir('sub');
console.log((await readdir('.')).length);
"#,
        Some(root.path()),
    )
    .await?;
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(
        run.stdout,
        "ENOENT: no such file or directory, open 'missing.txt'\n\
         EEXIST: file already exists, mkdir '.'\n\
         ENOENT: no such file or directory, rmdir 'nope/deeper'\n\
         ENOENT: no such file or directory, open 'sub/../fixture.txt'\n\
         true\nhé\u{FFFD} 4\ntrue false 1\n1\n"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_nullable_values_preserve_declared_absence_states() -> Result<()> {
    let run = execute(
        r#"
function numberState(value: number | null | undefined) {
  if (value === null) return 'null';
  if (value === undefined) return 'undefined';
  const required: number = value;
  return 'number:' + required;
}
function textState(value: string | null | undefined) {
  if (value === null) return 'null';
  if (value === undefined) return 'undefined';
  return 'string:' + value;
}
const number: number | null = null;
const text: string | null = null;
const flag: boolean | null | undefined = null;
console.log(numberState(number), numberState(undefined), numberState(4));
console.log(textState(text), textState(undefined), textState('yes'));
console.log(number, text, flag, typeof number, typeof text, typeof flag);
console.log(number === undefined, text === undefined, flag === undefined);
const values: (string | null)[] = ['yes', null];
console.log(textState(values[0]), textState(values[1]), textState(values[2]));
const data = JSON.parse('{"nil":null,"count":4,"text":"yes","flag":false}');
const a: number | null | undefined = data.nil;
const b: number | null | undefined = data.missing;
const c: number | null | undefined = data.count;
console.log(numberState(a), numberState(b), numberState(c));
const absent: string | undefined = undefined;
console.log(JSON.stringify({ number, text, flag, absent }));
try { const required: string = absent; console.log(required); } catch (e) { console.log('undefined required'); }
try { const required: number = number; console.log(required); } catch (e) { console.log('native required'); }
try { const required: number = data.nil; console.log(required); } catch (e) { console.log('null required'); }
try { const required: number = data.missing; console.log(required); } catch (e) { console.log('missing required'); }
try { const required: number = data.flag; console.log(required); } catch (e) { console.log('boolean required'); }
"#,
    ).await?;
    assert!(run.ok, "{}\nstdout:\n{}", run.stderr, run.stdout);
    assert_eq!(
        run.stdout,
        "null undefined number:4\nnull undefined string:yes\nnull null null object object object\nfalse false false\nstring:yes null undefined\nnull undefined number:4\n{\"number\":null,\"text\":null,\"flag\":null}\nundefined required\nnative required\nnull required\nmissing required\nboolean required\n"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn json_absence_is_distinct_from_null() -> Result<()> {
    let run = execute(
        r#"
const value = JSON.parse('{"a":null,"zero":0,"no":false,"text":""}');
const missing = value.missing;
console.log(value.a === null, null === value.a, value.a === undefined);
console.log(missing === undefined, undefined === missing, missing === null);
console.log(value.a !== null, value.a !== undefined, missing !== null);
console.log(value.zero === null, value.no === null, value.text === null);
console.log(typeof value.a, typeof missing);
const list = JSON.parse('[null]');
console.log(list[0] === null, list[1] === undefined, list[1] === null);
try { const text: string = missing; console.log(text); } catch (e) { console.log('required'); }
"#,
    )
    .await?;
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(
        run.stdout,
        "true true false\ntrue true false\nfalse true true\nfalse false false\nobject undefined\ntrue true false\nrequired\n"
    );
    Ok(())
}

/// Work still pending when a task ends is cancelled and fails the task: a command reports it
/// directly, an async export as an uncaught `Error`. Detached imports, fetches and stream reads
/// are covered by `import_test`, `examples_test` and `stream_test`.
#[tokio::test(flavor = "current_thread")]
async fn detached_work_is_cancelled_at_the_task_boundary() -> Result<()> {
    const DETACHED: &str =
        "Unresolved detached work at task boundary; pending operations cancelled";
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("fixture.txt"), vec![b'x'; 1024 * 1024])?;
    let run = execute_in(
        r#"
import { readFile, readdir, stat } from 'node:fs/promises';
import { setTimeout } from 'node:timers/promises';
readFile('fixture.txt');
readdir('.');
stat('fixture.txt');
setTimeout(60_000);
"#,
        Some(root.path()),
    )
    .await?;
    assert!(!run.ok);
    assert_eq!(run.stderr, format!("{DETACHED}\n"));

    let bytes = Standard.component(
        "package test:detached; world boundary {import wasi:clocks/monotonic-clock@0.3.0;import wasi:cli/stderr@0.3.0;export run:async func()->string;}",
        "boundary",
        "detached.ts",
        "import {setTimeout} from 'node:timers/promises'; export async function run():Promise<string>{setTimeout(60000);return await Promise.resolve('ready');}",
    )?;
    let stderr = MemoryOutput::new(8192);
    let (mut store, instance) = wasi_host::instantiate(
        &async_engine()?,
        &bytes,
        Host::new(WasiCtx::builder().stderr(stderr.clone()).build()),
    )
    .await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run.call_async(&mut store, ()),
    )
    .await?;
    assert!(result.is_err());
    assert_eq!(
        String::from_utf8(stderr.contents().to_vec())?,
        format!("Uncaught Error: {DETACHED}\n")
    );
    assert!(store.data().table.is_empty());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn writer_close_waits_for_queued_writes_and_propagates_failure() -> Result<()> {
    let run = execute(
        r#"
import { Writable } from 'node:stream';
async function observed(pending: Promise<void>, trace: number[]) { await pending; trace.push(1); }
const writer = Writable.toWeb(process.stdout).getWriter();
const trace: number[] = [];
const first = writer.write(new Uint8Array(65536));
const observer = observed(first, trace);
await writer.close();
trace.push(2);
await observer;
console.error(JSON.stringify(trace));
"#,
    )
    .await?;
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(run.stdout.len(), 65536);
    assert_eq!(run.stderr, "[1,2]\n");

    let run = execute_output(
        r#"
import { Writable } from 'node:stream';
const writer = Writable.toWeb(process.stdout).getWriter();
const first = writer.write(new Uint8Array(65536));
const second = writer.write(new Uint8Array(65536));
const closed = writer.close();
let failed = 0;
try { await first; } catch { }
try { await second; } catch { failed++; }
try { await closed; } catch { failed++; }
console.error(failed);
"#,
        None,
        3,
    )
    .await?;
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(run.stdout.len(), 3);
    assert_eq!(run.stderr, "2\n");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn inference_engine_specializes_generics_classes_cells_and_fixed_points() -> Result<()> {
    use std::fmt::Write;
    let mut chain = String::from("function f0(x) { return x + 1; }\n");
    for i in 1..=300 {
        writeln!(chain, "function f{i}(x) {{ return f{}(x) + 1; }}", i - 1)?;
    }
    chain.push_str("console.log(f300(0));\n");

    let cases: &[(&str, &str, &str)] = &[
        (
            "static_generic_and_implicit_classes",
            r#"
class Util { static wrap(x) { return [x]; } }
class Box {
  constructor(v) { this.v = v; }
  get value() { return this.v; }
  set value(x) { this.v = x; }
  map(f) { return f(this.v); }
}
class Pair {
  constructor(a, b) { this.a = a; this.b = b; }
  swap() { return new Pair(this.b, this.a); }
}
const bn = new Box(1);
const bs = new Box("a");
bn.value = 5;
const swapped = new Pair(1, "x").swap();
console.log(Util.wrap(2)[0], Util.wrap("a")[0] + "!", bn instanceof Box, bs instanceof Box, bn.value, bs.map(s => s + "!"), swapped.a, swapped.b);
"#,
            "2 a! true true 5 a! x 1\n",
        ),
        (
            "capture_owners_and_mutual_recursion",
            r#"
function addAll(k, arr) { return arr.map(x => x + k); }
function isEven(n) { return n === 0 ? true : isOdd(n - 1); }
function isOdd(n) { return n === 0 ? false : isEven(n - 1); }
console.log(addAll(1, [1, 2]).join(","), addAll("s", ["a", "b"]).join(","), isEven(4), isOdd(3));
"#,
            "2,3 as,bs true true\n",
        ),
        (
            "empty_array_cells_and_reassignment",
            r#"
function fill(list, v) { list.push(v); return list.length; }
function count(list) { return list.length; }
function makeList() { return []; }
class Bag {
  constructor() { this.items = []; }
  add(x) { this.items.push(x); }
}
const c1 = [];
const before = count(c1);
fill(c1, 7);
const c2 = [];
fill(c2, "s");
const f1 = makeList(); f1.push(1);
const f2 = makeList(); f2.push(2);
const state = { items: [] }; state.items.push("x");
const inner = []; inner.push(4);
const outer = []; outer.push(inner);
const bagN = new Bag(); bagN.add(2);
const bagS = new Bag(); bagS.add("k");
const closed = [];
const pushClosed = (v) => { closed.push(v); };
pushClosed(10); pushClosed(20);
let xs = [];
function grow() { xs.push(1); }
function reset() { xs = [2, 3]; }
grow(); reset();
console.log(before, c1[0], c2[0], f1[0] + f2[0], state.items[0], outer[0][0], bagN.items[0], bagS.items[0], closed.length, xs.length, xs[0]);
"#,
            "0 7 s 3 x 4 2 k 2 2 2\n",
        ),
        (
            "write_order_and_late_dependencies",
            r#"
class LateField {
  constructor() { this.items = []; }
  size() { return this.items.length; }
  first() { return this.items[0]; }
  add(v) { this.items.push(v); }
}
const lf = new LateField();
const beforeSize = lf.size();
lf.add("a");
let captured;
const readCap = () => captured;
captured = 2;
let a = null;
function setA1() { a = "x"; }
function setA2() { a = 2; }
setA1(); setA2();
let b = null;
function setB2() { b = 2; }
function setB1() { b = "x"; }
setB1(); setB2();
let c = null;
function late() { return helper(); }
function helper() { c = 3; return 0; }
c = "a";
late();
let mixed;
mixed = 1;
mixed = "a";
mixed = [1];
const ordered = { b: 1, a: "x" };
console.log(beforeSize, lf.size(), lf.first().toUpperCase(), readCap(), typeof a, a, typeof b, b, typeof c, typeof mixed, JSON.stringify(mixed), JSON.stringify(ordered));
"#,
            "0 1 A 2 number 2 number 2 number object [1] {\"b\":1,\"a\":\"x\"}\n",
        ),
        (
            "annotated_fields_keep_declared_types",
            r#"
class Mode { mode: "a" | "b" = "a"; constructor(m: "a" | "b") { this.mode = m; } }
class Late { n = null; set(v) { this.n = v; } }
const late = new Late();
late.set(3);
console.log(new Mode("b").mode, late.n);
"#,
            "b 3\n",
        ),
        ("deep_call_chain_300", chain.as_str(), "301\n"),
        (
            "function_identities_in_storage_and_object_methods",
            r#"
function apply(f, x) { return f(x); }
const twice = (n) => n * 2;
const holder = { f: twice };
const counter = { n: 0, bump(by) { this.n += by; return this.n; } };
const greeter = { greet(name) { return "hi " + name; } };
function getFn() { return twice; }
function selfRef() { return selfRef; }
class FnBox { constructor() { this.op = twice; } run(x) { return this.op(x); } }
const fs = [(x) => x + 1];
counter.bump(2);
console.log(
  apply(twice, 4),
  apply(holder.f, 5),
  holder.f(6),
  counter.bump(3),
  greeter.greet("a"),
  getFn()(7),
  selfRef()()() === selfRef,
  new FnBox().run(8),
  fs[0](1),
);
"#,
            "8 10 12 5 hi a 14 true 16 2\n",
        ),
        (
            "tagged_function_unions_dispatch_and_callbacks",
            r#"
const events = [];
const handlers = [() => events.push("a"), () => events.push("b")];
handlers.forEach((h) => h());
const inc = (x) => x + 1;
const dbl = (x) => x * 2;
const pick = (flag) => (flag ? inc : dbl);
function applyAll(f, xs) { return xs.map(f); }
const f0 = pick(true);
const f1 = pick(false);
let chosen = inc;
chosen = dbl;
const dynamicHandlers = [];
let added = false;
function addMore() {
  if (!added) {
    added = true;
    dynamicHandlers.push(() => events.push("c"));
  }
}
dynamicHandlers.push(addMore);
for (let i = 0; i < dynamicHandlers.length; i++) {
  dynamicHandlers[i]();
}
class FnHolder {
  constructor(f) { this.f = f; }
  run(x) { return this.f(x); }
}
const h1 = new FnHolder(f0);
const h2 = new FnHolder(f1);
console.log(
  events.join(","),
  f0(3),
  f1(3),
  chosen(4),
  applyAll(f0, [1, 2]).join(","),
  applyAll(f1, [3]).join(","),
  h1.run(5),
  h2.run(5),
);
"#,
            "a,b,c 4 6 8 2,3 6 6 10\n",
        ),
    ];
    for &(name, source, expected) in cases {
        let run = execute(source).await?;
        assert!(run.ok, "case {name} failed: {}", run.stderr);
        assert_eq!(run.stdout, expected, "case {name} output mismatch");
    }
    Ok(())
}
