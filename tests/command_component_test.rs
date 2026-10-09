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
}
impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
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
