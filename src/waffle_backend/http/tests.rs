use super::*;
use std::time::Duration;
use tokio::time::timeout;
use waffle::{Export, ExportKind, MemoryData};
use wasmtime::component::{Component, ComponentType, Instance, Lift, Linker, Lower, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

#[allow(dead_code, unreachable_pub)]
#[path = "../../../tests/support/http_fixture.rs"]
mod fixture;
use crate::{compile_typescript_waffle, waffle_backend::WaffleCompileOptions};
use fixture::{HttpFixture, Reply};

async fn instantiate_source(source: &str, memory: usize) -> Result<(Store<Host>, Instance)> {
    let compiled = compile_typescript_waffle(source, "http.ts", &WaffleCompileOptions::default())?;
    instantiate_component(compiled.component.unwrap(), memory).await
}

struct Host {
    http: WasiHttpCtx,
    table: ResourceTable,
    limits: StoreLimits,
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

#[derive(Debug, ComponentType, Lift, Lower)]
#[component(record)]
struct Response {
    status: u16,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
}

async fn instantiate() -> Result<(Store<Host>, Instance)> {
    let mut module = Module::empty();
    let memory = module.memories.push(MemoryData {
        initial_pages: 1,
        maximum_pages: None,
        segments: vec![],
    });
    module.exports.push(Export {
        name: "memory".into(),
        kind: ExportKind::Memory(memory),
    });
    let imports = declare_imports(&mut module);
    let allocator = super::super::allocation::emit_allocator(&mut module, memory, 1024)?;
    let get = emit_runtime(&mut module, memory, allocator, &imports)?;
    let wrappers = runtime::emit_functions(
        &mut module,
        memory,
        r#"(module
      (import "host" "get" (func $get (param i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))
      (import "host" "realloc" (func $realloc (param i32 i32 i32 i32) (result i32)))
      (import "host" "post-return" (func $post-return)) (memory 1)
      (func (export "run") (param $authority i32) (param $authority-length i32)
        (param $path i32) (param $path-length i32) (param $headers i32) (param $count i32) (param $limit i32) (result i32)
        (local $result i32) (local $error i32)
        (local.set $result (call $realloc (i32.const 0) (i32.const 0) (i32.const 4) (i32.const 24)))
        (local.set $error (call $get (i32.const 0) (local.get $authority) (local.get $authority-length)
          (local.get $path) (local.get $path-length) (local.get $headers) (local.get $count) (local.get $limit)
          (i32.add (local.get $result) (i32.const 4))))
        (i32.store (local.get $result) (i32.ne (local.get $error) (i32.const 0)))
        (if (local.get $error) (then (i32.store offset=4 (local.get $result) (local.get $error))))
        (local.get $result))
      (func (export "post") (param i32) (call $post-return)))"#,
        &BTreeMap::from([
            ("get", get),
            ("realloc", allocator.realloc),
            ("post-return", allocator.post_return),
        ]),
    )?;
    for (name, function) in wrappers {
        module.exports.push(Export {
            name,
            kind: ExportKind::Func(function),
        });
    }
    let core = wasmprinter::print_bytes(module.to_wasm_bytes()?)?;
    let wat = format!(
        r#"(component {}
      (core module $guest {})
      (core instance $guest (instantiate $guest (with "http" (instance $http-forward))))
      {}
      (type $response (record (field "status" u16) (field "headers" (list (tuple string (list u8)))) (field "body" (list u8))))
      (export $buffered-response "buffered-response" (type $response))
      (func (export "run") async (param "authority" string) (param "path" string)
        (param "headers" (list (tuple string (list u8)))) (param "limit" u32)
        (result (result $buffered-response (error u32)))
        (canon lift (core func $guest "run") (memory (core memory $guest "memory"))
          (realloc (core func $guest "cabi_realloc")) (post-return (core func $guest "post")))))"#,
        declare_adapters()?,
        core.strip_prefix("(module")
            .unwrap()
            .trim_end()
            .strip_suffix(')')
            .unwrap(),
        bind_adapters()?
    );
    instantiate_component(wat::parse_str(&wat)?, 16 * 1024 * 1024).await
}

async fn instantiate_component(bytes: Vec<u8>, memory: usize) -> Result<(Store<Host>, Instance)> {
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_threading(true)
        .wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, bytes)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = Store::new(
        &engine,
        Host {
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(memory).build(),
        },
    );
    store.limiter(|host| &mut host.limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    Ok((store, instance))
}

#[tokio::test(flavor = "current_thread")]
async fn buffered_get_uses_real_p3_http_resources_and_preserves_headers_status_and_bytes()
-> Result<()> {
    let server = HttpFixture::new(|request| {
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/document?q=%23value");
        assert!(
            request
                .headers
                .contains(&("x-request".into(), "value".into()))
        );
        Reply::WithHeaders(
            404,
            vec![
                ("X-Value".into(), "first".into()),
                ("X-Value".into(), "second".into()),
            ],
            "é😀\0".into(),
        )
    });
    let (mut store, instance) = instantiate().await?;
    let run = instance
        .get_typed_func::<(String, String, Vec<(String, Vec<u8>)>, u32), (Result<Response, u32>,)>(
            &mut store, "run",
        )?;
    for _ in 0..10 {
        let result = timeout(
            Duration::from_secs(5),
            run.call_async(
                &mut store,
                (
                    server.address.to_string(),
                    "/document?q=%23value".into(),
                    vec![("x-request".into(), b"value".to_vec())],
                    1024,
                ),
            ),
        )
        .await??
        .0
        .unwrap();
        assert_eq!(result.status, 404);
        assert_eq!(result.body, "é😀\0".as_bytes());
        let values: Vec<_> = result
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("x-value"))
            .map(|(_, value)| value.as_slice())
            .collect();
        assert_eq!(values, vec![b"first".as_slice(), b"second".as_slice()]);
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn bounded_responses_handle_exact_limits_overflow_and_repeated_four_mib_bodies() -> Result<()>
{
    let server = HttpFixture::new(|request| {
        let length: usize = request.target.trim_start_matches('/').parse().unwrap();
        Reply::Bytes(200, (0..length).map(|index| index as u8).collect())
    });
    let (mut store, instance) = instantiate().await?;
    let run = instance
        .get_typed_func::<(String, String, Vec<(String, Vec<u8>)>, u32), (Result<Response, u32>,)>(
            &mut store, "run",
        )?;
    for length in [0_u32, 1, 8192, 8193, 4 * 1024 * 1024] {
        for limit in [length, length.saturating_sub(1)] {
            for _ in 0..5 {
                let result = timeout(
                    Duration::from_secs(5),
                    run.call_async(
                        &mut store,
                        (
                            server.address.to_string(),
                            format!("/{length}"),
                            vec![],
                            limit,
                        ),
                    ),
                )
                .await??
                .0;
                if limit < length {
                    assert_eq!(result.unwrap_err(), 8);
                } else {
                    let response = result.unwrap();
                    assert_eq!(response.status, 200);
                    assert_eq!(
                        response.body,
                        (0..length).map(|index| index as u8).collect::<Vec<_>>()
                    );
                }
                store.assert_concurrent_state_empty();
                assert!(store.data().table.is_empty());
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn metadata_and_transport_errors_release_resources_and_allow_reuse() -> Result<()> {
    let server = HttpFixture::new(|request| match request.target.as_str() {
        "/disconnect" => Reply::Disconnect,
        "/incomplete" => Reply::TruncatedBody(b"partial".to_vec(), 100),
        _ => Reply::Bytes(200, b"recovered".to_vec()),
    });
    let (mut store, instance) = instantiate().await?;
    let run = instance
        .get_typed_func::<(String, String, Vec<(String, Vec<u8>)>, u32), (Result<Response, u32>,)>(
            &mut store, "run",
        )?;
    for _ in 0..10 {
        for (authority, path, headers, expected) in [
            (
                server.address.to_string(),
                String::from("/"),
                vec![("bad:name".into(), vec![])],
                200..201,
            ),
            ("bad authority".into(), "/".into(), vec![], 12..13),
            (
                server.address.to_string(),
                "/bad path".into(),
                vec![],
                12..13,
            ),
            (
                server.address.to_string(),
                "/disconnect".into(),
                vec![],
                100..139,
            ),
            (
                server.address.to_string(),
                "/incomplete".into(),
                vec![],
                100..139,
            ),
        ] {
            let result = timeout(
                Duration::from_secs(5),
                run.call_async(&mut store, (authority, path.clone(), headers, 1024)),
            )
            .await??
            .0;
            let error = result.expect_err("invalid metadata and failed transfers cannot succeed");
            assert!(expected.contains(&error), "{path}: {error}");
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
            let recovered = timeout(
                Duration::from_secs(5),
                run.call_async(
                    &mut store,
                    (server.address.to_string(), "/".into(), vec![], 1024),
                ),
            )
            .await??
            .0
            .unwrap();
            assert_eq!(recovered.body, b"recovered");
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.target != "/bad path"
                && request.headers.iter().all(|(name, _)| name != "bad:name"))
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn disposal_abandons_pending_headers_and_body_transfers() -> Result<()> {
    for reply in [false, true] {
        let server = HttpFixture::new(move |_| {
            if reply {
                Reply::StallBody
            } else {
                Reply::Stall
            }
        });
        let (mut store, instance) = instantiate().await?;
        let run = instance
            .get_typed_func::<(String, String, Vec<(String, Vec<u8>)>, u32), (Result<Response, u32>,)>(
                &mut store, "run",
            )?;
        assert!(
            timeout(
                Duration::from_millis(100),
                run.call_async(
                    &mut store,
                    (server.address.to_string(), "/".into(), vec![], 1024)
                )
            )
            .await
            .is_err()
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
        drop(store);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn typed_source_preserves_response_fields_headers_and_binary_body() -> Result<()> {
    let server = HttpFixture::new(|request| {
        assert!(
            request
                .headers
                .contains(&("x-request".into(), "value".into()))
        );
        Reply::WithHeaders(
            404,
            vec![
                ("X-Value".into(), "first".into()),
                ("X-Value".into(), "second".into()),
            ],
            "é😀\0".into(),
        )
    });
    let source = r#"
        import {get as request, type HttpResponse as Response} from 'perry:http';
        function body(response:Response):Uint8Array { return response.body; }
        export async function run(authority:string):Promise<Result<string,number>> {
            const response=await request('http',authority,'/',{'x-request':'value'},1024);
            if(response.status!==404 || response.body!==body(response)) { throw 99; }
            const decoder=new TextDecoder();
            let headers='';
            for(let i=0;i<response.headerCount;i++) {
                if(response.headerName(i)==='x-value') { headers+=decoder.decode(response.headerValue(i))+':'; }
            }
            return headers+decoder.decode(response.body);
        }
    "#;
    let (mut store, instance) = instantiate_source(source, 65536).await?;
    let run = instance.get_typed_func::<(String,), (Result<String, f64>,)>(&mut store, "run")?;
    for _ in 0..20 {
        let result = timeout(
            Duration::from_secs(5),
            run.call_async(&mut store, (server.address.to_string(),)),
        )
        .await??
        .0;
        assert_eq!(result, Ok("first:second:é😀\0".into()));
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn source_response_and_detached_views_survive_collection_and_repeated_calls() -> Result<()> {
    let server = HttpFixture::new(|_| {
        Reply::WithHeaders(
            200,
            vec![("X-Owned".into(), "retained".into())],
            "body:é😀".into(),
        )
    });
    let source = r#"
        import {get, type HttpResponse} from 'perry:http';
        function retain(response:HttpResponse):HttpResponse { return response; }
        async function retainedHeader(authority:string):Promise<Uint8Array> {
            const response=await get('http',authority,'/',{},1024);
            for(let i=0;i<response.headerCount;i++) {
                if(response.headerName(i)==='x-owned') { return response.headerValue(i); }
            }
            throw 99;
        }
        export async function run(authority:string):Promise<string> {
            const first=await get('http',authority,'/',{},1024);
            const saved={response:retain(first),body:first.body};
            let name='';
            const bytes=await retainedHeader(authority);
            for(let i=0;i<first.headerCount;i++) {
                if(first.headerName(i)==='x-owned') { name=first.headerName(i); }
            }
            for(let i=0;i<2000;i++) { const temporary=authority+authority+authority+authority; if(temporary.length!==authority.length*4) {throw 99;} }
            if(saved.response!==first || saved.body!==first.body) {throw 98;}
            const next=await get('http',authority,'/',{},1024);
            if(next===first || next.body===first.body) {throw 97;}
            const decoder=new TextDecoder();
            return name+':'+decoder.decode(bytes)+':'+decoder.decode(saved.body);
        }
    "#;
    let (mut store, instance) = instantiate_source(source, 262144).await?;
    let run = instance.get_typed_func::<(String,), (String,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(
            timeout(
                Duration::from_secs(5),
                run.call_async(&mut store, (server.address.to_string(),))
            )
            .await??
            .0,
            "x-owned:retained:body:é😀"
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn source_validation_preserves_argument_order_and_errors_before_io() -> Result<()> {
    let source = r#"
        import * as http from 'perry:http';
        function text(state:{trace:string},value:string):string {state.trace+=value;return value;}
        function headers(state:{trace:string}):{[name:string]:string} {state.trace+='H';return {};}
        function limit(state:{trace:string}):number {state.trace+='L';return -1;}
        export async function run(authority:string):Promise<string> {
            const state={trace:''};
            try { await http.get(text(state,'S'),text(state,authority),text(state,'/'),headers(state),limit(state)); }
            catch(error) { if(error!==12) {throw error;} return state.trace; }
            throw 99;
        }
    "#;
    let server = HttpFixture::new(|_| panic!("invalid request must not start I/O"));
    let (mut store, instance) = instantiate_source(source, 65536).await?;
    let run = instance.get_typed_func::<(String,), (String,)>(&mut store, "run")?;
    let authority = server.address.to_string();
    assert_eq!(
        run.call_async(&mut store, (authority.clone(),)).await?.0,
        format!("S{authority}/HL")
    );
    assert!(server.requests.lock().unwrap().is_empty());
    store.assert_concurrent_state_empty();
    assert!(store.data().table.is_empty());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn json_fetch_checks_status_media_type_and_payload() -> Result<()> {
    let server = HttpFixture::new(|request| {
        let (status, headers, body) = match request.target.as_str() {
            "/json" => (
                200,
                vec![("Content-Type".into(), "application/json".into())],
                "{\"text\":\"é😀\"}".into(),
            ),
            "/status" => (404, vec![], "missing".into()),
            "/missing" => (200, vec![], "missing".into()),
            "/duplicate" => (
                200,
                vec![
                    ("Content-Type".into(), "application/json".into()),
                    ("Content-Type".into(), "application/json".into()),
                ],
                "duplicate".into(),
            ),
            "/unsupported" => (
                200,
                vec![("Content-Type".into(), "image/png".into())],
                "unsupported".into(),
            ),
            "/bad-json" => (
                200,
                vec![("Content-Type".into(), "application/json".into())],
                "{".into(),
            ),
            _ => unreachable!(),
        };
        Reply::WithHeaders(status, headers, body)
    });
    let (mut store, instance) =
        instantiate_source(include_str!("../../../tests/fixtures/http_json.ts"), 524288).await?;
    let run =
        instance.get_typed_func::<(String, String), (Result<String, f64>,)>(&mut store, "run")?;
    for _ in 0..10 {
        for (path, expected) in [
            ("/json", Ok("{\"text\":\"é😀\"}".into())),
            ("/status", Err(400.)),
            ("/missing", Err(401.)),
            ("/duplicate", Err(401.)),
            ("/unsupported", Err(402.)),
            ("/bad-json", Err(1.)),
        ] {
            assert_eq!(
                timeout(
                    Duration::from_secs(5),
                    run.call_async(&mut store, (server.address.to_string(), path.into()))
                )
                .await??
                .0,
                expected,
                "{path}"
            );
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    Ok(())
}

#[test]
fn source_http_contract_rejects_unsupported_types_and_retained_requests() {
    for (body, diagnostic) in [
        (
            "await get('http','host','/',{x:1},1024);return 0;",
            "header values must be strings",
        ),
        (
            "await get('http','host','/',42,1024);return 0;",
            "statically typed string dictionary",
        ),
        (
            "await get('http','host','/',{},'1024');return 0;",
            "limit must be a number",
        ),
        (
            "const response=await get('http','host','/',{},1024);response.status=201;return 0;",
            "read-only",
        ),
        (
            "const response=await get('http','host','/',{},1024);response.headerValue('0');return 0;",
            "numeric index",
        ),
        (
            "const request=get('http','host','/',{},1024);return 0;",
            "Retained native capability Promises",
        ),
    ] {
        let source = format!(
            "import {{get}} from 'perry:http';export async function run():Promise<number>{{{body}}}"
        );
        let error = compile_typescript_waffle(&source, "http.ts", &WaffleCompileOptions::default())
            .expect_err("must diagnose unsupported HTTP source");
        assert!(
            format!("{error:#}").contains(diagnostic),
            "{body}: {error:#}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn source_binary_boundaries_keep_limits_and_completion_errors_separate_from_decoding()
-> Result<()> {
    let server = HttpFixture::new(|request| match request.target.as_str() {
        "/incomplete" => Reply::TruncatedBody(b"partial".to_vec(), 100),
        "/large" => Reply::Bytes(200, vec![255; 4 * 1024 * 1024]),
        _ => Reply::Bytes(200, vec![0, 255, 128, 240, 159, 146]),
    });
    let source = r#"
        import {get} from 'perry:http';
        export async function run(authority:string,path:string,limit:number):Promise<Result<Uint8Array,number>> {
            const response=await get('http',authority,path,{},limit);
            return response.body;
        }
    "#;
    let (mut store, instance) = instantiate_source(source, 16 * 1024 * 1024).await?;
    let run = instance
        .get_typed_func::<(String, String, f64), (Result<Vec<u8>, f64>,)>(&mut store, "run")?;
    for _ in 0..5 {
        for (path, limit, expected) in [
            ("/", 6., Ok(vec![0, 255, 128, 240, 159, 146])),
            ("/", 5., Err(8.)),
            ("/", f64::NAN, Err(12.)),
            ("/", -1., Err(12.)),
            ("/large", 4194304., Ok(vec![255; 4 * 1024 * 1024])),
        ] {
            assert_eq!(
                timeout(
                    Duration::from_secs(5),
                    run.call_async(&mut store, (server.address.to_string(), path.into(), limit))
                )
                .await??
                .0,
                expected
            );
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
        let error = timeout(
            Duration::from_secs(5),
            run.call_async(
                &mut store,
                (server.address.to_string(), "/incomplete".into(), 1024.),
            ),
        )
        .await??
        .0
        .unwrap_err();
        assert!((100.0..139.0).contains(&error));
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    let source = source
        .replace("Result<Uint8Array,number>", "Result<string,number>")
        .replace(
            "return response.body;",
            "return new TextDecoder().decode(response.body);",
        );
    let (mut store, instance) = instantiate_source(&source, 65536).await?;
    let run = instance
        .get_typed_func::<(String, String, f64), (Result<String, f64>,)>(&mut store, "run")?;
    assert_eq!(
        run.call_async(&mut store, (server.address.to_string(), "/".into(), 1024.))
            .await?
            .0,
        Err(2.)
    );
    store.assert_concurrent_state_empty();
    assert!(store.data().table.is_empty());
    Ok(())
}

#[test]
fn http_sdk_checks_the_document_fixture_and_static_contract() -> Result<()> {
    let scratch = tempfile::tempdir()?;
    let source = scratch.path().join("http.ts");
    std::fs::write(
        &source,
        format!(
            "type Result<T,E>=T;\n{}",
            include_str!("../../../tests/fixtures/http_json.ts")
        ),
    )?;
    let errors = scratch.path().join("errors.ts");
    std::fs::write(
        &errors,
        r#"
        import {get, type HttpResponse} from 'perry:http';
        declare const response:HttpResponse;
        // @ts-expect-error immutable metadata
        response.status=201;
        // @ts-expect-error string-valued headers
        get('http','host','/',{number:1},1024);
        // @ts-expect-error supported schemes only
        get('ftp','host','/',{},1024);
        // @ts-expect-error numeric index
        response.headerValue('0');
    "#,
    )?;
    let output = std::process::Command::new("tsc")
        .current_dir(scratch.path())
        .args([
            "--noEmit", "--strict", "--target", "ES2022", "--module", "esnext",
        ])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/types/p3.d.ts"))
        .args([source, errors])
        .output()?;
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
