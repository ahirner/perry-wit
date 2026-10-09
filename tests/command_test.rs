//! Commands: top-level statements (with top-level `await`) implement `wasi:cli/run`. Console
//! output drains through WASI P3 streams, `process.exitCode`/`process.exit` and uncaught
//! exceptions decide the result, and exceptions are values `try`/`catch` can inspect.
use anyhow::Result;
use perry_wit::{compile_for_world, component::wit::resolve_wit};
use wasmtime::{
    Config, Engine, Store,
    component::{Component, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView, p3::bindings::Command};

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
    let ctx = WasiCtx::builder()
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .env("GREETING", "grüezi")
        .build();
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
