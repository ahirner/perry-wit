#[path = "support/allocation_probe.rs"]
mod allocation_probe;
#[path = "support/output_capture.rs"]
mod output_capture;
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

#[tokio::test(flavor = "current_thread")]
async fn stored_wit_tasks_overlap_and_retain_outcomes_in_the_resolved_component() -> Result<()> {
    use std::sync::Arc;
    let compiled = compile(
        r#"
      import {load} from 'test:overlap/work';
      async function decorate(key:string):Promise<string> { return (await load(key))+'!'; }
      export async function run(key:string):Promise<string> {
        const first=decorate(key);
        const second=decorate(key+'2');
        const a=await first;
        const b=await second;
        return a+b+(await first);
      }
    "#,
        r#"package test:overlap;
      interface work { load:async func(key:string)->string; }
      world boundary { import work; export run:async func(key:string)->string; }
    "#,
    )?;
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::<Host>::new(&engine);
    let gate = Arc::new(tokio::sync::Barrier::new(2));
    linker.instance("test:overlap/work")?.func_wrap_concurrent(
        "load",
        move |_, (key,): (String,)| {
            let gate = gate.clone();
            Box::pin(async move {
                gate.wait().await;
                Ok((key,))
            })
        },
    )?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    for _ in 0..150 {
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(3),
                run.call_async(&mut store, ("漢🙂",))
            )
            .await??
            .0,
            "漢🙂!漢🙂2!漢🙂!"
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn stored_native_timers_handle_immediate_rejection_and_repeated_awaits() -> Result<()> {
    let compiled = compile(
        r#"
      import {setTimeout} from 'node:timers/promises';
      async function task(fail:boolean):Promise<number> {
        if(fail) throw 7;
        await setTimeout(0);
        return 11;
      }
      export async function run():Promise<number> {
        const a=task(false);
        const b=task(true);
        let error=0;
        try { await b; } catch(e) { error=e; }
        return (await a)+(await a)+error;
      }
    "#,
        r#"package test:timer;
      world boundary { import wasi:clocks/monotonic-clock@0.3.0; export run:async func()->f64; }
    "#,
    )?;
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..150 {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), run.call_async(&mut store, ()))
                .await??
                .0,
            29.0
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
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
        const pending=load(key);
        const result=await pending;
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
async fn stored_http_requests_overlap_and_complete_both_channels() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let compiled = compile(
        r#"
      import {get} from 'perry:http';
      export async function run(authority:string,path:string):Promise<string> {
        const first=get('http',authority,path+'1',{},4096);
        const second=get('http',authority,path+'2',{},4096);
        const responses=await Promise.all([first,second]);
        const retained=await first;
        if(retained!==responses[0])throw 1;
        const decoder=new TextDecoder('utf-8',{fatal:true});
        return decoder.decode(responses[0].body)+decoder.decode(responses[1].body);
      }
    "#,
        HTTP_WORLD,
    )?;
    let received = Arc::new(AtomicUsize::new(0));
    let count = received.clone();
    let server = fixture::HttpFixture::new(move |request| {
        let pair = count.fetch_add(1, Ordering::SeqCst) / 2 + 1;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while count.load(Ordering::SeqCst) < pair * 2 {
            if std::time::Instant::now() >= deadline {
                return fixture::Reply::Disconnect;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        fixture::Reply::Body(200, request.target.clone())
    });
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str, &str), (String,)>(&mut store, "run")?;
    for _ in 0..60 {
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(5),
                run.call_async(&mut store, (&server.address.to_string(), "/item"))
            )
            .await??
            .0,
            "/item1/item2"
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    assert_eq!(received.load(Ordering::SeqCst), 120);
    Ok(())
}

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
    let output = crate::output_capture::MemoryOutput::new(65536);
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
async fn node_filesystem_promises_preserve_retained_outcomes_and_cleanup() -> Result<()> {
    let source = r#"
      import {readFile,writeFile,stat,readdir,unlink} from 'node:fs/promises';
      export async function run(path:string):Promise<string> {
        const a=writeFile(path+'/first','hello');
        const b=writeFile(path+'/second','world');
        await Promise.all([a,b]);
        const first=readFile(path+'/first','utf8');
        const second=readFile(path+'/second','utf8');
        const texts=await Promise.all([first,second]);
        if((await first)!==texts[0])throw 1;
        const info=stat(path+'/first');
        const names=readdir(path);
        if((await info).size!==5||(await names).length!==2)throw 2;
        await Promise.all([unlink(path+'/first'),unlink(path+'/second')]);
        return texts[0]+texts[1];
      }
    "#;
    let compiled = compile(
        source,
        r#"package test:filesystem-promises; world boundary {
      include wasi:cli/imports@0.3.0; export run:async func(path:string)->string;
    }"#,
    )?;
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let directory = tempfile::tempdir()?;
    let mut store = store(&engine);
    store.data_mut().wasi = wasmtime_wasi::WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/data", wasmtime_wasi::FsPerms::ReadWrite)?
        .build();
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    for _ in 0..80 {
        assert_eq!(
            run.call_async(&mut store, ("/data",)).await?.0,
            "helloworld"
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    let script = tempfile::tempdir()?;
    let file = script.path().join("compare.ts");
    std::fs::write(
        &file,
        format!("{source}\nconsole.log(await run(process.argv[2]));"),
    )?;
    let output = std::process::Command::new("node")
        .arg(&file)
        .arg(directory.path())
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout)?.trim(), "helloworld");
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
fn resolved_world_rejects_detached_tasks_and_missing_capabilities() {
    let source = "import {load} from 'test:async-lookup/lookup';
      import type {Item} from 'test:async-lookup/lookup';
      export async function run(key:string):Promise<{ok:true,value:Item}|{ok:false,error:string}> {load(key); return {ok:false,error:'unawaited'};}";
    let error = compile(source, ASYNC_WORLD)
        .map(|_| ())
        .expect_err("detached task diagnostic");
    assert!(format!("{error:#}").contains("await"), "{error:#}");
    for (body, interface) in [
        ("return Date.now();", "wasi:clocks/system-clock@0.3.0"),
        ("console.log('text');return 1;", "wasi:cli/stdout@0.3.0"),
    ] {
        let source = format!("export function run():number {{{body}}}");
        let error = compile(
            &source,
            "package test:missing; world boundary {export run:async func()->f64;}",
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

#[path = "support/wit_source.rs"]
mod wit_source;

#[derive(Debug, Clone, PartialEq, ComponentType, Lift, Lower)]
#[component(record)]
struct FlowEntry {
    key: String,
    title: String,
    active: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
enum FlowFailure {
    #[component(name = "unavailable")]
    Unavailable,
    #[component(name = "invalid")]
    Invalid,
    #[component(name = "too-many")]
    TooMany,
}

#[tokio::test(flavor = "current_thread")]
async fn independent_record_flow_preserves_transformations_and_orders_host_effects() -> Result<()> {
    use std::sync::{Arc, Mutex};
    const SOURCE: &str = include_str!("fixtures/record_flow.ts");
    const WIT: &str = include_str!("fixtures/record-flow/world.wit");
    wit_source::check_sdk_source(WIT, SOURCE)?;
    let compiled = compile(SOURCE, WIT)?;
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let row = |key: &str, title: &str, active| FlowEntry {
        key: key.into(),
        title: title.into(),
        active,
    };
    let cases = [
        (Ok(vec![]), Ok(0)),
        (
            Ok(vec![
                row("a", "first", None),
                row("b", "skipped", Some(false)),
                row("a", "漢🙂", Some(true)),
            ]),
            Ok(1),
        ),
        (Ok(vec![row("a", "漢🙂", Some(true)); 64]), Ok(1)),
        (
            Ok(vec![row("", "invalid", None)]),
            Err(FlowFailure::Invalid),
        ),
        (Ok(vec![row("a", "", None)]), Err(FlowFailure::Invalid)),
        (
            Ok(vec![row("a", "valid", None); 65]),
            Err(FlowFailure::TooMany),
        ),
        (Err(FlowFailure::Unavailable), Err(FlowFailure::Unavailable)),
    ];
    for (input, expected) in cases {
        for reject_save in [false, true] {
            let events = Arc::new(Mutex::new(Vec::<String>::new()));
            let saved = Arc::new(Mutex::new(Vec::<Vec<FlowEntry>>::new()));
            let mut linker = Linker::<Host>::new(&engine);
            let mut storage = linker.instance("test:record-flow/storage")?;
            let trace = events.clone();
            let input = input.clone();
            storage.func_wrap_concurrent("load", move |_, (): ()| {
                let input = input.clone();
                let trace = trace.clone();
                Box::pin(async move {
                    trace.lock().unwrap().push("load-start".into());
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    trace.lock().unwrap().push("load-end".into());
                    Ok((input,))
                })
            })?;
            let (trace, outputs) = (events.clone(), saved.clone());
            storage.func_wrap_concurrent("save", move |_, (entries,): (Vec<FlowEntry>,)| {
                let (trace, outputs) = (trace.clone(), outputs.clone());
                Box::pin(async move {
                    trace.lock().unwrap().push("save-start".into());
                    tokio::task::yield_now().await;
                    let count = entries.len() as u32;
                    outputs.lock().unwrap().push(entries);
                    trace.lock().unwrap().push("save-end".into());
                    Ok((if reject_save {
                        Err(FlowFailure::Unavailable)
                    } else {
                        Ok(count)
                    },))
                })
            })?;
            let mut store = store(&engine);
            let instance = linker.instantiate_async(&mut store, &component).await?;
            let run = instance
                .get_typed_func::<(&str,), (std::result::Result<u32, FlowFailure>,)>(
                    &mut store, "run",
                )?;
            for _ in 0..40 {
                events.lock().unwrap().clear();
                saved.lock().unwrap().clear();
                let outcome = tokio::time::timeout(
                    Duration::from_secs(5),
                    run.call_async(&mut store, ("新:",)),
                )
                .await??
                .0;
                assert_eq!(
                    outcome,
                    if expected.is_ok() && reject_save {
                        Err(FlowFailure::Unavailable)
                    } else {
                        expected
                    }
                );
                if expected.is_ok() {
                    assert_eq!(
                        *events.lock().unwrap(),
                        ["load-start", "load-end", "save-start", "save-end"]
                    );
                    let entries = if expected == Ok(1) {
                        vec![row("a", "新:漢🙂", Some(true))]
                    } else {
                        vec![]
                    };
                    assert_eq!(*saved.lock().unwrap(), [entries]);
                } else {
                    assert_eq!(*events.lock().unwrap(), ["load-start", "load-end"]);
                    assert!(saved.lock().unwrap().is_empty());
                }
                store.assert_concurrent_state_empty();
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn promise_combinators_preserve_results_and_release_native_threads() -> Result<()> {
    for (body, expected) in [
        (
            "const values=await Promise.all([task(1,3),task(2,0)]); return values[0]+values[1];",
            3.0,
        ),
        (
            "const a=task(1,3); const b=task(2,0); const winner=await Promise.race([a,b]); await a; await b; return winner;",
            2.0,
        ),
        (
            "const values=await Promise.all([]); return values.length;",
            0.0,
        ),
        (
            "const values=await Promise.all([1,2,3]); return values[0]+values[2];",
            4.0,
        ),
        (
            "const values=await Promise.allSettled([task(1,0),failed()]); return values.length;",
            2.0,
        ),
        (
            "const a=task(1,3); const b=failed(); let error=0; try { await Promise.all([a,b]); } catch(e) { error=e; } await a; try { await b; } catch(e) {} return error;",
            7.0,
        ),
        (
            "const values=await Promise.allSettled([task(1,0),failed()]); const a=values[0]; const b=values[1]; if(a.status==='fulfilled'&&b.status==='rejected')return a.value+b.reason; throw 99;",
            8.0,
        ),
        (
            "const tasks:Promise<number>[]=[task(1,3),task(2,0)]; const combined=Promise.all(tasks); const values=await combined; const again=await combined; if(values!==again)throw 99; return values[0]+values[1];",
            3.0,
        ),
        (
            "const input:string[]=['ab','c']; const values=await Promise.all(input); return values[0].length+values[1].length;",
            3.0,
        ),
        (
            "const a=task(1,3); const b=text('hello',0); const winner=await Promise.race([a,b]); await a; await b; if(typeof winner==='string')return winner.length; return winner;",
            5.0,
        ),
        (
            "const a=task(1,0); const b=text('hello',3); const winner=await Promise.race([a,b]); await a; await b; if(typeof winner==='string')return winner.length; return winner;",
            1.0,
        ),
        (
            "const values=await Promise.all([task(1,0),text('hi',0)]); return values[0]+values[1].length;",
            3.0,
        ),
    ] {
        let source = format!(
            r#"
          import {{setTimeout}} from 'node:timers/promises';
          async function task(value:number,delay:number):Promise<number> {{ await setTimeout(delay); return value; }}
          async function text(value:string,delay:number):Promise<string> {{ await setTimeout(delay); return value; }}
          async function failed():Promise<number> {{ throw 7; }}
          export async function run():Promise<number> {{ {body} }}
        "#
        );
        let compiled = compile(
            &source,
            r#"package test:combinators;
          world boundary { import wasi:clocks/monotonic-clock@0.3.0; export run:async func()->f64; }
        "#,
        )
        .with_context(|| source.clone())?;
        let engine = engine()?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;
        let mut store = store(&engine);
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
        for _ in 0..40 {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(3), run.call_async(&mut store, ()))
                    .await??
                    .0,
                expected,
                "{body}"
            );
            store.assert_concurrent_state_empty();
        }
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("compare.ts");
        std::fs::write(&path, format!("{source}\nconsole.log(await run());"))?;
        let result = std::process::Command::new("node").arg(&path).output()?;
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            String::from_utf8(result.stdout)?.trim().parse::<f64>()?,
            expected
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn resolved_task_reaction_order_matches_node() -> Result<()> {
    for body in [
        "const a=child(state,'a'); const b=child(state,'b'); const first=observe(a,state,'one'); const second=observe(a,state,'two'); const both=Promise.all([a,b]); state.text=state.text+'parent;'; await both; state.text=state.text+'all;'; await first; await second; return state.text;",
        "const a=child(state,'a'); const adopted=adopt(a,state); const observer=observe(a,state,'observed'); state.text=state.text+'parent;'; await adopted; state.text=state.text+'adopted;'; await observer; return state.text;",
        "const adopted=adopt(rejected(),state); state.text=state.text+'parent;'; try {await adopted;} catch(e) {state.text=state.text+'rejected;';} return state.text;",
        "const race=Promise.race(['a','b']); const observed=observe(race,state,'race'); const empty=Promise.allSettled([]); state.text=state.text+'parent;'; await empty; state.text=state.text+'empty;'; await observed; return state.text;",
    ] {
        let source = format!(
            r#"
        interface Trace {{ text:string }}
        async function child(state:Trace, name:string):Promise<string> {{
          state.text=state.text+name+'-start;';
          await 0;
          state.text=state.text+name+'-end;';
          return name;
        }}
        async function observe(task:Promise<string>,state:Trace,name:string):Promise<void> {{
          await task;
          state.text=state.text+name+';';
        }}
        async function rejected():Promise<string> {{throw 7;}}
        async function adopt(task:Promise<string>,state:Trace):Promise<string> {{
          try {{return task;}} catch(e) {{state.text=state.text+'caught;'; return 'bad';}}
          finally {{state.text=state.text+'finally;';}}
        }}
        export async function run():Promise<string> {{
          const state:Trace={{text:''}};
          {body}
        }}
      "#
        );
        let compiled = compile(
            &source,
            "package test:ordering; world boundary {export run:async func()->string;}",
        )?;
        let directory = tempfile::tempdir()?;
        let file = directory.path().join("compare.ts");
        std::fs::write(&file, format!("{source}\nconsole.log(await run());"))?;
        let output = std::process::Command::new("node").arg(file).output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = String::from_utf8(output.stdout)?;
        let engine = engine()?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = store(&engine);
        let instance = Linker::new(&engine)
            .instantiate_async(&mut store, &component)
            .await?;
        let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
        for _ in 0..40 {
            assert_eq!(
                run.call_async(&mut store, ()).await?.0,
                expected.trim(),
                "{body}"
            );
            store.assert_concurrent_state_empty();
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn empty_race_stays_pending_and_cannot_escape_its_call() -> Result<()> {
    for awaited in [true, false] {
        let body = if awaited {
            "await Promise.race([]); return 1;"
        } else {
            "const pending=Promise.race([]); return 1;"
        };
        let compiled = compile(
            &format!("export async function run():Promise<number>{{{body}}}"),
            "package test:empty-race; world boundary {export run:async func()->f64;}",
        )?;
        let engine = engine()?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = store(&engine);
        let instance = Linker::new(&engine)
            .instantiate_async(&mut store, &component)
            .await?;
        let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
        let result =
            tokio::time::timeout(Duration::from_millis(20), run.call_async(&mut store, ())).await;
        if awaited {
            match result {
                Err(_) => {}
                Ok(Err(error)) => assert!(format!("{error:#}").contains("deadlock"), "{error:#}"),
                Ok(Ok(value)) => panic!("an empty race must not settle: {value:?}"),
            }
        } else {
            let error = result?.expect_err("unresolved work must fail the owning boundary");
            assert!(format!("{error:#}").contains("unreachable"), "{error:#}");
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn directly_awaited_imports_serialize_public_calls() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let compiled = compile(
        "import {wait} from 'test:ownership/work'; export async function run():Promise<number>{await wait();return 1;}",
        "package test:ownership; interface work {wait:async func();} world boundary {import work; export run:async func()->f64;}",
    )?;
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let started = Arc::new(AtomicUsize::new(0));
    let calls = started.clone();
    let gate = Arc::new(tokio::sync::Notify::new());
    let host_gate = gate.clone();
    let mut linker = Linker::new(&engine);
    linker
        .instance("test:ownership/work")?
        .func_wrap_concurrent("wait", move |_, (): ()| {
            calls.fetch_add(1, Ordering::SeqCst);
            let gate = host_gate.clone();
            Box::pin(async move {
                gate.notified().await;
                Ok(())
            })
        })?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    tokio::time::timeout(
        Duration::from_secs(2),
        store.run_concurrent(async |accessor| {
            let mut first = Box::pin(run.call_concurrent(accessor, ()));
            assert!(
                tokio::time::timeout(Duration::from_millis(10), &mut first)
                    .await
                    .is_err()
            );
            let mut second = Box::pin(run.call_concurrent(accessor, ()));
            assert!(
                tokio::time::timeout(Duration::from_millis(10), &mut second)
                    .await
                    .is_err()
            );
            assert_eq!(started.load(Ordering::SeqCst), 1);
            gate.notify_one();
            assert_eq!(first.await?.0, 1.0);
            gate.notify_one();
            assert_eq!(second.await?.0, 1.0);
            Ok::<(), wasmtime::Error>(())
        }),
    )
    .await???;
    assert_eq!(started.load(Ordering::SeqCst), 2);
    store.assert_concurrent_state_empty();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn timer_delays_map_to_p3_nanoseconds_without_waiting_for_the_clock() -> Result<()> {
    use std::sync::{Arc, Mutex};
    let source = r#"
      import {setTimeout} from 'node:timers/promises';
      async function shadow(undefined:number):Promise<void> {await setTimeout(undefined);}
      export async function run(delay:number):Promise<void> {
        await setTimeout();
        await setTimeout(undefined);
        await shadow(delay);
      }
    "#;
    let wit = r#"package test:timer-units; world boundary {
      import wasi:clocks/monotonic-clock@0.3.0;
      export run:async func(delay:f64);
    }"#;
    wit_source::check_sdk_source(wit, source)?;
    let compiled = compile(source, wit)?;
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let durations = Arc::new(Mutex::new(Vec::new()));
    let observed = durations.clone();
    let mut linker = Linker::new(&engine);
    linker
        .instance("wasi:clocks/monotonic-clock@0.3.0")?
        .func_wrap_concurrent("wait-for", move |_, (nanos,): (u64,)| {
            observed.lock().unwrap().push(nanos);
            Box::pin(async { Ok(()) })
        })?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), ()>(&mut store, "run")?;
    for (delay, nanos) in [
        (f64::NAN, 1_000_000),
        (f64::INFINITY, 1_000_000),
        (-1.0, 1_000_000),
        (0.0, 1_000_000),
        (0.9, 1_000_000),
        (1.9, 1_000_000),
        (2.9, 2_000_000),
        (2_147_483_647.0, 2_147_483_647_000_000),
        (2_147_483_648.0, 1_000_000),
    ] {
        durations.lock().unwrap().clear();
        run.call_async(&mut store, (delay,)).await?;
        assert_eq!(*durations.lock().unwrap(), [1_000_000, 1_000_000, nanos]);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn asynchronous_module_initialization_is_shared_by_named_exports() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let compiled = compile(
        r#"
        import {load} from 'test:initialization/host';
        const label:string=await load();
        let calls:number=0;
        export async function first():Promise<string> {calls++; return label;}
        export async function second():Promise<number> {return calls;}
        "#,
        "package test:initialization; interface host {load:async func()->string;} world boundary {import host; export first:async func()->string; export second:async func()->f64;}",
    )?;
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let count = Arc::new(AtomicUsize::new(0));
    let captured = count.clone();
    let mut linker = Linker::<Host>::new(&engine);
    linker
        .instance("test:initialization/host")?
        .func_wrap_concurrent("load", move |_, (): ()| {
            let count = captured.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                Ok(("initialized 漢🙂".to_string(),))
            })
        })?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let first = instance.get_typed_func::<(), (String,)>(&mut store, "first")?;
    let second = instance.get_typed_func::<(), (f64,)>(&mut store, "second")?;
    assert_eq!(second.call_async(&mut store, ()).await?.0, 0.0);
    for index in 1..=100 {
        assert_eq!(
            first.call_async(&mut store, ()).await?.0,
            "initialized 漢🙂"
        );
        assert_eq!(second.call_async(&mut store, ()).await?.0, index as f64);
        store.assert_concurrent_state_empty();
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn suspended_wit_return_storage_survives_collection_in_another_task() -> Result<()> {
    const WIT: &str = "package test:canonical-roots;
      interface work { load:async func()->list<string>; release:func(); }
      world boundary { import work; export run:async func(input:string)->string; }";
    let compiled = compile(
        r#"
      import {load,release} from 'test:canonical-roots/work';
      export async function run(input:string):Promise<string> {
        const result=load();
        for(let i=0;i<2000;i++) {
          const temporary=input.toLowerCase().split('.').join('-');
        }
        release();
        return (await result).join("|");
      }
    "#,
        WIT,
    )?;
    let engine = engine()?;
    let core = allocation_probe::guard_return_area(
        &compiled.core,
        "test:canonical-roots/work",
        "[async-lower]load",
        "release",
    )?;
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("world.wit"), WIT)?;
    let guarded =
        perry_wit::component::embed_and_encode(&core, directory.path(), Some("boundary"))?;
    let component = Component::new(&engine, guarded)?;
    let mut linker = Linker::<Host>::new(&engine);
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let pending = gate.clone();
    linker
        .instance("test:canonical-roots/work")?
        .func_wrap_concurrent("load", move |_, (): ()| {
            let gate = pending.clone();
            Box::pin(async move {
                gate.notified().await;
                Ok((vec!["answer!".to_string(); 16],))
            })
        })?;
    linker
        .instance("test:canonical-roots/work")?
        .func_wrap("release", move |_, (): ()| {
            gate.notify_one();
            Ok(())
        })?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(5),
                run.call_async(&mut store, ("A.B.C",))
            )
            .await??
            .0,
            vec!["answer!"; 16].join("|")
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_lower_uses_indirect_parameters_and_releases_terminal_subtasks() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let compiled = compile(
        r#"
        import {score, checkpoint} from 'test:async-abi/work';
        export async function run(label:string):Promise<number> {
          const first=score(label,1,2,3,4);
          const second=score(label,5,6,7,8);
          await checkpoint();
          return (await first)+(await second)+(await first);
        }
        "#,
        r#"package test:async-abi;
        interface work {
          score:async func(label:string,a:f64,b:f64,c:f64,d:f64)->f64;
          checkpoint:async func();
        }
        world boundary { import work; export run:async func(label:string)->f64; }
        "#,
    )?;
    let module = waffle::Module::from_wasm_bytes(&compiled.core, &Default::default())?;
    let imports: Vec<_> = module
        .imports
        .iter()
        .filter(|import| import.module == "test:async-abi/work")
        .collect();
    assert_eq!(imports.len(), 2);
    for import in imports {
        let waffle::ImportKind::Func(function) = import.kind else {
            panic!("function expected")
        };
        let signature = &module.signatures[module.funcs[function].sig()];
        assert_eq!(signature.returns, [waffle::Type::I32]);
        match import.name.as_str() {
            "[async-lower]score" => assert_eq!(signature.params, [waffle::Type::I32; 2]),
            "[async-lower]checkpoint" => assert!(signature.params.is_empty()),
            name => panic!("unexpected native import {name}"),
        }
    }
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::<Host>::new(&engine);
    let gate = Arc::new(tokio::sync::Barrier::new(3));
    let calls = Arc::new(AtomicUsize::new(0));
    let scores = calls.clone();
    let pending = gate.clone();
    linker
        .instance("test:async-abi/work")?
        .func_wrap_concurrent(
            "score",
            move |_, (label, a, b, c, d): (String, f64, f64, f64, f64)| {
                let gate = pending.clone();
                scores.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    gate.wait().await;
                    Ok((label.chars().count() as f64 + a + b + c + d,))
                })
            },
        )?;
    linker
        .instance("test:async-abi/work")?
        .func_wrap_concurrent("checkpoint", move |_, (): ()| {
            let gate = gate.clone();
            Box::pin(async move {
                gate.wait().await;
                Ok(())
            })
        })?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (f64,)>(&mut store, "run")?;
    for _ in 0..100 {
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(5),
                run.call_async(&mut store, ("漢🙂",))
            )
            .await??
            .0,
            52.0
        );
        store.assert_concurrent_state_empty();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 200);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn export_results_survive_collection_while_ready_work_finishes() -> Result<()> {
    let compiled = compile(
        r#"
        async function cleanup(input:string):Promise<void> {
            await 0;
            for(let i=0;i<2000;i++) {
                const discarded=input.toLowerCase().split('.').join('-');
            }
        }
        export function run(input:string):string {
            const pending = cleanup(input);
            return input + ' retained result';
        }
        "#,
        "package test:export-roots; world boundary {export run:async func(input:string)->string;}",
    )?;
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = store(&engine);
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    for _ in 0..50 {
        assert_eq!(
            run.call_async(&mut store, ("A.B.C",)).await?.0,
            "A.B.C retained result"
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn standard_fetch_consumes_body_once_and_preserves_http_status() -> Result<()> {
    let compiled = compile(
        r#"
      export async function run(url:string):Promise<string> {
        const response = await fetch(url);
        if(response.status !== 404) return "status";
        if(response.ok) return "ok";
        if(response.bodyUsed) return "body-used-before";
        const body = await response.text();
        if(!response.bodyUsed) return "body-unused-after";
        let rejected = false;
        try { await response.bytes(); } catch(error) { rejected = true; }
        if(!rejected) return "second-consumed";
        return body+"|"+response.url;

      }
    "#,
        r#"package test:fetch; world boundary {
      import wasi:http/client@0.3.0;
      export run:async func(url:string)->string;
    }"#,
    )?;
    let server = fixture::HttpFixture::new(|_| {
        fixture::Reply::Bytes(404, b"\xef\xbb\xbfhello\xe2\x82X\xff".to_vec())
    });
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    for _ in 0..30 {
        let url = format!("http://{}/test?q=hello world#ignored", server.address);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), run.call_async(&mut store, (&url,)))
                .await??
                .0,
            format!("hello�X�|http://{}/test?q=hello%20world", server.address)
        );
        store.assert_concurrent_state_empty();
    }
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.method == "GET" && request.target == "/test?q=hello%20world")
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn standard_fetch_resolves_at_headers_and_matches_node() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let source = include_str!("fixtures/fetch/headers_first.ts");
    let compiled = compile(
        source,
        r#"package test:fetch; world boundary {
      import wasi:http/client@0.3.0;
      export run:async func(base:string)->string;
    }"#,
    )?;
    let released = Arc::new(AtomicBool::new(false));
    let gate = released.clone();
    let server = fixture::HttpFixture::new(move |request| match request.target.as_str() {
        "/gated" => fixture::Reply::GatedBody("hello🙂".as_bytes().to_vec(), gate.clone()),
        "/release" => {
            gate.store(true, Ordering::Release);
            fixture::Reply::Body(200, "!".into())
        }
        _ => fixture::Reply::Disconnect,
    });
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    let base = format!("http://{}", server.address);
    for _ in 0..30 {
        released.store(false, Ordering::Release);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), run.call_async(&mut store, (&base,)))
                .await??
                .0,
            "hello🙂!hello🙂"
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    released.store(false, Ordering::Release);
    let module =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fetch/headers_first.ts");
    let script = format!(
        "import {{run}} from {}; console.log(await run(process.argv[1]));",
        serde_json::to_string(&module.to_string_lossy())?
    );
    let output = std::process::Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script, &base])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout)?.trim(), "hello🙂!hello🙂");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn standard_fetch_releases_failures_and_grows_bodies_incrementally() -> Result<()> {
    let source = r#"
      export async function run(url:string):Promise<number> {
        let headers = false;
        try {
          const response = await fetch(url);
          headers = true;
          const data = await response.bytes();
          return data.length;
        } catch(error) { if(headers) return -2; return -1; }
      }
    "#;
    let compiled = compile(
        source,
        r#"package test:fetch; world boundary {
      import wasi:http/client@0.3.0;
      export run:async func(url:string)->f64;
    }"#,
    )?;
    let server = fixture::HttpFixture::new(|request| match request.target.as_str() {
        "/truncated" => fixture::Reply::TruncatedBody(vec![1, 2, 3], 4_000_000_000),
        "/disconnect" => fixture::Reply::Disconnect,
        "/empty" => fixture::Reply::Bytes(200, vec![]),
        _ => fixture::Reply::Bytes(200, vec![0; 32_769]),
    });
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (f64,)>(&mut store, "run")?;
    for _ in 0..40 {
        for (path, expected) in [
            ("/truncated", -2.0),
            ("/disconnect", -1.0),
            ("/empty", 0.0),
            ("/large", 32769.0),
        ] {
            let url = format!("http://{}{path}", server.address);
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(3), run.call_async(&mut store, (&url,)))
                    .await??
                    .0,
                expected,
                "{path}"
            );
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
        assert_eq!(run.call_async(&mut store, ("not-a-url",)).await?.0, -1.0);
    }
    assert_eq!(server.requests.lock().unwrap().len(), 160);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn standard_fetch_rejects_unconsumed_body_at_call_boundary() -> Result<()> {
    let compiled = compile(
        r#"export async function run(url:string):Promise<number> {
        const response = await fetch(url); return response.status;
    }"#,
        r#"package test:fetch; world boundary {
      import wasi:http/client@0.3.0;
      export run:async func(url:string)->f64;
    }"#,
    )?;
    let server = fixture::HttpFixture::new(|_| fixture::Reply::Body(200, "body".into()));
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (f64,)>(&mut store, "run")?;
    let url = format!("http://{}/", server.address);
    assert!(run.call_async(&mut store, (&url,)).await.is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn standard_fetch_runs_as_a_top_level_node_and_p3_command() -> Result<()> {
    let server = fixture::HttpFixture::new(|_| fixture::Reply::Body(200, "from fetch".into()));
    let source = format!(
        r#"
        const response:Response = await fetch("http://{}/");
        console.log(await response.text());
    "#,
        server.address
    );
    let compiled = compile(
        &source,
        r#"package test:fetch-command; world boundary {
        include wasi:cli/imports@0.3.0;
        import wasi:http/client@0.3.0;
        export wasi:cli/run@0.3.0;
    }"#,
    )?;
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let captured = output_capture::MemoryOutput::new(1024);
    store.data_mut().wasi = wasmtime_wasi::WasiCtxBuilder::new()
        .stdout(captured.clone())
        .build();
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let interface = instance
        .get_export_index(&mut store, None, "wasi:cli/run@0.3.0")
        .context("CLI interface")?;
    let export = instance
        .get_export_index(&mut store, Some(&interface), "run")
        .context("CLI run")?;
    let run = instance.get_typed_func::<(), (std::result::Result<(), ()>,)>(&mut store, export)?;
    assert_eq!(run.call_async(&mut store, ()).await?.0, Ok(()));
    assert_eq!(captured.contents().as_ref(), b"from fetch\n");
    store.assert_concurrent_state_empty();
    let directory = tempfile::tempdir()?;
    let file = directory.path().join("main.ts");
    std::fs::write(&file, source)?;
    let output = std::process::Command::new("node").arg(file).output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"from fetch\n");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn standard_fetch_uploads_snapshot_bodies_and_options_and_match_node() -> Result<()> {
    let compiled = compile(
        include_str!("fixtures/fetch/upload.ts"),
        r#"package test:upload; world boundary {
        import wasi:http/client@0.3.0;
        export run: async func(base:string)->string;
    }"#,
    )?;
    let server = fixture::HttpFixture::new(|request| {
        if request.target == "/text" {
            assert_eq!(request.method, "POST");
            assert!(
                request
                    .headers
                    .contains(&("x-request".into(), "original".into()))
            );
            assert_eq!(request.body, "héllo🙂".as_bytes());
            assert!(
                request
                    .headers
                    .contains(&("content-type".into(), "text/plain;charset=UTF-8".into()))
            );
            fixture::Reply::Body(200, "text".into())
        } else {
            assert_eq!(request.method, "PATCH");
            assert!(
                request
                    .headers
                    .contains(&("content-type".into(), "application/octet-stream".into()))
            );
            assert_eq!(request.body.len(), 70001);
            assert_eq!(request.body[0], 65);
            assert_eq!(request.body[70000], 66);
            fixture::Reply::Body(200, "bytes".into())
        }
    });
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    let base = format!("http://{}", server.address);
    for _ in 0..30 {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), run.call_async(&mut store, (&base,)))
                .await??
                .0,
            "text|bytes"
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    let module = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fetch/upload.ts");
    let script = format!(
        "import {{run}} from {}; console.log(await run(process.argv[1]));",
        serde_json::to_string(&module.to_string_lossy())?
    );
    let output = std::process::Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script, &base])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout)?.trim(), "text|bytes");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn standard_fetch_validates_methods_and_finishes_null_bodies() -> Result<()> {
    let compiled = compile(
        r#"
        export async function run(base:string):Promise<number> {
            let rejected = 0;
            try { await fetch(base, {method: "CONNECT"}); } catch(error) { rejected++; }
            try { await fetch(base, {method: "get", body: "bad"}); } catch(error) { rejected++; }
            try { await fetch(base, {method: "HEAD", body: new Uint8Array(0)}); } catch(error) { rejected++; }
            try { await fetch(base, {method: "not a token"}); } catch(error) { rejected++; }
            try { await fetch(base, {headers: {"bad header": "value"}}); } catch(error) { rejected++; }
            try { await fetch(base, {headers: {"x-bad": "embedded\nline"}}); } catch(error) { rejected++; }
            try { await fetch(base, {headers: {"x-bad": "🙂"}}); } catch(error) { rejected++; }
            const head = await fetch(base + "/head", {method: "head"});
            if((await head.bytes()).length !== 0 || (await head.text()) !== "" || head.bodyUsed) return -1;
            const empty = await fetch(base + "/empty");
            if(empty.status !== 204 || (await empty.text()) !== "" || (await empty.bytes()).length !== 0 || empty.bodyUsed) return -2;
            const unread = await fetch(base + "/empty");
            return rejected;
        }
    "#,
        r#"package test:fetch-methods; world boundary {import wasi:http/client@0.3.0; export run:async func(base:string)->f64;}"#,
    )?;
    let server = fixture::HttpFixture::new(|request| match request.target.as_str() {
        "/head" => fixture::Reply::WithHeaders(200, vec![], "not sent".into()),
        "/empty" => fixture::Reply::WithHeaders(204, vec![], "".into()),
        _ => panic!("invalid fetch reached the host"),
    });
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (f64,)>(&mut store, "run")?;
    let base = format!("http://{}", server.address);
    for _ in 0..30 {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), run.call_async(&mut store, (&base,)))
                .await??
                .0,
            7.0
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    assert_eq!(server.requests.lock().unwrap().len(), 90);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn standard_fetch_json_and_array_buffers_preserve_types_identity_and_failures() -> Result<()>
{
    let compiled = compile(
        include_str!("fixtures/fetch/body_methods.ts"),
        r#"package test:body-methods; world boundary {
        import wasi:http/client@0.3.0;
        export run:async func(base:string)->string;
    }"#,
    )?;
    let server = fixture::HttpFixture::new(|request| match request.target.as_str() {
        "/json" => {
            fixture::Reply::Body(200, r#"{"label":"hello🙂","unused":[true,null,42]}"#.into())
        }
        "/bytes" => fixture::Reply::Bytes(200, vec![0, 1, 255]),
        "/invalid" => fixture::Reply::Body(200, "[1,]".into()),
        _ => fixture::Reply::Disconnect,
    });
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    let base = format!("http://{}", server.address);
    for _ in 0..40 {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), run.call_async(&mut store, (&base,)))
                .await??
                .0,
            "hello🙂:2"
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    let module =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fetch/body_methods.ts");
    let script = format!(
        "import {{run}} from {}; console.log(await run(process.argv[1]));",
        serde_json::to_string(&module.to_string_lossy())?
    );
    let output = std::process::Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script, &base])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout)?.trim(), "hello🙂:2");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn standard_fetch_headers_match_node_and_retain_metadata_after_consumption() -> Result<()> {
    let compiled = compile(
        include_str!("fixtures/fetch/headers.ts"),
        r#"package test:fetch-headers; world boundary {
        import wasi:http/client@0.3.0;
        export run:async func(url:string)->string;
    }"#,
    )?;
    let server = fixture::HttpFixture::new(|_| {
        fixture::Reply::WithHeaders(
            200,
            vec![
                ("X-Value".into(), "hello".into()),
                ("X-Bytes".into(), "é".into()),
                ("X-Empty".into(), "".into()),
                ("X-Separate".into(), "left".into()),
                ("x-separate".into(), "right".into()),
            ],
            "body".into(),
        )
    });
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    let url = format!("http://{}/", server.address);
    for _ in 0..40 {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), run.call_async(&mut store, (&url,)))
                .await??
                .0,
            "hello:body"
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    let module = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fetch/headers.ts");
    let script = format!(
        "import {{run}} from {}; console.log(await run(process.argv[1]));",
        serde_json::to_string(&module.to_string_lossy())?
    );
    let output = std::process::Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script, &url])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout)?.trim(), "hello:body");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn decoded_string_literals_preserve_latin1_without_encoding_repair() -> Result<()> {
    let compiled = compile(
        r#"
        export function run():string {
            if("Ã©" !== "\u00c3\u00a9") return "repaired accent";
            if("Â£" !== "\u00c2\u00a3") return "repaired currency";
            return "Ã©|Â£|é|🙂";
        }
    "#,
        "package test:decoded-literals; world boundary { export run:async func()->string; }",
    )?;
    let engine = engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = store(&engine);
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, ()).await?.0, "Ã©|Â£|é|🙂");
    Ok(())
}
