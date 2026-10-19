use crate::world::Compiler::MemoryChecked;
use anyhow::Result;
use std::{future::Future, time::Duration};
use wasmtime::component::{Component, ComponentType, Instance, Lift, Linker};
use wasmtime::{Engine, Store};
use wasmtime_wasi::WasiCtx;
use wasmtime_wasi_http::{WasiBody, WasiHttpHooks};

use crate::{output_capture, wasi_host};

struct Failure(wasmtime_wasi_http::Error);
impl WasiHttpHooks for Failure {
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
        let error = std::mem::replace(&mut self.0, wasmtime_wasi_http::Error::ConnectionRefused);
        Box::new(async { Err(error) })
    }
}
type Host = wasi_host::Host<Failure>;

fn compile_component(source: &str, wit: &str) -> Result<(Engine, Component, Linker<Host>)> {
    let bytes = MemoryChecked.component(wit, "boundary", "errors.ts", source)?;
    let engine = crate::engine::async_engine()?;
    let component = Component::new(&engine, bytes)?;
    let linker = wasi_host::linker(&engine)?;
    Ok((engine, component, linker))
}

async fn instantiate_compiled(
    engine: &Engine,
    component: &Component,
    linker: &Linker<Host>,
) -> Result<(Store<Host>, Instance, output_capture::MemoryOutput)> {
    let output = output_capture::MemoryOutput::new(8192);
    let host = wasi_host::Host::new(WasiCtx::builder().stderr(output.clone()).build())
        .with_hooks(Failure(wasmtime_wasi_http::Error::ConnectionRefused));
    let mut store = wasi_host::store(engine, host);
    let instance = linker.instantiate_async(&mut store, component).await?;
    Ok((store, instance, output))
}

async fn instantiate(
    source: &str,
    wit: &str,
) -> Result<(Store<Host>, Instance, output_capture::MemoryOutput)> {
    let (engine, component, linker) = compile_component(source, wit)?;
    instantiate_compiled(&engine, &component, &linker).await
}

async fn invoke(source: &str) -> Result<(wasmtime::Result<(String,)>, String)> {
    let wit = "package test:errors; world boundary { import wasi:http/client@0.3.0; import wasi:cli/stderr@0.3.0; export run: async func()->string; }";
    let (mut store, instance, output) = instantiate(source, wit).await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    let result =
        tokio::time::timeout(Duration::from_secs(5), run.call_async(&mut store, ())).await?;
    Ok((result, String::from_utf8(output.contents().to_vec())?))
}

#[tokio::test(flavor = "current_thread")]
async fn uncaught_values_report_the_actual_failure() -> Result<()> {
    let source = r#"
        async function fail(which: number): Promise<string> {
            if (which === 0) throw new Error('broken');
            if (which === 1) throw new TypeError('invalid');
            if (which === 2) throw new RangeError('range');
            if (which === 3) throw new ReferenceError('reference');
            if (which === 4) throw new SyntaxError('syntax');
            if (which === 5) throw 'text\0雪';
            if (which === 6) throw false;
            if (which === 7) throw undefined;
            if (which === 8) throw null;
            throw 42;
        }
        export async function run(which: number): Promise<string> { return fail(which); }
    "#;
    let wit = "package test:errors; world boundary { import wasi:http/client@0.3.0; import wasi:cli/stderr@0.3.0; export run: async func(which: f64)->string; }";
    let (engine, component, linker) = compile_component(source, wit)?;
    for (which, expected) in [
        (0.0, "Error: broken"),
        (1.0, "TypeError: invalid"),
        (2.0, "RangeError: range"),
        (3.0, "ReferenceError: reference"),
        (4.0, "SyntaxError: syntax"),
        (5.0, "text\0雪"),
        (6.0, "false"),
        (7.0, "undefined"),
        (8.0, "null"),
        (9.0, "42"),
    ] {
        let (mut store, instance, output) =
            instantiate_compiled(&engine, &component, &linker).await?;
        let run = instance.get_typed_func::<(f64,), (String,)>(&mut store, "run")?;
        let result =
            tokio::time::timeout(Duration::from_secs(5), run.call_async(&mut store, (which,)))
                .await?;
        assert!(result.is_err(), "which={which}");
        assert_eq!(
            String::from_utf8(output.contents().to_vec())?,
            format!("Uncaught {expected}\n"),
            "which={which}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_rejections_keep_category_url_and_identity() -> Result<()> {
    let (mut store, instance, output) = instantiate(
        r#"
        export async function run():Promise<string> {
            const pending=fetch('http://refused.invalid/doc.json');
            try {await pending;} catch(original) {
              try {await pending;} catch(e) {
                if(e!==original || !(e instanceof TypeError))throw new Error('identity lost');
                return e.name+'|'+e.message+'|'+e.cause.code+'|'+e.cause.url;
              }
            }
            return 'unexpected success';
        }
    "#,
        "package test:errors; world boundary {import wasi:http/client@0.3.0; import wasi:cli/stderr@0.3.0; export run:async func()->string;}",
    )
    .await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    use wasmtime_wasi_http::Error as HttpError;
    for (error, category) in [
        (HttpError::DnsTimeout, "DNS-timeout"),
        (HttpError::ConnectionRefused, "connection-refused"),
        (HttpError::HttpRequestDenied, "HTTP-request-denied"),
        (HttpError::InternalError(None), "internal-error"),
    ] {
        store.data_mut().hooks.0 = error;
        let result = run.call_async(&mut store, ()).await;
        assert!(
            result.is_ok(),
            "{result:?}; stderr={}",
            String::from_utf8_lossy(&output.contents())
        );
        assert_eq!(
            result?.0,
            format!(
                "TypeError|fetch failed: {category}: http://refused.invalid/doc.json|{category}|http://refused.invalid/doc.json"
            )
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    assert!(output.contents().is_empty());
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
async fn native_failures_keep_their_domain_at_the_error_boundary() -> Result<()> {
    let source = r#"
        export function run(which: number): string {
            if (which === 0) new Date(NaN).toISOString();
            if (which === 1) new TextDecoder('invalid');
            if (which === 2) new TextDecoder('utf-8', { fatal: true }).decode(new Uint8Array([255]));
            if (which === 3) String.fromCodePoint(1114112);
            if (which === 4) new Uint8Array(-1);
            if (which === 5) Temporal.Instant.from('bad');
            return 'unexpected success';
        }
    "#;
    let (mut store, instance, _) = instantiate(
        source,
        "package test:errors; world boundary {
            record failure {name:string,message:string}
            export run:func(which:f64)->result<string,failure>;
        }",
    )
    .await?;
    let run =
        instance.get_typed_func::<(f64,), (Result<String, ExportError>,)>(&mut store, "run")?;
    for (which, name, message) in [
        (0.0, "RangeError", "Invalid time value"),
        (1.0, "RangeError", "Unsupported text encoding"),
        (
            2.0,
            "TypeError",
            "The encoded data was not valid for encoding utf-8",
        ),
        (3.0, "RangeError", "Invalid Unicode scalar value"),
        (4.0, "RangeError", "Invalid typed array length"),
        (5.0, "RangeError", "Invalid time value for Temporal"),
    ] {
        assert_eq!(
            run.call_async(&mut store, (which,)).await?.0,
            Err(ExportError {
                name: name.into(),
                message: message.into()
            }),
            "which={which}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn filesystem_failures_include_category_and_path() -> Result<()> {
    let (mut store, instance, _) = instantiate(r#"
        import {readFile} from 'node:fs/promises';
        export async function run():Promise<string>{await readFile('/missing.json');return 'unexpected success';}
    "#, "package test:errors; world boundary {
        import wasi:filesystem/preopens@0.3.0;
        record failure {name:string,message:string}
        export run:async func()->result<string,failure>;
    }").await?;
    let run = instance.get_typed_func::<(), (Result<String, ExportError>,)>(&mut store, "run")?;
    let error = run.call_async(&mut store, ()).await?.0.unwrap_err();
    assert_eq!(error.name, "Error");
    assert_eq!(
        error.message,
        "ENOENT: no such file or directory, open '/missing.json'"
    );
    assert!(store.data().table.is_empty());
    Ok(())
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
