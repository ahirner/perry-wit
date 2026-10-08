use anyhow::Result;
use perry_wit::waffle_backend::{WaffleCompileOptions, compile_typescript_for_world};
use std::{future::Future, time::Duration};
use wasmtime::component::{Component, ComponentType, Instance, Lift, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

#[path = "support/output_capture.rs"]
mod output_capture;

struct Refused;
impl WasiHttpHooks for Refused {
    fn send_request(
        &mut self,
        _: http::Request<WasiBody>,
        _: Option<wasmtime_wasi_http::RequestOptions>,
        _: Box<dyn Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send>,
    ) -> Box<
        dyn Future<
                Output = Result<
                    (
                        http::Response<WasiBody>,
                        Box<dyn Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send>,
                    ),
                    wasmtime_wasi_http::Error,
                >,
            > + Send,
    > {
        Box::new(async { Err(wasmtime_wasi_http::Error::ConnectionRefused) })
    }
}
struct Host {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    refused: Refused,
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
            hooks: &mut self.refused,
        }
    }
}

async fn instantiate(
    source: &str,
    wit: &str,
) -> Result<(Store<Host>, Instance, output_capture::MemoryOutput)> {
    let mut resolve = wit_parser::Resolve::default();
    let main = wit_parser::UnresolvedPackageGroup::parse("errors.wit", wit)
        .map_err(|(map, error)| anyhow::anyhow!(error.render(&map)))?;
    let mut paths = std::fs::read_dir(std::env::var("WASI_WIT_PATH")?)?
        .map(|entry| Ok(entry?.path()))
        .collect::<Result<Vec<_>>>()?;
    paths.sort();
    let dependencies = paths
        .into_iter()
        .map(wit_parser::UnresolvedPackageGroup::parse_dir)
        .collect::<Result<Vec<_>>>()?;
    let package = resolve.push_groups(main, dependencies)?;
    let world = resolve.select_world(&[package], Some("boundary"))?;
    let compiled = compile_typescript_for_world(
        source,
        "errors.ts",
        &WaffleCompileOptions::default(),
        resolve,
        world,
    )?;
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_threading(true)
        .wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::<Host>::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let output = output_capture::MemoryOutput::new(8192);
    let mut store = Store::new(
        &engine,
        Host {
            wasi: WasiCtx::builder().stderr(output.clone()).build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            refused: Refused,
        },
    );
    let instance = linker.instantiate_async(&mut store, &component).await?;
    Ok((store, instance, output))
}

async fn invoke(source: &str, asynchronous: bool) -> Result<(wasmtime::Result<(String,)>, String)> {
    let wit = format!(
        "package test:errors; world boundary {{ import wasi:http/client@0.3.0; import wasi:cli/stderr@0.3.0; export run: {}func()->string; }}",
        if asynchronous { "async " } else { "" }
    );
    let (mut store, instance, output) = instantiate(source, &wit).await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    let result =
        tokio::time::timeout(Duration::from_secs(5), run.call_async(&mut store, ())).await?;
    Ok((result, String::from_utf8(output.contents().to_vec())?))
}

#[tokio::test(flavor = "current_thread")]
async fn uncaught_values_report_the_actual_failure() -> Result<()> {
    for (value, expected) in [
        ("new Error('broken')", "Error: broken"),
        ("new TypeError('invalid')", "TypeError: invalid"),
        ("'text\\0雪'", "text\0雪"),
        ("false", "false"),
        ("undefined", "undefined"),
        ("null", "null"),
        ("42", "42"),
    ] {
        for asynchronous in [false, true] {
            let source = if asynchronous {
                format!(
                    "async function fail():Promise<string>{{throw {value};}} export async function run():Promise<string>{{return fail();}}"
                )
            } else {
                format!("export function run():string{{throw {value};}}")
            };
            let (result, stderr) = invoke(&source, asynchronous).await?;
            assert!(result.is_err(), "{source}");
            assert_eq!(stderr, format!("Uncaught {expected}\n"), "{source}");
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_rejections_keep_category_url_and_identity() -> Result<()> {
    let (result, stderr) = invoke(
        r#"
        export async function run():Promise<string> {
            const pending=fetch('http://refused.invalid/doc.json');
            try {await pending;} catch(original) {
              try {await pending;} catch(e) {
                if(e!==original || !(e instanceof TypeError))throw new Error('identity lost');
                return e.name+':'+e.cause.code+':'+e.cause.url;
              }
            }
            return 'unexpected success';
        }
    "#,
        true,
    )
    .await?;
    assert_eq!(
        result?.0,
        "TypeError:connection-refused:http://refused.invalid/doc.json"
    );
    assert_eq!(stderr, "");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn caught_fetch_does_not_replace_a_later_uncaught_error() -> Result<()> {
    let (result, stderr) = invoke(
        r#"
        export async function run():Promise<string> {
            try {await fetch('http://refused.invalid/');} catch(e) {}
            throw new Error('later failure');
        }
    "#,
        true,
    )
    .await?;
    assert!(result.is_err());
    assert_eq!(stderr, "Uncaught Error: later failure\n");
    let (result, stderr) = invoke(
        r#"
        export async function run():Promise<string> {
            const responses=await Promise.all([fetch('http://refused.invalid/doc.json')]);
            return 'unexpected success';
        }
    "#,
        true,
    )
    .await?;
    assert!(result.is_err());
    assert_eq!(
        stderr,
        "Uncaught TypeError: fetch failed: connection-refused: http://refused.invalid/doc.json\n"
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq, ComponentType, Lift)]
#[component(record)]
struct ExportError {
    name: String,
    message: String,
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_fetch_returns_a_typed_error_and_preserves_instance_reuse() -> Result<()> {
    let wit = "package test:errors; world boundary {
        import wasi:http/client@0.3.0;
        record failure { name:string, message:string }
        export run:async func(fail:bool)->result<string,failure>;
    }";
    let (mut store, instance, output) = instantiate(r#"
        export async function run(fail:boolean):Promise<string> {
            if(!fail)return 'recovered';
            await Promise.all([fetch('http://refused.invalid/doc.json'), fetch('http://refused.invalid/other.json')]);
            return 'unexpected success';
        }
    "#, wit).await?;
    let run =
        instance.get_typed_func::<(bool,), (Result<String, ExportError>,)>(&mut store, "run")?;
    for fail in [true, false].into_iter().cycle().take(20) {
        let result =
            tokio::time::timeout(Duration::from_secs(5), run.call_async(&mut store, (fail,)))
                .await??
                .0;
        if fail {
            let error = result.unwrap_err();
            assert_eq!(error.name, "TypeError");
            assert!(
                error
                    .message
                    .starts_with("fetch failed: connection-refused: http://refused.invalid/"),
                "{error:?}"
            );
        } else {
            assert_eq!(result, Ok("recovered".into()));
        }
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    assert!(output.contents().is_empty());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn module_initialization_rejections_use_the_export_error_channel() -> Result<()> {
    let (mut store, instance, output) = instantiate(
        r#"
        async function initialize():Promise<string>{throw new Error('initialization failed');}
        const initialized=await initialize();
        export async function run():Promise<string>{return initialized;}
    "#,
        "package test:errors; world boundary {
        record failure { name:string, message:string }
        export run:async func()->result<string,failure>;
    }",
    )
    .await?;
    let run = instance.get_typed_func::<(), (Result<String, ExportError>,)>(&mut store, "run")?;
    for _ in 0..10 {
        assert_eq!(
            run.call_async(&mut store, ()).await?.0,
            Err(ExportError {
                name: "Error".into(),
                message: "initialization failed".into(),
            })
        );
        store.assert_concurrent_state_empty();
    }
    assert!(output.contents().is_empty());
    Ok(())
}
