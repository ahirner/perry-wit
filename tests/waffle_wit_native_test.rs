use anyhow::{Context, Result};
use perry_wit::waffle_backend::{
    WaffleCompileOptions, WaffleCompiled, compile_typescript_for_world,
};
use std::{path::PathBuf, time::Duration};
use wasmtime::component::{Component, ComponentType, Lift, Linker, Lower, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

#[allow(dead_code)]
#[path = "support/http_fixture.rs"]
mod fixture;

struct Host {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    limits: StoreLimits,
}
impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}
impl WasiHttpView for Host {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

fn compile(source: &str, wit: &str) -> Result<WaffleCompiled> {
    let mut resolve = wit_parser::Resolve::default();
    let main = wit_parser::UnresolvedPackageGroup::parse("native.wit", wit)
        .map_err(|(map, error)| anyhow::anyhow!(error.render(&map)))?;
    let mut paths = std::fs::read_dir(PathBuf::from(
        std::env::var("WASI_P3_WIT_PATH").context("Run native WIT tests through nix develop")?,
    ))?
    .map(|entry| Ok(entry?.path()))
    .collect::<Result<Vec<_>>>()?;
    paths.sort();
    let dependencies = paths
        .into_iter()
        .map(wit_parser::UnresolvedPackageGroup::parse_dir)
        .collect::<Result<Vec<_>>>()?;
    let package = resolve.push_groups(main, dependencies)?;
    let world = resolve.select_world(&[package], Some("boundary"))?;
    compile_typescript_for_world(
        source,
        "native.ts",
        &WaffleCompileOptions::default(),
        resolve,
        world,
    )
}

fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_threading(true)
        .wasm_component_model_async_stackful(true);
    Ok(Engine::new(&config)?)
}
fn store(engine: &Engine) -> Store<Host> {
    let mut store = Store::new(
        engine,
        Host {
            wasi: WasiCtx::default(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(524288).build(),
        },
    );
    store.limiter(|host| &mut host.limits);
    store
}

#[derive(Debug, Clone, PartialEq, ComponentType, Lift, Lower)]
#[component(record)]
struct Item {
    label: String,
    count: u32,
}

const ASYNC_WORLD: &str = r#"
package test:async-lookup;
interface lookup {
  record item {label:string,count:u32}
  load: async func(key:string)->result<item,string>;
}
world boundary {
  import lookup;
  use lookup.{item};
  export run: async func(key:string)->result<item,string>;
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn asynchronous_wit_imports_preserve_records_results_and_repeated_calls() -> Result<()> {
    let compiled = compile(
        r#"
      import {load} from 'test:async-lookup/lookup';
      import type {Item} from 'test:async-lookup/lookup';
      export async function run(key:string):Promise<{ok:true,value:Item}|{ok:false,error:string}> {
        const result=await load(key);
        if(result.ok) {return {ok:true,value:{label:result.value.label+'!',count:result.value.count+1}};}
        return {ok:false,error:result.error};
      }
    "#,
        ASYNC_WORLD,
    )?;
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::<Host>::new(&engine);
    linker
        .instance("test:async-lookup/lookup")?
        .func_wrap_concurrent("load", |_, (key,): (String,)| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(1)).await;
                Ok((if key.is_empty() {
                    Err("missing".to_string())
                } else {
                    Ok(Item {
                        label: key,
                        count: 7,
                    })
                },))
            })
        })?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance
        .get_typed_func::<(&str,), (std::result::Result<Item, String>,)>(&mut store, "run")?;
    for _ in 0..100 {
        for key in ["漢🙂", ""] {
            let result =
                tokio::time::timeout(Duration::from_secs(5), run.call_async(&mut store, (key,)))
                    .await??
                    .0;
            assert_eq!(
                result,
                if key.is_empty() {
                    Err("missing".into())
                } else {
                    Ok(Item {
                        label: format!("{key}!"),
                        count: 8,
                    })
                }
            );
            store.assert_concurrent_state_empty();
        }
    }
    Ok(())
}

const HTTP_WORLD: &str = r#"
package test:http-lookup;
world boundary {
  import wasi:http/client@0.3.0;
  export run: async func(authority:string,path:string)->string;
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn resolved_world_http_get_uses_native_component_bindings() -> Result<()> {
    let compiled = compile(
        r#"
      import {get} from 'perry:http';
      export async function run(authority:string,path:string):Promise<string> {
        const response=await get('http',authority,path,{accept:'application/json'},65536);
        if(response.status!==200) {throw 1;}
        const text=new TextDecoder('utf-8',{fatal:true}).decode(response.body);
        JSON.parse(text);
        for(let index=0;index<400;index++) {JSON.parse(text);}
        return text;
      }
    "#,
        HTTP_WORLD,
    )?;
    let server = fixture::HttpFixture::new(|_| {
        fixture::Reply::WithHeaders(
            200,
            vec![("content-type".into(), "application/json".into())],
            "{\"label\":\"漢🙂\"}".into(),
        )
    });
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str, &str), (String,)>(&mut store, "run")?;
    for _ in 0..100 {
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(5),
                run.call_async(&mut store, (&server.address.to_string(), "/item"))
            )
            .await??
            .0,
            "{\"label\":\"漢🙂\"}"
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn resolved_world_platform_io_uses_shared_guest_memory() -> Result<()> {
    let compiled = compile(
        r#"
      import {waitFor} from 'perry:clocks';
      import {readFileSync,writeFileSync,statSync,readdirSync} from 'node:fs';
      export async function run(path:string):Promise<string> {
        const before=performance.now();
        await waitFor(1);
        if(performance.now()<before) {throw 1;}
        if(Date.now()<0) {throw 2;}
        const random=Math.random();
        if(random<0 || random>=1) {throw 3;}
        const bytes=new Uint8Array(16);
        crypto.getRandomValues(bytes);
        const label=process.env.LABEL;
        if(label===undefined) {throw 4;}
        const text=label+process.argv.join(':');
        writeFileSync(path,text);
        console.log(text);
        console.error(text);
        const retained=readFileSync(path,'utf8');
        if(statSync(path).size<1) {throw 5;}
        const entries=readdirSync('/sandbox');
        if(entries.length!==1) {throw 6;}
        return retained;
      }
    "#,
        r#"package test:platform; world boundary {
      include wasi:cli/imports@0.3.0;
      export run: async func(path:string)->string;
    }"#,
    )?;
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let directory = tempfile::tempdir()?;
    let output = wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(65536);
    store.data_mut().wasi = wasmtime_wasi::WasiCtxBuilder::new()
        .env("LABEL", "漢🙂")
        .args(&["one", "two"])
        .stdout(output.clone())
        .stderr(output.clone())
        .preopened_dir(
            directory.path(),
            "/sandbox",
            wasmtime_wasi::FsPerms::ReadWrite,
        )?
        .build();
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    for _ in 0..80 {
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(5),
                run.call_async(&mut store, ("/sandbox/item",))
            )
            .await??
            .0,
            "漢🙂one:two"
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    assert_eq!(output.contents(), "漢🙂one:two\n".repeat(160).as_bytes());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn asynchronous_wit_traps_and_disposal_release_pending_host_calls() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let compiled = compile(
        r#"import {load} from 'test:async-lookup/lookup';
        import type {Item} from 'test:async-lookup/lookup';
        export async function run(key:string):Promise<{ok:true,value:Item}|{ok:false,error:string}> {
          return await load(key);
        }"#,
        ASYNC_WORLD,
    )?;
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    for fail in [false, true] {
        let started = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let mut linker = Linker::<Host>::new(&engine);
        let (host_started, host_dropped) = (started.clone(), dropped.clone());
        linker
            .instance("test:async-lookup/lookup")?
            .func_wrap_concurrent("load", move |_, (_key,): (String,)| {
                let guard = DropFlag(host_dropped.clone());
                host_started.store(true, Ordering::SeqCst);
                Box::pin(async move {
                    let _guard = guard;
                    if fail {
                        wasmtime::bail!("host lookup failed");
                    }
                    std::future::pending::<()>().await;
                    Ok((Err::<Item, String>("unreachable".into()),))
                })
            })?;
        let mut store = store(&engine);
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance
            .get_typed_func::<(&str,), (std::result::Result<Item, String>,)>(&mut store, "run")?;
        let mut pending = Box::pin(run.call_async(&mut store, ("漢🙂",)));
        if fail {
            let error = tokio::time::timeout(Duration::from_secs(5), &mut pending)
                .await?
                .unwrap_err();
            assert!(format!("{error:#}").contains("host lookup failed"));
        } else {
            assert!(
                tokio::time::timeout(Duration::from_millis(10), &mut pending)
                    .await
                    .is_err()
            );
            assert!(!dropped.load(Ordering::SeqCst));
        }
        assert!(started.load(Ordering::SeqCst));
        drop(pending);
        drop(store);
        assert!(dropped.load(Ordering::SeqCst));
    }
    Ok(())
}

#[test]
fn resolved_world_rejects_retained_tasks_and_missing_capabilities() {
    for body in [
        "const pending=load(key); return await pending;",
        "load(key); return {ok:false,error:'unawaited'};",
    ] {
        let source = format!("import {{load}} from 'test:async-lookup/lookup';
          import type {{Item}} from 'test:async-lookup/lookup';
          export async function run(key:string):Promise<{{ok:true,value:Item}}|{{ok:false,error:string}}> {{{body}}}");
        let error = compile(&source, ASYNC_WORLD).unwrap_err();
        assert!(format!("{error:#}").contains("await"), "{error:#}");
    }
    for (body, interface) in [
        ("return Date.now();", "wasi:clocks/system-clock@0.3.0"),
        ("console.log('text');return 1;", "wasi:cli/stdout@0.3.0"),
    ] {
        let source = format!("export function run():number {{{body}}}");
        let error = compile(
            &source,
            "package test:missing; world boundary {export run:func()->f64;}",
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("WIT world must import")
                && format!("{error:#}").contains(interface),
            "{error:#}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn resolved_world_http_domain_failures_release_resources_before_reuse() -> Result<()> {
    let compiled = compile(
        r#"
        import {get} from 'perry:http';
        export async function run(authority:string,path:string):Promise<{ok:true,value:Uint8Array}|{ok:false,error:number}> {
          try {
            const response=await get('http',authority,path,{},8);
            return {ok:true,value:response.body};
          } catch(error) {
            if(typeof error==='number') {return {ok:false,error};}
            throw error;
          }
        }
    "#,
        r#"package test:http-errors; world boundary {
      import wasi:http/client@0.3.0;
      export run:async func(authority:string,path:string)->result<list<u8>,f64>;
    }"#,
    )?;
    let server = fixture::HttpFixture::new(|request| match request.target.as_str() {
        "/overflow" => fixture::Reply::Bytes(200, vec![0; 9]),
        "/truncated" => fixture::Reply::TruncatedBody(vec![0; 3], 5),
        "/disconnect" => fixture::Reply::Disconnect,
        _ => fixture::Reply::Bytes(200, vec![0, 255, 128, 1]),
    });
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance
        .get_typed_func::<(&str, &str), (std::result::Result<Vec<u8>, f64>,)>(&mut store, "run")?;
    for _ in 0..25 {
        for path in ["/overflow", "/truncated", "/disconnect", "/valid"] {
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                run.call_async(&mut store, (&server.address.to_string(), path)),
            )
            .await??
            .0;
            if path == "/valid" {
                assert_eq!(result, Ok(vec![0, 255, 128, 1]));
            } else {
                let error = result.unwrap_err();
                if path == "/overflow" {
                    assert_eq!(error, 8.0);
                } else {
                    assert!((100.0..139.0).contains(&error), "{path}: {error}");
                }
            }
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn resolved_world_http_disposal_releases_suspended_headers_and_body() -> Result<()> {
    let compiled = compile(
        r#"
      import {get} from 'perry:http';
      export async function run(authority:string,path:string):Promise<string> {
        const response=await get('http',authority,path,{},128);
        return new TextDecoder().decode(response.body);
      }
    "#,
        HTTP_WORLD,
    )?;
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    for path in ["/headers", "/body"] {
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let host_started = started.clone();
        let server = fixture::HttpFixture::new(move |request| {
            host_started.notify_one();
            if request.target == "/body" {
                fixture::Reply::StallBody
            } else {
                fixture::Reply::Stall
            }
        });
        let mut store = store(&engine);
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(&str, &str), (String,)>(&mut store, "run")?;
        let authority = server.address.to_string();
        let mut pending = Box::pin(run.call_async(&mut store, (&authority, path)));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                _ = started.notified() => {},
                result = &mut pending => panic!("stalled response completed: {result:?}"),
            }
        })
        .await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut pending)
                .await
                .is_err()
        );
        drop(pending);
        drop(store);
    }
    Ok(())
}
