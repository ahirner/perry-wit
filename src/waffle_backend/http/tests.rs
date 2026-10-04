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
use fixture::{HttpFixture, Reply};

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
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, wat::parse_str(&wat)?)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = Store::new(
        &engine,
        Host {
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(16 * 1024 * 1024)
                .build(),
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
