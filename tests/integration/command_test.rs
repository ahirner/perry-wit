//! Commands: top-level statements (with top-level `await`) implement `wasi:cli/run`. Console
//! output drains through WASI P3 streams, `process.exitCode`/`process.exit` and uncaught
//! exceptions decide the result, and exceptions are values `try`/`catch` can inspect.
use anyhow::Result;
use perry_wit::{compile_for_world, component::wit::resolve_wit};
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
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("world.wit"), WIT)?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("command"))?;
    let compiled = compile_for_world(source, "main.ts", &resolve, world, true)?;
    let engine = async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap().stripped)?;
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
async fn console_timers_and_exceptions() -> Result<()> {
    let run = execute(
        r#"
import { setTimeout } from 'node:timers/promises';
async function later(value: number): Promise<number> {
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
function stop(): number { process.exit(0); return 1; }
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

#[tokio::test(flavor = "current_thread")]
async fn node_fs_reports_node_errors_and_resolves_paths_like_posix() -> Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("fixture.txt"), "é\0abc")?;
    let run = execute_in(
        r#"
import { readFile, writeFile, stat, readdir, mkdir, unlink, rmdir } from 'node:fs/promises';
async function message(pending: Promise<number>): Promise<void> {
  try { await pending; } catch (e) { if (e instanceof Error) console.log(e.message); }
}


async function text(path: string): Promise<number> { await readFile(path, 'utf8'); return 0; }
async function made(path: string): Promise<number> { await mkdir(path); return 0; }
async function removed(path: string): Promise<number> { await rmdir(path); return 0; }
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
function numberState(value: number | null | undefined): string {
  if (value === null) return 'null';
  if (value === undefined) return 'undefined';
  const required: number = value;
  return 'number:' + required;
}
function textState(value: string | null | undefined): string {
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

    let (resolve, package) = perry_wit::component::wit::resolve_source(
        "detached.wit",
        "package test:detached; world boundary {import wasi:clocks/monotonic-clock@0.3.0;import wasi:cli/stderr@0.3.0;export run:async func()->string;}",
    )?;
    let world = resolve.select_world(&[package], Some("boundary"))?;
    let compiled = compile_for_world(
        "import {setTimeout} from 'node:timers/promises'; export async function run():Promise<string>{setTimeout(60000);return await Promise.resolve('ready');}",
        "detached.ts",
        &resolve,
        world,
        true,
    )?;
    let stderr = MemoryOutput::new(8192);
    let (mut store, instance) = wasi_host::instantiate(
        &async_engine()?,
        &compiled.component.unwrap().stripped,
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
    let run = execute(r#"
import { Writable } from 'node:stream';
async function observed(pending: Promise<void>, trace: number[]): Promise<void> { await pending; trace.push(1); }
const writer = Writable.toWeb(process.stdout).getWriter();
const trace: number[] = [];
const first = writer.write(new Uint8Array(65536));
const observer = observed(first, trace);
await writer.close();
trace.push(2);
await observer;
console.error(JSON.stringify(trace));
"#).await?;
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
