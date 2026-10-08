#[path = "support/waffle.rs"]
mod waffle_fixture;
use anyhow::Result;
use perry_wit::waffle_backend::WaffleCompileOptions;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::{
    sync::Notify,
    time::{Duration, timeout},
};
use waffle_fixture::compile_typescript_waffle;
use wasmtime::component::{Component, Instance, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p3::bindings::clocks::system_clock::Instant;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

#[path = "waffle_platform/context.rs"]
mod context;
#[path = "waffle_platform/date.rs"]
mod date;
#[path = "waffle_platform/numbers.rs"]
mod numbers;
#[path = "waffle_platform/time.rs"]
mod time;
#[path = "waffle_platform/values.rs"]
mod values;

struct Host {
    context: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
}
impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.context,
            table: &mut self.table,
        }
    }
}

async fn instantiate(
    source: &str,
    memory: usize,
    configure: impl FnOnce(&mut Linker<Host>) -> Result<()>,
) -> Result<(Store<Host>, Instance)> {
    let compiled =
        compile_typescript_waffle(source, "platform.ts", &WaffleCompileOptions::default())?;
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_threading(true)
        .wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    linker.allow_shadowing(true);
    configure(&mut linker)?;
    let mut store = Store::new(
        &engine,
        Host {
            context: WasiCtx::default(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(memory).build(),
        },
    );
    store.limiter(|host| &mut host.limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    Ok((store, instance))
}

#[tokio::test(flavor = "current_thread")]
async fn random_short_reads_fill_only_the_visible_view_and_return_its_identity() -> Result<()> {
    let source = r#"
    export function run(input:Uint8Array):Uint8Array {
        const view=input.subarray(1,input.length-1);
        const filled=crypto.getRandomValues(view);
        if(filled!==view) {throw 99;}
        return input;
    }"#;
    let next = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut store, instance) = instantiate(source, 262144, |linker| {
        let next = next.clone();
        let requests = requests.clone();
        linker.instance("wasi:random/random@0.3.0")?.func_wrap(
            "get-random-bytes",
            move |_: StoreContextMut<'_, Host>, (length,): (u64,)| {
                requests.lock().unwrap().push(length);
                let length = length.min(17) as usize;
                let start = next.fetch_add(length, Ordering::SeqCst);
                Ok(((start..start + length)
                    .map(|index| (index * 37) as u8)
                    .collect::<Vec<_>>(),))
            },
        )?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(Vec<u8>,), (Vec<u8>,)>(&mut store, "run")?;
    for length in [2, 3, 259, 4098, 65538, 2, 259] {
        requests.lock().unwrap().clear();
        let mut expected = vec![0x55; length];
        expected[0] = 7;
        expected[length - 1] = 9;
        let input = expected.clone();
        let start = next.load(Ordering::SeqCst);
        for (index, byte) in expected[1..length - 1].iter_mut().enumerate() {
            *byte = ((start + index) * 37) as u8;
        }
        assert_eq!(run.call_async(&mut store, (input,)).await?.0, expected);
        assert_eq!(
            *requests.lock().unwrap(),
            (0..length - 2)
                .step_by(17)
                .map(|written| (length - 2 - written) as u64)
                .collect::<Vec<_>>()
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn random_rejection_preserves_argument_effects_and_avoids_host_calls() -> Result<()> {
    let source = r#"
    function invalid(state:Uint8Array):string {state[0]=3;return 'invalid';}
    export function run(size:number):number {
        const bytes=new Uint8Array(size);
        bytes[0]=7;bytes[size-1]=9;
        try {crypto.getRandomValues(bytes);return -1;}
        catch(error) {if(error.code !== 2) {throw error;}}
        const state=new Uint8Array(1);
        try {crypto.getRandomValues(invalid(state));return -2;}
        catch(error) {if(error.code !== 1) {throw error;}}
        return bytes[0]+bytes[size-1]+state[0];
    }"#;
    let calls = Arc::new(AtomicUsize::new(0));
    let (mut store, instance) = instantiate(source, 262144, |linker| {
        let calls = calls.clone();
        linker.instance("wasi:random/random@0.3.0")?.func_wrap(
            "get-random-bytes",
            move |_: StoreContextMut<'_, Host>, (_length,): (u64,)| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok((vec![0u8],))
            },
        )?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(run.call_async(&mut store, (65537.0,)).await?.0, 19.0);
        store.assert_concurrent_state_empty();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    for argument in [
        "null",
        "undefined",
        "false",
        "42",
        "'text'",
        "{}",
        "['text']",
    ] {
        let source = format!(
            "export function run():Result<Uint8Array,number> {{try {{return crypto.getRandomValues({argument});}} catch(error) {{throw error.code;}}}}"
        );
        let (mut store, instance) = instantiate(&source, 65536, |linker| {
            linker.instance("wasi:random/random@0.3.0")?.func_wrap(
                "get-random-bytes",
                |_: StoreContextMut<'_, Host>,
                 (_length,): (u64,)|
                 -> wasmtime::Result<(Vec<u8>,)> {
                    wasmtime::bail!("unexpected random call")
                },
            )?;
            Ok(())
        })
        .await?;
        let run = instance.get_typed_func::<(), (Result<Vec<u8>, f64>,)>(&mut store, "run")?;
        assert_eq!(
            run.call_async(&mut store, ()).await?.0,
            Err(1.0),
            "{argument}"
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn uuid_vectors_and_retained_results_survive_bounded_allocating_loops() -> Result<()> {
    let source = r#"
    async function create():Promise<string> {return crypto.randomUUID();}
    export async function run():Promise<string> {
        const pending=create();
        let index=0;
        while(index<2000) {
            const temporary=crypto.randomUUID();
            if(temporary.length!==36) {throw 99;}
            index=index+1;
        }
        return await pending+await pending;
    }"#;
    for (byte, expected) in [
        (None, "00010203-0405-4607-8809-0a0b0c0d0e0f"),
        (Some(0), "00000000-0000-4000-8000-000000000000"),
        (Some(255), "ffffffff-ffff-4fff-bfff-ffffffffffff"),
    ] {
        let next = AtomicUsize::new(0);
        let (mut store, instance) = instantiate(source, 65536, |linker| {
            linker.instance("wasi:random/random@0.3.0")?.func_wrap(
                "get-random-bytes",
                move |_: StoreContextMut<'_, Host>, (length,): (u64,)| {
                    let length = length.min(7) as usize;
                    let start = next.fetch_add(length, Ordering::SeqCst);
                    Ok(((start..start + length)
                        .map(|index| byte.unwrap_or((index % 16) as u8))
                        .collect::<Vec<_>>(),))
                },
            )?;
            Ok(())
        })
        .await?;
        let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
        for _ in 0..3 {
            assert_eq!(run.call_async(&mut store, ()).await?.0, expected.repeat(2));
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn clock_units_preserve_unsigned_monotonic_and_signed_epoch_values() -> Result<()> {
    let monotonic = Arc::new(AtomicU64::new(0));
    let epoch = Arc::new(Mutex::new(Instant {
        seconds: 0,
        nanoseconds: 0,
    }));
    let source = "export function run(wall:boolean):number {if(wall) {return Date.now();}return performance.now();}";
    let (mut store, instance) = instantiate(source, 65536, |linker| {
        let monotonic = monotonic.clone();
        let epoch = epoch.clone();
        linker
            .instance("wasi:clocks/monotonic-clock@0.3.0")?
            .func_wrap("now", move |_: StoreContextMut<'_, Host>, (): ()| {
                Ok((monotonic.load(Ordering::SeqCst),))
            })?;
        linker
            .instance("wasi:clocks/system-clock@0.3.0")?
            .func_wrap("now", move |_: StoreContextMut<'_, Host>, (): ()| {
                Ok((*epoch.lock().unwrap(),))
            })?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(bool,), (f64,)>(&mut store, "run")?;
    for nanos in [0, 1, 1_234_567_890, u64::MAX] {
        monotonic.store(nanos, Ordering::SeqCst);
        assert_eq!(
            run.call_async(&mut store, (false,)).await?.0,
            nanos as f64 / 1_000_000.0
        );
    }
    for (seconds, nanoseconds, expected) in [
        (0, 999999, 0.0),
        (-1, 999999999, -1.0),
        (-1, 1, -1000.0),
        (1234, 567890123, 1234567.0),
        (i64::MIN, 0, i64::MIN as f64 * 1000.0),
        (i64::MAX, 0, i64::MAX as f64 * 1000.0),
    ] {
        *epoch.lock().unwrap() = Instant {
            seconds,
            nanoseconds,
        };
        assert_eq!(run.call_async(&mut store, (true,)).await?.0, expected);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn random_failures_and_native_contract_violations_trap_without_guest_cleanup() -> Result<()> {
    for mode in 0..3 {
        let source = r#"
        export function run(input:Uint8Array):Uint8Array {
            try {return crypto.getRandomValues(input);}
            finally {Math.random();}
        }"#;
        let finalizers = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let (mut store, instance) = instantiate(source, 65536, |linker| {
            let finalizers = finalizers.clone();
            let calls = calls.clone();
            let mut random = linker.instance("wasi:random/random@0.3.0")?;
            random.func_wrap(
                "get-random-u64",
                move |_: StoreContextMut<'_, Host>, (): ()| {
                    finalizers.fetch_add(1, Ordering::SeqCst);
                    Ok((0u64,))
                },
            )?;
            random.func_wrap(
                "get-random-bytes",
                move |_: StoreContextMut<'_, Host>, (length,): (u64,)| {
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Ok((vec![165u8; 2],));
                    }
                    match mode {
                        0 => Ok((vec![],)),
                        1 => Ok((vec![0; length as usize + 1],)),
                        _ => wasmtime::bail!("controlled random failure"),
                    }
                },
            )?;
            Ok(())
        })
        .await?;
        let run = instance.get_typed_func::<(Vec<u8>,), (Vec<u8>,)>(&mut store, "run")?;
        let error = run.call_async(&mut store, (vec![0; 8],)).await.unwrap_err();
        if mode == 2 {
            assert!(format!("{error:#}").contains("controlled random failure"));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(finalizers.load(Ordering::SeqCst), 0);
        drop(store);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn filled_views_survive_retained_promises_and_runtime_string_alternatives() -> Result<()> {
    let source = r#"
    async function fill(view:Uint8Array,text:boolean):Promise<Uint8Array> {
        let value:string|Uint8Array=view;
        if(text) {value='wrong type';}
        try {return crypto.getRandomValues(value);} catch(error) {throw error.code;}
    }
    export async function run(text:boolean):Promise<Result<Uint8Array,number>> {
        const bytes=new Uint8Array([7,0,0,0,9]);
        const view=bytes.subarray(1,4);
        const pending=fill(view,text);
        let index=0;
        while(index<2000) {const temporary=new Uint8Array(128);index=index+1;}
        const first=await pending;
        if(first!==view) {throw 99;}
        if(await pending!==first) {throw 98;}
        first[0]=42;
        if(bytes[1]!==42) {throw 97;}
        return bytes;
    }"#;
    let calls = Arc::new(AtomicUsize::new(0));
    let (mut store, instance) = instantiate(source, 65536, |linker| {
        let calls = calls.clone();
        linker.instance("wasi:random/random@0.3.0")?.func_wrap(
            "get-random-bytes",
            move |_: StoreContextMut<'_, Host>, (length,): (u64,)| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok((vec![165u8; length as usize],))
            },
        )?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(bool,), (Result<Vec<u8>, f64>,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(
            run.call_async(&mut store, (false,)).await?.0,
            Ok(vec![7, 42, 165, 165, 9])
        );
        let before = calls.load(Ordering::SeqCst);
        assert_eq!(run.call_async(&mut store, (true,)).await?.0, Err(1.0));
        assert_eq!(calls.load(Ordering::SeqCst), before);
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn imports_and_lexical_shadows_remain_scoped_to_selected_operations() -> Result<()> {
    for (source, present, absent) in [
        (
            "export function run():number {return performance.now();}",
            "wasi:clocks/monotonic-clock@0.3.0",
            vec![
                "wait-for",
                "system-clock",
                "wasi:random/",
                "wasi:filesystem/",
                "wasi:http/",
                "wasi:cli/",
            ],
        ),
        (
            "export function run():number {return Date.now();}",
            "wasi:clocks/system-clock@0.3.0",
            vec![
                "monotonic-clock",
                "wasi:random/",
                "wasi:filesystem/",
                "wasi:http/",
                "wasi:cli/",
            ],
        ),
        (
            "export function run():string {return crypto['randomUUID']();}",
            "get-random-bytes",
            vec![
                "get-random-u64",
                "wasi:clocks/",
                "wasi:filesystem/",
                "wasi:http/",
                "wasi:cli/",
            ],
        ),
        (
            "export function run():number {return Math.random();}",
            "get-random-u64",
            vec![
                "get-random-bytes",
                "wasi:clocks/",
                "wasi:filesystem/",
                "wasi:http/",
                "wasi:cli/",
            ],
        ),
    ] {
        let compiled =
            compile_typescript_waffle(source, "imports.ts", &WaffleCompileOptions::default())?;
        let wat = compiled.component_wat.unwrap();
        assert!(wat.contains(present), "{source}");
        for missing in absent {
            assert!(!wat.contains(missing), "{source}: unexpected {missing}");
        }
    }
    let source = r#"
    function local(crypto:number,performance:number,Date:number):number {return crypto+performance+Date;}
    export function run():number {
        const crypto={getRandomValues:10};
        const performance={now:20};
        const Date={now:30};
        return local(crypto.getRandomValues,performance.now,Date.now);
    }"#;
    let compiled =
        compile_typescript_waffle(source, "shadow.ts", &WaffleCompileOptions::default())?;
    assert!(!compiled.component_wat.unwrap().contains("wasi:"));
    let (mut store, instance) = instantiate(source, 65536, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, ()).await?.0, 60.0);
    Ok(())
}

#[test]
fn unsupported_builtin_forms_have_explicit_diagnostics() {
    for body in [
        "return crypto.randomUUID(1)",
        "return crypto.getRandomValues()",
        "return crypto.getRandomValues(new Uint8Array(1),1)",
        "const call=crypto.randomUUID;return call()",
        "return crypto['random'+'UUID']()",
        "return performance.now(1)",
        "return Date.now(1)",
        "return crypto.getRandomValues(...[])",
        "return crypto.getRandomValues(new Uint16Array(1))",
    ] {
        let source = format!("export function run():number {{{body};}}");
        assert!(
            compile_typescript_waffle(&source, "invalid.ts", &WaffleCompileOptions::default())
                .is_err(),
            "{body}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn mixed_clock_random_tasks_retain_values_across_suspension_and_disposal() -> Result<()> {
    let source = r#"
    import {setTimeout as waitFor} from 'node:timers/promises';
    async function produce(bytes:Uint8Array):Promise<string> {
        crypto.getRandomValues(bytes);
        const id=crypto.randomUUID();
        await waitFor(3.25);
        if(bytes[0]!==165) {throw 99;}
        return id;
    }
    export async function run():Promise<number> {
        const before=performance.now();
        const pending=produce(new Uint8Array(8));
        let index=0;
        while(index<2000) {const garbage=new Uint8Array(256);index=index+1;}
        Math.random();
        const id=await pending;
        if(id!=='a5a5a5a5-a5a5-45a5-a5a5-a5a5a5a5a5a5') {throw 98;}
        if(await pending!==id) {throw 97;}
        if(Date.now()!==1234567) {throw 96;}
        return performance.now()-before;
    }"#;
    struct WaitOwner(Arc<AtomicUsize>);
    impl Drop for WaitOwner {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    for dispose in [false, true] {
        let entered = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let collected = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicUsize::new(0));
        let now = Arc::new(AtomicU64::new(1000));
        let (mut store, instance) = instantiate(source, 65536, |linker| {
            let entered = entered.clone();
            let finish = finish.clone();
            let dropped = dropped.clone();
            let now = now.clone();
            let mut clock = linker.instance("wasi:clocks/monotonic-clock@0.3.0")?;
            clock.func_wrap("now", move |_: StoreContextMut<'_, Host>, (): ()| {
                Ok((now.load(Ordering::SeqCst),))
            })?;
            clock.func_wrap_concurrent("wait-for", move |_, (duration,): (u64,)| {
                let entered = entered.clone();
                let finish = finish.clone();
                let dropped = dropped.clone();
                Box::pin(async move {
                    assert_eq!(duration, 3_000_000);
                    let _owner = WaitOwner(dropped);
                    entered.notify_one();
                    finish.notified().await;
                    Ok(())
                })
            })?;
            linker
                .instance("wasi:clocks/system-clock@0.3.0")?
                .func_wrap("now", |_: StoreContextMut<'_, Host>, (): ()| {
                    Ok((Instant {
                        seconds: 1234,
                        nanoseconds: 567890123,
                    },))
                })?;
            let mut random = linker.instance("wasi:random/random@0.3.0")?;
            random.func_wrap(
                "get-random-bytes",
                |_: StoreContextMut<'_, Host>, (length,): (u64,)| {
                    Ok((vec![165u8; length.min(7) as usize],))
                },
            )?;
            let collected = collected.clone();
            random.func_wrap(
                "get-random-u64",
                move |_: StoreContextMut<'_, Host>, (): ()| {
                    collected.notify_one();
                    Ok((0u64,))
                },
            )?;
            Ok(())
        })
        .await?;
        let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
        for index in 0..if dispose { 1 } else { 10 } {
            now.store(1000, Ordering::SeqCst);
            let mut invocation = Box::pin(run.call_async(&mut store, ()));
            tokio::select! {
                result=&mut invocation=>panic!("returned before controlled wait: {result:?}"),
                result=timeout(Duration::from_secs(5),async {entered.notified().await;collected.notified().await;})=>{result?;}
            }
            if dispose {
                drop(invocation);
                break;
            }
            now.store(2_501_000, Ordering::SeqCst);
            finish.notify_one();
            assert_eq!(timeout(Duration::from_secs(5), invocation).await??.0, 2.5);
            assert_eq!(dropped.load(Ordering::SeqCst), index + 1);
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
        drop(store);
        if dispose {
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn supported_random_properties_and_rejections_match_node() -> Result<()> {
    let source = r#"
    export function run():number {
        const bytes=new Uint8Array(4);
        if(crypto.getRandomValues(bytes)!==bytes) {return -1;}
        const view=bytes.subarray(1,3);
        if(crypto.getRandomValues(view)!==view) {return -2;}
        const id=crypto.randomUUID();
        if(id.length!==36) {return -3;}
        if(id[8]!=='-') {return -4;}
        if(id[13]!=='-') {return -5;}
        if(id[18]!=='-') {return -6;}
        if(id[23]!=='-') {return -7;}
        if(id[14]!=='4') {return -8;}
        if('89ab'.indexOf(id.charAt(19))<0) {return -9;}
        const oversized=new Uint8Array(65537);
        oversized[0]=42;
        try {crypto.getRandomValues(oversized);return -10;}
        catch(error) {if(oversized[0]!==42) {return -11;}}
        try {crypto.getRandomValues([1,2,3] as any);return -12;} catch(error) {}
        return 1;
    }"#;
    let (mut store, instance) = instantiate(source, 262144, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    let result = run.call_async(&mut store, ()).await?.0;
    let directory = tempfile::tempdir()?;
    let entry = directory.path().join("random.mts");
    std::fs::write(
        &entry,
        format!("{source}\nprocess.stdout.write(String(run()));"),
    )?;
    let node = std::process::Command::new("node")
        .arg("--disable-warning=ExperimentalWarning")
        .arg(entry)
        .output()?;
    assert!(
        node.status.success(),
        "{}",
        String::from_utf8_lossy(&node.stderr)
    );
    assert_eq!(result, 1.0);
    assert_eq!(String::from_utf8(node.stdout)?, result.to_string());
    store.assert_concurrent_state_empty();
    assert!(store.data().table.is_empty());
    Ok(())
}
