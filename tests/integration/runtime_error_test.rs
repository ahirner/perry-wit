use crate::core_program::Program;
use crate::world::Compiler::{MemoryChecked, Standard};
use crate::{output_capture, wasi_host};
use anyhow::Result;
use std::{future::Future, time::Duration};
use wasmtime::component::{Component, ComponentType, Instance, Lift, Linker};
use wasmtime::{Engine, Store};
use wasmtime_wasi::WasiCtx;
use wasmtime_wasi_http::{WasiBody, WasiHttpHooks};

#[test]
fn error_classes_causes_rethrows_primitives_and_rejections_preserve_values() {
    let source = r#"
interface Detail { code:string }
interface Nested { pair:[Detail,string]; table:Record<string,Detail> }
async function fail():Promise<number> {const values:number[]=[];return values[0];}
export async function run():Promise<number> {
 const original=new ReferenceError('original');
 const detail:Detail={code:'E_TEST'};
 const error=new SyntaxError('outer',{cause:original});
 if(error.name!=='SyntaxError' || error.message!=='outer' || !(error instanceof Error))return 1;
 if(error.cause!==original || !(error.cause instanceof ReferenceError))return 2;
 try{throw error;}catch(e){if(e!==error || e.cause.name!=='ReferenceError')return 3;}
 const record=new Error('record',{cause:detail});
 detail.code='E_CHANGED';
 if(record.cause!==detail || record.cause!==record.cause || record.cause.code!=='E_CHANGED')return 4;
 const number=new Error('number',{cause:42});
 if(number.cause!==42)return 5;
 try{throw number.cause;}catch(e){if(e!==number.cause)return 9;}
 const children:Detail[]=[detail];const arrayError=new Error('array',{cause:children});
 if(arrayError.cause.length!==1 || arrayError.cause[0]!==detail)return 10;
 const pair:[Detail,string]=[detail,'kept'];
 const table:Record<string,Detail>={first:detail};
 const nested:Nested={pair,table};const nestedError=new Error('nested',{cause:nested});
 if(nestedError.cause.pair!==pair || nestedError.cause.table!==table)return 20;
 try{throw null;}catch(e){if(e!==null)return 6;}
 try{throw undefined;}catch(e){if(e!==undefined)return 7;}
 let first=new Error('placeholder');
 try{throw 42;}catch(e){first=e;if(e!==42)return 11;}
 try{throw 42;}catch(e){if(e!==first)return 12;}
 try{throw false;}catch(e){if(e!==false)return 13;}
 try{throw 'value';}catch(e){if(e!=='value')return 14;}
 try{throw 9007199254740993n;}catch(e){first=e;if(e!==9007199254740993n || 1n===e)return 15;}
 try{throw 9007199254740993n;}catch(e){if(e!==first)return 16;}
 const a=new Error('same');const b=new Error('same');if(a===b)return 17;
 const nanPending=Promise.reject(NaN);
 try{await nanPending;}catch(e){first=e;}
 try{await nanPending;}catch(e){if(e===first)return 18;}
 const pending=fail();let saved=new Error('placeholder');
 try{await pending;}catch(e){saved=e;}
 try{await pending;}catch(e){if(e!==saved || !(e instanceof TypeError))return 19;}
 try{try{throw error;}finally{return 0;}}catch(e){return 8;}
}
export function json():number {
 try{JSON.parse('{"é":"🙂",bad}');}catch(e){
  if(!(e instanceof SyntaxError) || e.name!=='SyntaxError')return 1;
  if(e.message!=='Invalid JSON at byte 13 (line 1, column 10)')return 2;
 }
 try{JSON.parse('{\r\n"x":bad}');}catch(e){
  if(e.message!=='Invalid JSON at byte 7 (line 2, column 5)')return 3;
 }
 return 0;
}
"#;
    let mut program = Program::checked(source, "errors.ts");
    program.stress_collection(true);
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
    assert_eq!(program.outcome("json", &[]), (0.0, 0));
}

#[derive(ComponentType, Lift, Debug, PartialEq, Eq)]
#[component(record)]
struct Failure {
    name: String,
    message: String,
}

#[test]
fn initialization_and_native_failures_use_declared_results() -> Result<()> {
    let engine = Engine::default();
    let init_wit = "package test:initialization; world app { record failure {name:string,message:string} export run:func()->result<f64,failure>; }";
    let init_source = "function setup():number{throw new RangeError('initialization failed');} const state=setup(); export function run():number{return state;}";
    let init_component = Standard.component(init_wit, "app", "initialization.ts", init_source)?;
    let (mut store, instance) = crate::world::instantiate(&engine, &init_component)?;
    let run = instance.get_typed_func::<(), (Result<f64, Failure>,)>(&mut store, "run")?;
    for _ in 0..3 {
        let (error,) = run.call(&mut store, ())?;
        assert_eq!(
            error,
            Err(Failure {
                name: "RangeError".into(),
                message: "initialization failed".into()
            })
        );
    }

    let native_wit = "package test:errors; world boundary { record failure {name:string,message:string} export run:func(which:f64)->result<string,failure>; }";
    let native_source = r#"
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
    let native_component =
        Standard.component(native_wit, "boundary", "native.ts", native_source)?;
    let (mut store, instance) = crate::world::instantiate(&engine, &native_component)?;
    let run = instance.get_typed_func::<(f64,), (Result<String, Failure>,)>(&mut store, "run")?;
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
            run.call(&mut store, (which,))?.0,
            Err(Failure {
                name: name.into(),
                message: message.into()
            }),
            "which={which}"
        );
    }
    Ok(())
}

struct HttpFailure(wasmtime_wasi_http::Error);
impl WasiHttpHooks for HttpFailure {
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
type Host = wasi_host::Host<HttpFailure>;

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
        .with_hooks(HttpFailure(wasmtime_wasi_http::Error::ConnectionRefused));
    let mut store = wasi_host::store(engine, host);
    let instance = linker.instantiate_async(&mut store, component).await?;
    Ok((store, instance, output))
}

#[tokio::test(flavor = "current_thread")]
async fn uncaught_values_and_fetch_errors_preserve_boundaries() -> Result<()> {
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
            if (which === 9) throw 42;
            if (which === 10) {
                try { await fetch('http://refused.invalid/'); } catch (e) {}
                throw new Error('later failure');
            }
            await Promise.all([fetch('http://refused.invalid/doc.json')]);
            return 'unexpected success';
        }
        export async function uncaught(which: number): Promise<string> { return fail(which); }
        export async function categories(): Promise<string> {
            const pending = fetch('http://refused.invalid/doc.json');
            try { await pending; } catch (original) {
                try { await pending; } catch (e) {
                    if (e !== original || !(e instanceof TypeError)) throw new Error('identity lost');
                    return e.name + '|' + e.message + '|' + e.cause.code + '|' + e.cause.url;
                }
            }
            return 'unexpected success';
        }
        export async function reuse(fail: boolean): Promise<string> {
            if (!fail) return 'recovered';
            await Promise.all([fetch('http://refused.invalid/doc.json'), fetch('http://refused.invalid/other.json')]);
            return 'unexpected success';
        }
    "#;
    let wit = "package test:errors; world boundary {
        import wasi:http/client@0.3.0;
        import wasi:cli/stderr@0.3.0;
        record failure { name: string, message: string }
        export uncaught: async func(which: f64) -> string;
        export categories: async func() -> string;
        export reuse: async func(fail: bool) -> result<string, failure>;
    }";
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
        (10.0, "Error: later failure"),
        (
            11.0,
            "TypeError: fetch failed: connection-refused: http://refused.invalid/doc.json",
        ),
    ] {
        let (mut store, instance, output) =
            instantiate_compiled(&engine, &component, &linker).await?;
        let uncaught = instance.get_typed_func::<(f64,), (String,)>(&mut store, "uncaught")?;
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            uncaught.call_async(&mut store, (which,)),
        )
        .await?;
        assert!(result.is_err(), "which={which}");
        assert_eq!(
            String::from_utf8(output.contents().to_vec())?,
            format!("Uncaught {expected}\n"),
            "which={which}"
        );
    }

    let (mut store, instance, output) = instantiate_compiled(&engine, &component, &linker).await?;
    let categories = instance.get_typed_func::<(), (String,)>(&mut store, "categories")?;
    use wasmtime_wasi_http::Error as HttpError;
    for (error, category) in [
        (HttpError::DnsTimeout, "DNS-timeout"),
        (HttpError::ConnectionRefused, "connection-refused"),
        (HttpError::HttpRequestDenied, "HTTP-request-denied"),
        (HttpError::InternalError(None), "internal-error"),
    ] {
        store.data_mut().hooks.0 = error;
        let result = categories.call_async(&mut store, ()).await;
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

    let reuse =
        instance.get_typed_func::<(bool,), (Result<String, Failure>,)>(&mut store, "reuse")?;
    for fail in [true, false].into_iter().cycle().take(20) {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            reuse.call_async(&mut store, (fail,)),
        )
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
