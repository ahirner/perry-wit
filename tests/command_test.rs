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

/// Object spreads over JSON values, property order, `json()` and array destructuring,
/// against output recorded from Node 22.
#[tokio::test(flavor = "current_thread")]
async fn json_values_match_node() -> Result<()> {
    let run = execute(
        r#"
const a = JSON.parse('{"b":1,"2":[1],"1":true,"b":3,"x":{"z":null,"10":"t"}}');
const list = JSON.parse('[7,8]');
console.log(JSON.stringify({ ...a, ...list, x: 'y', n: null, ...JSON.parse('"hé"'), ...JSON.parse('5'), u: a.missing, b: false }));
console.log(JSON.stringify(a), JSON.stringify(JSON.parse('{"4294967295":1,"4294967294":2,"01":3,"0":4}')));
const merged = { ...a.x, ...JSON.parse('{"z":2}'), count: 3, ok: true };
const z: number = merged.z;
console.log(JSON.stringify(merged), z, typeof merged.count);
const r = new Response('{"k":[1,2.5e-3]}');
const parsed = await r.json();
console.log(JSON.stringify(parsed), r.bodyUsed);
try { await new Response('{"k":').json(); } catch (e) { if (e instanceof Error) console.log('rejected'); }
const [first, second, third] = await Promise.all([new Response('[1]').json(), new Response('{"v":2}').json()]);
console.log(JSON.stringify({ ...first, ...second }), third === undefined);
const numeric = JSON.parse('{"0":0,"41":1,"82":2,"22":3,"63":4,"3":5,"44":6,"85":7,"25":8,"66":9,"6":10,"47":11,"88":12,"28":13,"69":14,"9":15,"50":16,"91":17,"31":18,"72":19,"12":20,"53":21,"94":22,"34":23,"75":24,"15":25,"56":26,"97":27,"37":28,"78":29,"18":30,"59":31,"100":32,"40":33,"81":34,"21":35,"62":36,"2":37,"43":38,"84":39}');
console.log(JSON.stringify(numeric));
"#,
    )
    .await?;
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(
        run.stdout,
        r#"{"0":"h","1":"é","2":[1],"b":false,"x":"y","n":null}
{"1":true,"2":[1],"b":3,"x":{"10":"t","z":null}} {"0":4,"4294967294":2,"4294967295":1,"01":3}
{"10":"t","z":2,"count":3,"ok":true} 2 number
{"k":[1,0.0025]} true
rejected
{"0":1,"v":2} true
{"0":0,"2":37,"3":5,"6":10,"9":15,"12":20,"15":25,"18":30,"21":35,"22":3,"25":8,"28":13,"31":18,"34":23,"37":28,"40":33,"41":1,"43":38,"44":6,"47":11,"50":16,"53":21,"56":26,"59":31,"62":36,"63":4,"66":9,"69":14,"72":19,"75":24,"78":29,"81":34,"82":2,"84":39,"85":7,"88":12,"91":17,"94":22,"97":27,"100":32}
"#
    );
    Ok(())
}

/// `Error` names and string causes, `Promise.resolve`/`reject` and plain values among
/// combinator inputs, and error spreads, against output recorded from Node 22.
#[tokio::test(flavor = "current_thread")]
async fn errors_and_settled_promises_match_node() -> Result<()> {
    let run = execute(
        r#"
const plain = new Error('plain');
const typed = new TypeError('bad', { cause: 'root' });
const range = new RangeError('far', { cause: 'distance' });
console.log(plain.name, plain.message, plain.cause === undefined, typed.name, typed.cause, range.name, range.cause);
const values = await Promise.all([Promise.resolve(1), 2, 3]);
console.log(values.length, values[0], values[1], values[2]);
const first = await Promise.race([4, 5]);
const settled = await Promise.allSettled([6, Promise.reject(typed)]);
console.log(first, settled[0].status, settled[1].status, settled[1].reason === typed);
try { throw typed; } catch (e) { console.log(e === typed, JSON.stringify({ ...typed, extra: 1 })); }
"#,
    )
    .await?;
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(
        run.stdout,
        r#"Error plain true TypeError root RangeError distance
3 1 2 3
4 fulfilled rejected true
true {"extra":1}
"#
    );
    Ok(())
}
