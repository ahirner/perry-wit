//! P2 output must instantiate with native Component Model async disabled.
use crate::{
    wasi_host::{self, Host},
    world::Compiler,
};
use anyhow::Result;
use wasmtime::{
    Config, Engine, Store,
    component::{Component, Instance, Linker},
};
use wasmtime_wasi::{WasiCtx, p2::pipe::MemoryOutputPipe};

const WIT: &str = r#"
package test:p2;
world test {
  include wasi:cli/imports@0.2.12;
  import wasi:http/outgoing-handler@0.2.12;
  export run: func(url: string) -> string;
}
"#;

#[test]
fn the_selected_world_controls_the_target_for_identical_source() -> Result<()> {
    let wit = include_str!("../../examples/dual_preview/world.wit");
    let source = include_str!("../../examples/dual_preview/index.ts");
    for world in ["p2", "p3"] {
        let bytes = Compiler::Standard.component(wit, world, "dual.ts", source)?;
        let mut features = wasmparser::WasmFeatures::default();
        features.set(wasmparser::WasmFeatures::CM_ASYNC, world == "p3");
        wasmparser::Validator::new_with_features(features).validate_all(&bytes)?;
    }
    Ok(())
}

#[test]
fn p2_reports_services_that_require_p3() {
    for (body, service) in [
        ("await fetch(url);", "outgoing fetch"),
        ("await readFile(url, 'utf8');", "node:fs"),
    ] {
        let source = format!(
            "import {{ readFile }} from 'node:fs/promises';
export async function run(url: string) {{ {body} return url; }}"
        );
        let error = Compiler::Standard
            .component(WIT, "test", "services.ts", &source)
            .unwrap_err();
        let error = error
            .downcast_ref::<perry_wit::CompileError>()
            .unwrap()
            .primary();
        assert_eq!(error.code(), perry_wit::codes::UNSUPPORTED_CONSTRUCT);
        assert!(
            error
                .message()
                .contains(&format!("{service} requires a WASI 0.3 world")),
            "{error}"
        );
    }
}

async fn instance(wit: &str, source: &str, host: Host) -> Result<(Store<Host>, Instance)> {
    let bytes = Compiler::MemoryChecked.component(wit, "test", "p2.ts", source)?;
    instantiate(bytes, host).await
}

async fn instantiate(bytes: Vec<u8>, host: Host) -> Result<(Store<Host>, Instance)> {
    let mut features = wasmparser::WasmFeatures::default();
    features.remove(wasmparser::WasmFeatures::CM_ASYNC);
    wasmparser::Validator::new_with_features(features).validate_all(&bytes)?;
    let mut config = Config::new();
    config.wasm_component_model_async(false);
    config.consume_fuel(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, &bytes)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    let mut store = wasi_host::store(&engine, host);
    store.set_fuel(100_000_000)?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    Ok((store, instance))
}

#[tokio::test(flavor = "current_thread")]
async fn direct_exports_can_share_a_module_with_console_exports() -> Result<()> {
    let wit = WIT.replace(
        "export run:",
        "export add: func(a:u32,b:u32)->u32; export run:",
    );
    let source = "export function add(a:number,b:number){return a+b;} export function run(text:string){console.log(text);return text;}";
    let bytes = Compiler::Standard.component(&wit, "test", "mixed.ts", source)?;
    let stdout = MemoryOutputPipe::new(4096);
    let host = Host::new(WasiCtx::builder().stdout(stdout.clone()).build());
    let (mut store, instance) = instantiate(bytes, host).await?;
    let add = instance.get_typed_func::<(u32, u32), (u32,)>(&mut store, "add")?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    assert_eq!(add.call_async(&mut store, (20, 22)).await?.0, 42);
    assert_eq!(run.call_async(&mut store, ("mixed",)).await?.0, "mixed");
    assert_eq!(add.call_async(&mut store, (1, 2)).await?.0, 3);
    assert_eq!(&stdout.contents()[..], b"mixed\n");
    assert!(store.data().table.is_empty());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_source_uses_sync_exports_and_preserves_return_storage() -> Result<()> {
    let source = r#"
import { setTimeout } from 'node:timers/promises';
async function later(text: string) { await setTimeout(1); return text + '!'; }
export async function run(text: string) {
  const values = await Promise.all([later(text), later('other')]);
  return values[0] + '|' + values[1];
}
"#;
    let (mut store, instance) = instance(WIT, source, Host::default()).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    for _ in 0..12 {
        let (value,) = run.call_async(&mut store, ("héllo",)).await?;
        assert_eq!(value, "héllo!|other!");
        assert!(
            store.data().table.is_empty(),
            "P2 resources leaked between calls"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn declared_error_results_allow_the_next_call() -> Result<()> {
    let wit = WIT.replace("-> string", "-> result<string, string>");
    let source = r#"
import {setTimeout} from 'node:timers/promises';
export async function run(text:string) {
  await setTimeout(1);
  if(text === 'bad') throw new Error('expected failure');
  return text;
}
"#;
    let (mut store, instance) = instance(&wit, source, Host::default()).await?;
    let run = instance.get_typed_func::<(&str,), (Result<String, String>,)>(&mut store, "run")?;
    assert!(
        run.call_async(&mut store, ("bad",))
            .await?
            .0
            .unwrap_err()
            .contains("expected failure")
    );
    assert_eq!(
        run.call_async(&mut store, ("ok",)).await?.0,
        Ok("ok".into())
    );
    assert!(store.data().table.is_empty());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn p2_commands_and_sync_console_exports() -> Result<()> {
    let wit = "package test:p2-command; world test { include wasi:cli/command@0.2.12; }";
    let stderr_only = "package test:p2-command; world test {
        import wasi:cli/stderr@0.2.12; import wasi:io/poll@0.2.12; export wasi:cli/run@0.2.12;
    }";
    for (world, source, out, err) in [
        (wit, "console.log('hello P2');", "hello P2\n", ""),
        (stderr_only, "console.error('error P2');", "", "error P2\n"),
        (stderr_only, "let value = 1;", "", ""),
    ] {
        let stdout = MemoryOutputPipe::new(1024);
        let stderr = MemoryOutputPipe::new(1024);
        let host = Host::new(
            WasiCtx::builder()
                .stdout(stdout.clone())
                .stderr(stderr.clone())
                .build(),
        );
        let (mut store, instance) = instance(world, source, host).await?;
        let command = wasmtime_wasi::p2::bindings::Command::new(&mut store, &instance)?;
        assert!(command.wasi_cli_run().call_run(&mut store).await?.is_ok());
        assert_eq!(&stdout.contents()[..], out.as_bytes());
        assert_eq!(&stderr.contents()[..], err.as_bytes());
        assert!(store.data().table.is_empty());
    }

    let stdout = MemoryOutputPipe::new(1 << 20);
    let host = Host::new(WasiCtx::builder().stdout(stdout.clone()).build());
    let (mut store, instance) = self::instance(
        WIT,
        "export function run(value: string) { console.log(value); return value; }",
        host,
    )
    .await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, ("sync",)).await?.0, "sync");
    assert_eq!(&stdout.contents()[..], b"sync\n");
    assert!(store.data().table.is_empty());
    Ok(())
}

/// Compile a world that mixes WASI 0.2 with native async and instantiate it on a host that
/// provides both WASI 0.2 and WASI 0.3.
async fn mixed(wit: &str, source: &str, host: Host) -> Result<(Store<Host>, Instance)> {
    let bytes = Compiler::MemoryChecked.component(wit, "test", "mixed.ts", source)?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, &bytes)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let mut store = wasi_host::store(&engine, host);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    Ok((store, instance))
}

#[tokio::test(flavor = "current_thread")]
async fn a_p2_import_set_can_carry_an_async_export() -> Result<()> {
    let wit = "package test:mixed; world test { include wasi:cli/imports@0.2.12; export run: async func() -> u32; }";
    let host = Host::new(WasiCtx::builder().build());
    let (mut store, instance) =
        mixed(wit, "export async function run() { return 1; }", host).await?;
    let run = instance.get_typed_func::<(), (u32,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, ()).await?.0, 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn p2_and_p3_interfaces_are_called_from_one_component() -> Result<()> {
    let wit = r#"
package test:mixed;
world test {
  import wasi:random/random@0.2.12;
  import wasi:clocks/monotonic-clock@0.3.0;
  import wasi:cli/stderr@0.3.0;
  export run: async func(text: string) -> string;
  export sync: func() -> bool;
}
"#;
    let source = r#"
import { setTimeout } from 'node:timers/promises';
import { getRandomU64 } from 'wasi:random/random@0.2.12';
export function sync(): boolean { return getRandomU64() !== getRandomU64(); }
export async function run(text: string): Promise<string> {
  const first = getRandomU64();
  await setTimeout(1);
  return first !== getRandomU64() ? text + '!' : text;
}
"#;
    let host = Host::new(WasiCtx::builder().build());
    let (mut store, instance) = mixed(wit, source, host).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, ("mixed",)).await?.0, "mixed!");
    let sync = instance.get_typed_func::<(), (bool,)>(&mut store, "sync")?;
    assert!(sync.call_async(&mut store, ()).await?.0);
    Ok(())
}

const MIXED: &str = "package test:mixed; world test { include wasi:cli/imports@0.2.12; export run: async func(text: string) -> string; }";

#[tokio::test(flavor = "current_thread")]
async fn services_that_never_wait_bind_p2_interfaces_in_a_mixed_world() -> Result<()> {
    let source = r#"
export async function run(text: string): Promise<string> {
  const value = process.env['MIXED'] ?? 'unset';
  return text + value
    + (Date.now() > 1e12 ? 'D' : 'd')
    + (performance.now() >= 0 ? 'P' : 'p')
    + (Math.random() < 1 ? 'R' : 'r')
    + crypto.randomUUID().length.toString();
}
"#;
    let host = Host::new(WasiCtx::builder().env("MIXED", "yes").build());
    let (mut store, instance) = mixed(MIXED, source, host).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, ("p2:",)).await?.0, "p2:yesDPR36");
    Ok(())
}

#[test]
fn waiting_services_name_the_p3_interface_a_mixed_world_lacks() {
    for (source, expected) in [
        (
            "import { setTimeout } from 'node:timers/promises';\nexport async function run(text: string) { await setTimeout(1); return text; }",
            "setTimeout cannot wait on wasi:clocks/monotonic-clock@0.2.12",
        ),
        (
            "export async function run(text: string) { console.log(text); return text; }",
            "console output cannot wait on wasi:cli/stdout@0.2.12",
        ),
    ] {
        let error = Compiler::Standard
            .component(MIXED, "test", "mixed.ts", source)
            .unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
}
