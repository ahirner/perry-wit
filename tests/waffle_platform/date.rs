use super::{Host, instantiate};
use anyhow::Result;
use perry_wit::{compile_typescript_waffle, waffle_backend::WaffleCompileOptions};
use std::{
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    sync::Notify,
    time::{Duration, timeout},
};
use wasmtime::StoreContextMut;
use wasmtime_wasi::p3::bindings::clocks::system_clock::Instant;

#[tokio::test(flavor = "current_thread")]
async fn dates_match_node_for_clipping_calendar_boundaries_and_iso() -> Result<()> {
    let source = r#"
    function copy(date:Date):Date {return new Date(date);}
    export function run(time:number):string {
        const date=new Date(time);
        const alias=date;
        const copied=copy(date);
        if(alias!==date) {throw 99;}
        if(copied===date) {throw 98;}
        if(copied.getTime()!==date.getTime()) {throw 97;}
        if(date.getFullYear()!==date.getUTCFullYear()) {throw 96;}
        if(date.getMonth()!==date.getUTCMonth()) {throw 95;}
        if(date.getDate()!==date.getUTCDate()) {throw 94;}
        if(date.getDay()!==date.getUTCDay()) {throw 93;}
        if(date.getHours()!==date.getUTCHours()) {throw 92;}
        if(date.getMinutes()!==date.getUTCMinutes()) {throw 91;}
        if(date.getSeconds()!==date.getUTCSeconds()) {throw 90;}
        if(date.getMilliseconds()!==date.getUTCMilliseconds()) {throw 89;}
        return copied.toISOString();
    }"#;
    let compiled = compile_typescript_waffle(source, "dates.ts", &WaffleCompileOptions::default())?;
    assert!(!compiled.component_wat.unwrap().contains("wasi:"));
    let (mut store, instance) = instantiate(source, 65536, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(f64,), (String,)>(&mut store, "run")?;
    let mut values = vec![
        0.0,
        -0.0,
        1.9,
        -1.9,
        -1.0,
        -86400000.0,
        86400000.0,
        -62167219200000.0,
        -62167219200001.0,
        253402300799999.0,
        253402300800000.0,
        -8640000000000000.0,
        8640000000000000.0,
        951782400000.0,
        -2203891200000.0,
        4107542400000.0,
    ];
    let mut seed = 7u64;
    for _ in 0..500 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        values.push((seed % 17_280_000_000_000_001) as f64 - 8_640_000_000_000_000.0);
    }
    let node=Command::new("node").args(["--eval", "const v=JSON.parse(process.argv[1]);console.log(JSON.stringify(v.map(t=>new Date(t).toISOString())))", &serde_json::to_string(&values)?]).output()?;
    assert!(
        node.status.success(),
        "{}",
        String::from_utf8_lossy(&node.stderr)
    );
    let expected: Vec<String> = serde_json::from_slice(&node.stdout)?;
    for (time, expected) in values.into_iter().zip(expected) {
        assert_eq!(
            run.call_async(&mut store, (time,)).await?.0,
            expected,
            "timestamp {time}"
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_dates_unwind_and_retained_dates_survive_collection() -> Result<()> {
    let source = r#"
    async function create(time:number):Promise<Date> {return new Date(time);}
    function serialize(date:Date):string {return date.toISOString();}
    export async function run(time:number):Promise<Result<string,number>> {
        const pending=create(time);
        const date=await pending;
        const owner={date:date};
        let index=0;
        while(index<4000) {new Date(index).toISOString(); index=index+1;}
        if(await pending!==date) {throw 99;}
        if(owner.date!==date) {throw 98;}
        try {return serialize(owner.date);}
        catch(error) {
            if(date.getTime()===date.getTime()) {throw 97;}
            if(date.getFullYear()===date.getFullYear()) {throw 96;}
            if(date.getDay()===date.getDay()) {throw 95;}
            if(new Date(date).getTime()===new Date(date).getTime()) {throw 94;}
            throw error+1;
        } finally {
            let cleanup=0;
            while(cleanup<1000) {new Date(cleanup).toISOString(); cleanup=cleanup+1;}
        }
    }"#;
    let (mut store, instance) = instantiate(source, 65536, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(f64,), (Result<String, f64>,)>(&mut store, "run")?;
    for _ in 0..4 {
        for time in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            8640000000000001.0,
            -8640000000000001.0,
        ] {
            assert_eq!(run.call_async(&mut store, (time,)).await?.0, Err(2.0));
            store.assert_concurrent_state_empty();
        }
        assert_eq!(
            run.call_async(&mut store, (-1.0,)).await?.0,
            Ok("1969-12-31T23:59:59.999Z".into())
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn date_constructor_distinguishes_omitted_undefined_and_primitive_values() -> Result<()> {
    let source = r#"
    function effect(state:Uint8Array):number {state[0]=state[0]+1;return -1.9;}
    function empty(state:Uint8Array):void {state[0]=state[0]+1;}
    export function run():number {
        if(new Date(null).getTime()!==0) {throw 99;}
        if(new Date(false).getTime()!==0) {throw 98;}
        if(new Date(true).getTime()!==1) {throw 97;}
        const state=new Uint8Array(1);
        const invalid=new Date(empty(state));
        if(invalid.getTime()===invalid.getTime()) {throw 96;}
        if(new Date(effect(state)).getTime()!==-1) {throw 95;}
        if(state[0]!==2) {throw 94;}
        if(1/new Date(-0.1).getTime()!==1/0) {throw 93;}
        return new Date().valueOf();
    }"#;
    let calls = Arc::new(AtomicUsize::new(0));
    let (mut store, instance) = instantiate(source, 65536, |linker| {
        let calls = calls.clone();
        linker
            .instance("wasi:clocks/system-clock@0.3.0")?
            .func_wrap("now", move |_: StoreContextMut<'_, Host>, (): ()| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok((Instant {
                    seconds: -1,
                    nanoseconds: 999999999,
                },))
            })?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, -1.0);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 20);
    Ok(())
}

#[test]
fn unsupported_date_forms_are_diagnosed_before_frontend_argument_loss() {
    for expression in [
        "new Date(2024,1)",
        "new Date(...[0])",
        "new Date('2024-01-01')",
        "new Date({})",
        "new Date(0).getTime(42)",
        "new Date(0).getTime(...[])",
        "new Date(0).toISOString(42)",
        "new Date(0).setTime(1)",
        "new Date(0).getTime",
        "Date.parse('2024-01-01')",
        "Date.UTC(2024,1)",
    ] {
        let source = format!("export function run():number {{ {expression};return 0;}}");
        assert!(
            compile_typescript_waffle(&source, "unsupported.ts", &WaffleCompileOptions::default())
                .is_err(),
            "{expression}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn utc_calendar_getters_match_node_across_negative_days_and_leap_centuries() -> Result<()> {
    let source = r#"
    export function run(time:number,part:number):number {
        const date=new Date(time);
        if(part===0) {return date.getFullYear();}
        if(part===1) {return date.getMonth();}
        if(part===2) {return date.getDate();}
        if(part===3) {return date.getDay();}
        if(part===4) {return date.getHours();}
        if(part===5) {return date.getMinutes();}
        if(part===6) {return date.getSeconds();}
        return date.getMilliseconds();
    }"#;
    let values = [
        -8640000000000000.0,
        -62167219200001.0,
        -2203977600001.0,
        -2203891200000.0,
        -86400001.0,
        -86400000.0,
        -1.0,
        0.0,
        1.0,
        951782399999.0,
        951782400000.0,
        4107542400000.0,
        8640000000000000.0,
    ];
    let node=Command::new("node").args(["--eval", "console.log(JSON.stringify(JSON.parse(process.argv[1]).map(t=>{const d=new Date(t);return ['FullYear','Month','Date','Day','Hours','Minutes','Seconds','Milliseconds'].map(p=>d['getUTC'+p]())})))",&serde_json::to_string(&values)?]).output()?;
    assert!(node.status.success());
    let expected: Vec<Vec<f64>> = serde_json::from_slice(&node.stdout)?;
    let (mut store, instance) = instantiate(source, 65536, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(f64, f64), (f64,)>(&mut store, "run")?;
    for (time, parts) in values.into_iter().zip(expected) {
        for (part, expected) in parts.into_iter().enumerate() {
            assert_eq!(
                run.call_async(&mut store, (time, part as f64)).await?.0,
                expected,
                "{time}, part {part}"
            );
        }
    }
    for time in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        8640000000000001.0,
    ] {
        for part in 0..8 {
            assert!(
                run.call_async(&mut store, (time, part as f64))
                    .await?
                    .0
                    .is_nan()
            );
        }
    }
    Ok(())
}

#[test]
fn dynamic_date_coercion_keeps_the_general_value_abi_gap_explicit() {
    let source = r#"
    function time(value:any):number {return new Date(value).getTime();}
    export function run():number {return time(null)+time(undefined)+time(false)+time(true);}
    "#;
    assert!(
        compile_typescript_waffle(source, "dynamic-date.ts", &WaffleCompileOptions::default())
            .is_err()
    );
}

#[test]
fn date_helpers_are_retained_for_emitted_functions_without_constructors() -> Result<()> {
    let source = r#"
    function format(values:{[key:string]:Date}):string {return values['date'].toISOString();}
    export function run():number {return 1;}
    "#;
    let compiled = compile_typescript_waffle(
        source,
        "date-dependencies.ts",
        &WaffleCompileOptions::default(),
    )?;
    assert!(!compiled.component_wat.unwrap().contains("wasi:"));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn pending_dates_survive_sibling_collection_and_release_on_disposal() -> Result<()> {
    let source = r#"
    import {waitFor} from 'perry:clocks';
    async function make():Promise<Date> {
        const date=new Date(-1);
        await waitFor(2);
        return date;
    }
    export async function run():Promise<string> {
        const pending=make();
        let index=0;
        while(index<3000) {new Date(index).toISOString();index=index+1;}
        Math.random();
        const date=await pending;
        if(await pending!==date) {throw 99;}
        return date.toISOString();
    }"#;
    struct WaitOwner(Arc<AtomicUsize>);
    impl Drop for WaitOwner {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    for dispose in [false, true] {
        let entered = Arc::new(Notify::new());
        let collected = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicUsize::new(0));
        let (mut store, instance) = instantiate(source, 65536, |linker| {
            let entered = entered.clone();
            let finish = finish.clone();
            let dropped = dropped.clone();
            linker
                .instance("wasi:clocks/monotonic-clock@0.3.0")?
                .func_wrap_concurrent("wait-for", move |_, (duration,): (u64,)| {
                    let entered = entered.clone();
                    let finish = finish.clone();
                    let dropped = dropped.clone();
                    Box::pin(async move {
                        assert_eq!(duration, 2_000_000);
                        let _owner = WaitOwner(dropped);
                        entered.notify_one();
                        finish.notified().await;
                        Ok(())
                    })
                })?;
            let collected = collected.clone();
            linker.instance("wasi:random/random@0.3.0")?.func_wrap(
                "get-random-u64",
                move |_: StoreContextMut<'_, Host>, (): ()| {
                    collected.notify_one();
                    Ok((0u64,))
                },
            )?;
            Ok(())
        })
        .await?;
        let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
        for index in 0..if dispose { 1 } else { 10 } {
            let mut invocation = Box::pin(run.call_async(&mut store, ()));
            tokio::select! {
                result=&mut invocation=>panic!("returned before controlled wait: {result:?}"),
                result=timeout(Duration::from_secs(5),async {entered.notified().await;collected.notified().await;})=>{result?;}
            }
            if dispose {
                drop(invocation);
                break;
            }
            finish.notify_one();
            assert_eq!(
                timeout(Duration::from_secs(5), invocation).await??.0,
                "1969-12-31T23:59:59.999Z"
            );
            assert_eq!(dropped.load(Ordering::SeqCst), index + 1);
            store.assert_concurrent_state_empty();
        }
        drop(store);
        if dispose {
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
        }
    }
    Ok(())
}
