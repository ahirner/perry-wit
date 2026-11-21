//! P2 output must instantiate with native Component Model async disabled.
use crate::{
    http_fixture::{HttpFixture, Reply},
    wasi_host::{self, Host},
    world::Compiler,
};
use anyhow::Result;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
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

#[tokio::test(flavor = "current_thread")]
async fn filesystem_roundtrip_and_errors() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut wasi = WasiCtx::builder();
    wasi.preopened_dir(root.path(), "/", wasmtime_wasi::FsPerms::ReadWrite)?;
    let source = r#"
import { readFile, writeFile, stat, readdir, mkdir, unlink, rmdir } from 'node:fs/promises';
export async function run(_: string) {
  await mkdir('sub');
  await writeFile('sub/data', 'héllo');
  const text = await readFile('sub/data', 'utf8');
  const size = (await stat('sub/data')).size;
  const directory = (await stat('sub')).isDirectory();
  const count = (await readdir('sub')).length;
  await unlink('sub/data');
  await rmdir('sub');
  let missing = false;
  try { await readFile('missing', 'utf8'); }
  catch (e) { if (e instanceof Error) missing = e.message.startsWith('ENOENT:'); }
  return text + '|' + size + '|' + directory + '|' + count + '|' + missing;
}
"#;
    let (mut store, instance) = instance(WIT, source, Host::new(wasi.build())).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    for _ in 0..3 {
        let (value,) = run.call_async(&mut store, ("",)).await?;
        assert_eq!(value, "héllo|6|true|1|true");
        assert_eq!(std::fs::read_dir(root.path())?.count(), 0);
    }
    Ok(())
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
async fn fetch_fans_out_before_waiting_for_responses() -> Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let (a, p, s) = (active.clone(), peak.clone(), started.clone());
    let server = HttpFixture::new(move |_| {
        p.fetch_max(a.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
        s.fetch_add(1, Ordering::SeqCst);
        let until = Instant::now() + Duration::from_secs(3);
        while s.load(Ordering::SeqCst) < 4 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        a.fetch_sub(1, Ordering::SeqCst);
        Reply::Body(200, "12345678".into())
    });
    let source = r#"
async function get(url: string): Promise<number> { const response = await fetch(url); return (await response.text()).length; }
export async function run(url: string) {
  const values = await Promise.all([get(url + '/0'), get(url + '/1'), get(url + '/2'), get(url + '/3')]);
  return '' + (values[0] + values[1] + values[2] + values[3]);
}
"#;
    let (mut store, instance) = instance(WIT, source, Host::default()).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    let (value,) = run
        .call_async(&mut store, (&format!("http://{}", server.address),))
        .await?;
    assert_eq!(value, "32");
    assert_eq!(peak.load(Ordering::SeqCst), 4);
    assert_eq!(server.requests.lock().unwrap().len(), 4);
    assert!(store.data().table.is_empty());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_resolves_at_headers_and_streams_the_body() -> Result<()> {
    let released = Arc::new(AtomicBool::new(false));
    let gate = released.clone();
    let server = HttpFixture::new(move |request| {
        if request.target == "/release" {
            gate.store(true, Ordering::Release);
            Reply::Body(200, "ok".into())
        } else {
            Reply::GatedResponse(
                201,
                vec![("x-answer".into(), "yes".into())],
                vec![b'x'; 25_000],
                gate.clone(),
            )
        }
    });
    let source = r#"
export async function run(url: string) {
  const first = await fetch(url + '/body');
  const release = await fetch(url + '/release');
  await release.text();
  const text = await first.text();
  return first.status + '|' + first.headers.get('x-answer') + '|' + text.length;
}
"#;
    let (mut store, instance) = instance(WIT, source, Host::default()).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    let (value,) = tokio::time::timeout(
        Duration::from_secs(10),
        run.call_async(&mut store, (&format!("http://{}", server.address),)),
    )
    .await??;
    assert_eq!(value, "201|yes|25000");
    assert!(store.data().table.is_empty());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn post_body_headers_and_truncated_response_errors() -> Result<()> {
    let server = HttpFixture::new(|request| {
        if request.target == "/broken" {
            Reply::TruncatedBody(b"short".to_vec(), 100)
        } else {
            Reply::Body(
                200,
                format!(
                    "{}|{}",
                    request.method,
                    String::from_utf8_lossy(&request.body)
                ),
            )
        }
    });
    let source = r#"
export async function run(url: string) {
  const response = await fetch(url, { method: 'POST', headers: { 'X-Token': ' abc ' }, body: 'héllo' });
  const text = await response.text();
  let failed = false;
  try { const broken = await fetch(url + '/broken'); await broken.text(); }
  catch (e) { if (e instanceof TypeError) failed = true; }
  return text + '|' + failed;
}
"#;
    let (mut store, instance) = instance(WIT, source, Host::default()).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    let (value,) = run
        .call_async(&mut store, (&format!("http://{}", server.address),))
        .await?;
    assert_eq!(value, "POST|héllo|true");
    assert!(
        server.requests.lock().unwrap()[0]
            .headers
            .iter()
            .any(|(k, v)| k == "x-token" && v == "abc")
    );
    assert!(store.data().table.is_empty());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn p2_commands_and_sync_console_exports() -> Result<()> {
    let stdout = MemoryOutputPipe::new(1 << 20);
    let host = Host::new(WasiCtx::builder().stdout(stdout.clone()).build());
    let wit = "package test:p2-command; world test { include wasi:cli/command@0.2.12; }";
    let (mut store, instance) = instance(wit, "console.log('hello P2');", host).await?;
    let interface = instance
        .get_export_index(&mut store, None, "wasi:cli/run@0.2.12")
        .unwrap();
    let export = instance
        .get_export_index(&mut store, Some(&interface), "run")
        .unwrap();
    let run = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, &export)?;
    assert!(run.call_async(&mut store, ()).await?.0.is_ok());
    assert_eq!(&stdout.contents()[..], b"hello P2\n");
    assert!(store.data().table.is_empty());

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

#[tokio::test(flavor = "current_thread")]
async fn large_upload_and_cancelled_body_release_resources() -> Result<()> {
    let server = HttpFixture::new(|request| Reply::Body(200, request.body.len().to_string()));
    let source = r#"
export async function run(url: string): Promise<string> {
  const response = await fetch(url, {method:'POST', body:'abc'.repeat(40000)});
  const length = await response.text();
  const unused = await fetch(url);
  const body = unused.body;
  if (body !== null) await body.cancel();
  return length;
}
"#;
    let (mut store, instance) = instance(WIT, source, Host::default()).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    let url = format!("http://{}", server.address);
    let (value,) =
        tokio::time::timeout(Duration::from_secs(10), run.call_async(&mut store, (&url,)))
            .await??;
    assert_eq!(value, "120000");
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
