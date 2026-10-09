//! Commands: top-level statements (with top-level `await`) implement `wasi:cli/run`. Console
//! output drains through WASI P3 streams, `process.exitCode`/`process.exit` and uncaught
//! exceptions decide the result, and exceptions are values `try`/`catch` can inspect.
use anyhow::Result;
use perry_wit::{compile_for_world, component::wit::resolve_wit};
use wasmtime::{
    Config, Engine, Store,
    component::{Component, Linker, ResourceTable},
};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView, p3::bindings::Command};

#[path = "support/output_capture.rs"]
mod output_capture;
use output_capture::MemoryOutput;

const WIT: &str = "package test:command; world command { include wasi:cli/command@0.3.0; }";

struct Host {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

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
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("world.wit"), WIT)?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("command"))?;
    let compiled = compile_for_world(source, "main.ts", &resolve, world, true)?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let (stdout, stderr) = (MemoryOutput::new(1 << 20), MemoryOutput::new(1 << 20));
    let mut builder = WasiCtx::builder();
    builder
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .env("GREETING", "grüezi");
    if let Some(root) = root {
        builder.preopened_dir(root, "/", FsPerms::ReadWrite)?;
    }
    let ctx = builder.build();
    let mut store = Store::new(
        &engine,
        Host {
            ctx,
            table: ResourceTable::new(),
        },
    );
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
async fn console_environment_timers_and_exceptions() -> Result<()> {
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
const greeting = process.env.GREETING;
if (greeting === "grüezi") console.log("env " + greeting);
if (process.env.MISSING === undefined) console.log("missing");
const keys: string[] = Object.keys(process.env);
console.log("keys " + keys.length);
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
if (performance.now() > 0 && Date.now() > 1.7e12) console.log("clocks");
"#,
    )
    .await?;
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(
        run.stdout,
        "hello 1.5 true -3\nenv grüezi\nmissing\nkeys 1\nstep 0\nstep 1\nstep 2\n\
         caught negative -1\nfinally 6\nclocks\n"
    );
    assert_eq!(run.stderr, "to stderr\n");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn exit_codes_and_uncaught_exceptions_fail_the_command() -> Result<()> {
    let run =
        execute(r#"console.log("before"); process.exitCode = 1; console.log("after");"#).await?;
    assert!(!run.ok);
    assert_eq!(run.stdout, "before\nafter\n");

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

/// `Headers`, `Response` and `Request` against output recorded from Node 22.
#[tokio::test(flavor = "current_thread")]
async fn fetch_values_match_node() -> Result<()> {
    let run = execute(
        r#"
const h = new Headers({ 'X-A': ' one ' });
h.append('x-a', 'twé');
console.log(h.get('X-A'), h.has('x-b'), h.get('x-b') === null);
try { h.append('bad name', 'v'); } catch (e) { if (e instanceof TypeError) console.log(e.message); }
const r = new Response('hé', { status: 404, statusText: 'Nope', headers: h });
console.log(r.status, r.ok, r.statusText, r.headers.get('content-type'), r.headers.get('x-a'));
console.log(await r.text(), r.bodyUsed);
try { await r.bytes(); } catch (e) { if (e instanceof TypeError) console.log(e.message); }
try { new Response(null, { status: 99 }); } catch (e) { if (e instanceof RangeError) console.log(e.message); }
const q = new Request('HTTPS://Example.TEST:443/a b?x#frag', { method: 'post', body: 'x' });
console.log(q.method, q.url, q.headers.get('content-type'));
const g = new Request('https://example.test', { method: 'patch' });
console.log(g.method, g.url, (await g.bytes()).length, g.bodyUsed);
try { new Request('https://example.test', { method: 'GET', body: 'x' }); } catch (e) { if (e instanceof TypeError) console.log(e.message); }
try { new Request('nope'); } catch (e) { if (e instanceof TypeError) console.log(e.message); }
"#,
    )
    .await?;
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(
        run.stdout,
        r#"one, twé false true
Headers.append: "bad name" is an invalid header name.
404 false Nope text/plain;charset=UTF-8 one, twé
hé true
Body is unusable: Body has already been read
init["status"] must be in the range of 200 to 599, inclusive.
POST https://example.test/a%20b?x#frag text/plain;charset=UTF-8
patch https://example.test/ 0 false
Request with GET/HEAD method cannot have body.
Failed to parse URL from nope
"#
    );
    Ok(())
}
