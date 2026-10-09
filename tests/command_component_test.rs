//! Commands execute module statements and report uncaught exceptions through wasi:cli/run.
use anyhow::Result;
use perry_wit::{compile_for_world, component::wit::resolve_wit};
use wasmtime::{
    Config, Engine, Store,
    component::{Component, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

struct Host {
    ctx: WasiCtx,
    table: ResourceTable,
    peak: usize,
}
impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl wasmtime::ResourceLimiter for Host {
    fn memory_growing(
        &mut self,
        _: usize,
        desired: usize,
        _: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.peak = self.peak.max(desired);
        Ok(true)
    }
    fn table_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn commands_run_top_level_await_and_return_errors_without_trapping() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("world.wit"),
        r#"
package test:commands;
world command { include wasi:cli/command@0.3.0; }
"#,
    )?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("command"))?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    for (source, expected) in [
        ("", Ok(())),
        ("throw new Error('command failed');", Err(())),
        (
            "console.log('drained on rejection'); throw new Error('command failed');",
            Err(()),
        ),
        (
            r#"
import { setTimeout } from 'node:timers/promises';
async function main(): Promise<void> {
  try { await setTimeout(1); throw new TypeError("caught"); }
  catch (e) { if (e.message !== "caught") throw e; }
  finally { await setTimeout(1); }
}
await main();
"#,
            Ok(()),
        ),
        (
            r#"
import { setTimeout } from 'node:timers/promises';
await setTimeout(1);
throw new RangeError("after suspension");
"#,
            Err(()),
        ),
        (
            r#"
import { setTimeout } from 'node:timers/promises';
async function main(): Promise<void> {
  await setTimeout(1);
  throw new Error("detached failure");
}
main();
"#,
            Err(()),
        ),
        (
            r#"
import { setTimeout } from 'node:timers/promises';
async function fail(): Promise<number> { await setTimeout(1); throw new Error("eventually handled"); }
async function main(): Promise<void> {
  const rejected = fail();
  try { await rejected; } catch (e) { if (e.message !== "eventually handled") throw e; }
}
main();
"#,
            Ok(()),
        ),
        (
            r#"
let total = 0;
for (let i = 0; i < 4; i++) { total = total + i; }
if (total !== 6) throw new Error("top-level locals");
"#,
            Ok(()),
        ),
    ] {
        let compiled = compile_for_world(source, "command.ts", &resolve, world, true)?;
        let wasm = compiled.stripped.unwrap();
        wasmparser::Validator::new().validate_all(&wasm)?;
        let component = Component::new(&engine, &wasm)?;
        let mut store = Store::new(
            &engine,
            Host {
                ctx: WasiCtx::builder().build(),
                table: ResourceTable::new(),
                peak: 0,
            },
        );
        let command = wasmtime_wasi::p3::bindings::Command::instantiate_async(
            &mut store, &component, &linker,
        )
        .await?;
        for _ in 0..3 {
            let result = store
                .run_concurrent(async |accessor| command.wasi_cli_run().call_run(accessor).await)
                .await??;
            assert_eq!(result, expected, "{source}");
            store.assert_concurrent_state_empty();
            assert!(
                store.data().table.is_empty(),
                "command leaked host resources"
            );
        }
    }
    let error = compile_for_world(
        "export function runRun(): {ok: boolean} { return {ok: false}; }",
        "explicit-command.ts",
        &resolve,
        world,
        true,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("explicit command implementation runRun"));
    Ok(())
}

#[path = "support/output_capture.rs"]
mod output_capture;

#[derive(Clone)]
struct PartialOutput(output_capture::MemoryOutput);
impl wasmtime_wasi::cli::IsTerminal for PartialOutput {
    fn is_terminal(&self) -> bool {
        false
    }
}
impl wasmtime_wasi::cli::StdoutStream for PartialOutput {
    fn async_stream(&self) -> Box<dyn tokio::io::AsyncWrite + Send + Sync> {
        Box::new(PartialWriter {
            output: self.0.clone(),
            pending: false,
        })
    }
}
struct PartialWriter {
    output: output_capture::MemoryOutput,
    pending: bool,
}
impl tokio::io::AsyncWrite for PartialWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.pending = !self.pending;
        if self.pending {
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        } else {
            std::pin::Pin::new(&mut self.output).poll_write(cx, &data[..data.len().min(17)])
        }
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cli_output_drains_partial_writes_and_lifts_host_values() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("world.wit"),
        "package test:cli; world command { include wasi:cli/command@0.3.0; }",
    )?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("command"))?;
    let text = "é🙂-".repeat(12000);
    let source = format!(
        r#"
import {{ setTimeout }} from 'node:timers/promises';
console.log("{text}");
console.log("second");
console.error("error:🙂");
if (process.env.PERRY_TEST !== "héllo") throw new Error("environment");
if (process.env.PERRY_MISSING_TEST !== undefined) throw new Error("missing environment");
const keys: string[] = Object.keys(process.env);
let found = false;
for (let i = 0; i < keys.length; i++) {{ if (keys[i] === "PERRY_TEST") found = true; }}
if (!found) throw new Error("keys");
const args: string[] = process.argv;
if (args.length !== 2 || args[1] !== "é🙂") throw new Error("arguments");
const before = performance.now();
const wall = Date.now();
await setTimeout(1);
if (performance.now() < before || wall < 1000000000000) throw new Error("clock");
console.log("last");
console.log(process.env.PERRY_MISSING_TEST);
if (process.env.PERRY_EMPTY_TEST !== "") throw new Error("empty environment");
console.log(process.env.PERRY_EMPTY_TEST);
"#
    );
    let compiled = compile_for_world(&source, "cli.ts", &resolve, world, true)?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let stdout = output_capture::MemoryOutput::new(1_000_000);
    let stderr = output_capture::MemoryOutput::new(1_000);
    let mut store = Store::new(
        &engine,
        Host {
            ctx: WasiCtx::builder()
                .stdout(PartialOutput(stdout.clone()))
                .stderr(PartialOutput(stderr.clone()))
                .env("PERRY_TEST", "héllo")
                .env("PERRY_EMPTY_TEST", "")
                .args(&["cli", "é🙂"])
                .build(),
            table: ResourceTable::new(),
            peak: 0,
        },
    );
    store.limiter(|host| host);
    let command =
        wasmtime_wasi::p3::bindings::Command::instantiate_async(&mut store, &component, &linker)
            .await?;
    let mut peak = 0;
    for round in 1..=3 {
        let result = store
            .run_concurrent(async |accessor| command.wasi_cli_run().call_run(accessor).await)
            .await??;
        assert_eq!(result, Ok(()));
        assert_eq!(
            stdout.contents().as_ref(),
            format!("{text}\nsecond\nlast\nundefined\n\n")
                .repeat(round)
                .as_bytes()
        );
        assert_eq!(
            stderr.contents().as_ref(),
            "error:🙂\n".repeat(round).as_bytes()
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
        if round == 1 {
            peak = store.data().peak;
        } else {
            assert_eq!(store.data().peak, peak, "task memory was not reused");
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn output_failure_finishes_the_command_without_leaking_handles() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("world.wit"),
        "package test:cli; world command { include wasi:cli/command@0.3.0; }",
    )?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("command"))?;
    let text = "x".repeat(100_000);
    let source = format!("console.log('{text}'); console.log('queued');");
    let compiled = compile_for_world(&source, "output-error.ts", &resolve, world, true)?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let mut store = Store::new(
        &engine,
        Host {
            ctx: WasiCtx::builder()
                .stdout(output_capture::MemoryOutput::new(0))
                .build(),
            table: ResourceTable::new(),
            peak: 0,
        },
    );
    let command =
        wasmtime_wasi::p3::bindings::Command::instantiate_async(&mut store, &component, &linker)
            .await?;
    for _ in 0..3 {
        let result = store
            .run_concurrent(async |accessor| command.wasi_cli_run().call_run(accessor).await)
            .await??;
        assert_eq!(result, Err(()));
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn stderr_only_exports_keep_overlapping_task_output_separate() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("world.wit"),
        r#"
package test:output;
world output {
  import wasi:cli/stderr@0.3.0;
  export echo: async func(label: string) -> string;
}"#,
    )?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("output"))?;
    let source = r#"
export async function echo(label: string): Promise<string> {
  console.error(label);
  console.error(label + "!");
  return label;
}"#;
    let compiled = compile_for_world(source, "output.ts", &resolve, world, true)?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let stderr = output_capture::MemoryOutput::new(1000);
    let mut store = Store::new(
        &engine,
        Host {
            ctx: WasiCtx::builder()
                .stderr(PartialOutput(stderr.clone()))
                .build(),
            table: ResourceTable::new(),
            peak: 0,
        },
    );
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let echo = instance.get_typed_func::<(&str,), (String,)>(&mut store, "echo")?;
    let (a, b) = store
        .run_concurrent(async |accessor| -> Result<_> {
            let (a, b) = tokio::join!(
                echo.call_concurrent(accessor, ("alpha",)),
                echo.call_concurrent(accessor, ("beta",))
            );
            Ok((a?.0, b?.0))
        })
        .await??;
    assert_eq!((a.as_str(), b.as_str()), ("alpha", "beta"));
    let output = String::from_utf8(stderr.contents().to_vec())?;
    let mut lines: Vec<_> = output.lines().collect();
    lines.sort();
    assert_eq!(lines, ["alpha", "alpha!", "beta", "beta!"]);
    assert!(output.find("alpha\n") < output.find("alpha!\n"));
    assert!(output.find("beta\n") < output.find("beta!\n"));
    store.assert_concurrent_state_empty();
    assert!(store.data().table.is_empty());
    let error = compile_for_world(
        &source.replace("console.error", "console.log"),
        "missing-import.ts",
        &resolve,
        world,
        true,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("WIT world must import wasi:cli/stdout@0.3.0"));
    Ok(())
}
