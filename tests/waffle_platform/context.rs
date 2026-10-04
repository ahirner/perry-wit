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

#[tokio::test(flavor = "current_thread")]
async fn arguments_and_cwd_are_cached_across_returns_and_collection() -> Result<()> {
    let source = r#"
    async function saved():Promise<string[]> {return process.argv;}
    export async function run(fail:boolean):Promise<Result<string[],number>> {
        const args=process.argv;
        const pending=saved();
        if(args!==process['argv']) {throw 99;}
        if(args.length!==4) {throw 98;}
        if(args[0]!=='task') {throw 97;}
        if(args[1]!=='é😀') {throw 96;}
        if(args[2]!=='') {throw 95;}
        if(args[3]!=='zero\0byte') {throw 94;}
        if(process.cwd()!=='/é/😀') {throw 93;}
        if(process.cwd().length!==4) {throw 92;}
        let index=0;
        while(index<4000) {new Date(index).toISOString();index=index+1;}
        if(await pending!==args) {throw 91;}
        if(await pending!==process.argv) {throw 90;}
        if(fail) {throw 7;}
        return args;
    }"#;
    let arguments = Arc::new(AtomicUsize::new(0));
    let cwd = Arc::new(AtomicUsize::new(0));
    let expected = vec![
        "task".to_string(),
        "é😀".into(),
        "".into(),
        "zero\0byte".into(),
    ];
    let (mut store, instance) = instantiate(source, 65536, |linker| {
        let mut context = linker.instance("wasi:cli/environment@0.3.0")?;
        let arguments = arguments.clone();
        let values = expected.clone();
        context.func_wrap(
            "get-arguments",
            move |_: StoreContextMut<'_, Host>, (): ()| {
                assert_eq!(arguments.fetch_add(1, Ordering::SeqCst), 0);
                Ok((values.clone(),))
            },
        )?;
        let cwd = cwd.clone();
        context.func_wrap(
            "get-initial-cwd",
            move |_: StoreContextMut<'_, Host>, (): ()| {
                assert_eq!(cwd.fetch_add(1, Ordering::SeqCst), 0);
                Ok((Some("/é/😀".to_string()),))
            },
        )?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(bool,), (Result<Vec<String>, f64>,)>(&mut store, "run")?;
    for index in 0..200 {
        let fail = index % 3 == 0;
        assert_eq!(
            run.call_async(&mut store, (fail,)).await?.0,
            if fail { Err(7.0) } else { Ok(expected.clone()) }
        );
        store.assert_concurrent_state_empty();
    }
    assert_eq!(arguments.load(Ordering::SeqCst), 1);
    assert_eq!(cwd.load(Ordering::SeqCst), 1);
    let directory = tempfile::tempdir()?;
    let script = directory.path().join("context.mts");
    let node_source = source.replace("process.cwd().length", "Array.from(process.cwd()).length");
    std::fs::write(
        &script,
        format!(
            "process.argv={};process.cwd=()=>'/é/😀';{node_source}\nprocess.stdout.write(JSON.stringify(await run(false)));",
            serde_json::to_string(&expected)?
        ),
    )?;
    let node = Command::new("node")
        .arg("--disable-warning=ExperimentalWarning")
        .arg(script)
        .output()?;
    assert!(
        node.status.success(),
        "{}",
        String::from_utf8_lossy(&node.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Vec<String>>(&node.stdout)?,
        expected
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cached_arguments_survive_sibling_collection_and_pending_store_disposal() -> Result<()> {
    let source = r#"
    import {waitFor} from 'perry:clocks';
    async function keep():Promise<string[]> {const args=process.argv;await waitFor(2);return args;}
    export async function run():Promise<string[]> {
        const pending=keep();
        let index=0;while(index<3000) {new Date(index).toISOString();index=index+1;}
        Math.random();
        const args=await pending;
        if(args!==process.argv) {throw 99;}
        return args;
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
        let calls = Arc::new(AtomicUsize::new(0));
        let (mut store, instance) = instantiate(source, 65536, |linker| {
            let calls = calls.clone();
            linker.instance("wasi:cli/environment@0.3.0")?.func_wrap(
                "get-arguments",
                move |_: StoreContextMut<'_, Host>, (): ()| {
                    assert_eq!(calls.fetch_add(1, Ordering::SeqCst), 0);
                    Ok((vec!["kept é😀".to_string()],))
                },
            )?;
            let (entered, finish, dropped) = (entered.clone(), finish.clone(), dropped.clone());
            linker
                .instance("wasi:clocks/monotonic-clock@0.3.0")?
                .func_wrap_concurrent("wait-for", move |_, (duration,): (u64,)| {
                    let (entered, finish, dropped) =
                        (entered.clone(), finish.clone(), dropped.clone());
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
        let run = instance.get_typed_func::<(), (Vec<String>,)>(&mut store, "run")?;
        for _ in 0..if dispose { 1 } else { 10 } {
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
                vec!["kept é😀".to_string()]
            );
            store.assert_concurrent_state_empty();
        }
        drop(store);
        assert_eq!(dropped.load(Ordering::SeqCst), if dispose { 1 } else { 10 });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn context_host_failures_trap_without_running_guest_finally() -> Result<()> {
    for expression in ["process.argv.length", "process.cwd().length"] {
        let source = format!(
            "export function run():number {{try {{return {expression};}} finally {{Math.random();}}}}"
        );
        let cleanup = Arc::new(AtomicUsize::new(0));
        let (mut store, instance) = instantiate(&source, 65536, |linker| {
            let mut context = linker.instance("wasi:cli/environment@0.3.0")?;
            context.func_wrap(
                "get-arguments",
                |_: StoreContextMut<'_, Host>, (): ()| -> wasmtime::Result<(Vec<String>,)> {
                    Err(wasmtime::Error::msg("controlled context failure"))
                },
            )?;
            context.func_wrap(
                "get-initial-cwd",
                |_: StoreContextMut<'_, Host>, (): ()| -> wasmtime::Result<(Option<String>,)> {
                    Err(wasmtime::Error::msg("controlled context failure"))
                },
            )?;
            let cleanup = cleanup.clone();
            linker.instance("wasi:random/random@0.3.0")?.func_wrap(
                "get-random-u64",
                move |_: StoreContextMut<'_, Host>, (): ()| {
                    cleanup.fetch_add(1, Ordering::SeqCst);
                    Ok((0u64,))
                },
            )?;
            Ok(())
        })
        .await?;
        let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
        let error = run.call_async(&mut store, ()).await.unwrap_err();
        assert!(format!("{error:#}").contains("controlled context failure"));
        assert_eq!(cleanup.load(Ordering::SeqCst), 0);
        drop(store);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn selected_context_imports_cover_empty_arguments_and_absent_cwd() -> Result<()> {
    for (source, operation, excluded, cwd, expected) in [
        (
            "export function run():string {if(process.argv.length!==0) {throw 1;}return 'empty';}",
            "get-arguments",
            "get-initial-cwd",
            None,
            "empty",
        ),
        (
            "export function run():string {return process.cwd();}",
            "get-initial-cwd",
            "get-arguments",
            None,
            "/",
        ),
        (
            "export function run():string {return process.cwd();}",
            "get-initial-cwd",
            "get-arguments",
            Some(""),
            "",
        ),
    ] {
        let compiled =
            compile_typescript_waffle(source, "context.ts", &WaffleCompileOptions::default())?;
        let wat = compiled.component_wat.unwrap();
        assert!(wat.contains(operation));
        assert!(!wat.contains(excluded));
        assert!(!wat.contains("get-environment"));
        assert!(!wat.contains("wasi:clocks"));
        let (mut store, instance) = instantiate(source, 65536, |linker| {
            let mut context = linker.instance("wasi:cli/environment@0.3.0")?;
            context.func_wrap("get-arguments", |_: StoreContextMut<'_, Host>, (): ()| {
                Ok((Vec::<String>::new(),))
            })?;
            context.func_wrap(
                "get-initial-cwd",
                move |_: StoreContextMut<'_, Host>, (): ()| Ok((cwd.map(str::to_string),)),
            )?;
            Ok(())
        })
        .await?;
        let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
        for _ in 0..200 {
            assert_eq!(run.call_async(&mut store, ()).await?.0, expected);
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn large_argument_snapshots_reuse_storage_across_component_exports() -> Result<()> {
    let source = r#"
    export function run():string[] {
        const args=process.argv;
        let index=0;
        while(index<3000) {new Date(index).toISOString();index=index+1;}
        return args;
    }"#;
    let expected: Vec<_> = (0..4000)
        .map(|index| format!("{index}:é😀").repeat(16))
        .collect();
    let calls = Arc::new(AtomicUsize::new(0));
    let (mut store, instance) = instantiate(source, 2 * 1024 * 1024, |linker| {
        let arguments = expected.clone();
        let calls = calls.clone();
        linker.instance("wasi:cli/environment@0.3.0")?.func_wrap(
            "get-arguments",
            move |_: StoreContextMut<'_, Host>, (): ()| {
                assert_eq!(calls.fetch_add(1, Ordering::SeqCst), 0);
                Ok((arguments.clone(),))
            },
        )?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(), (Vec<String>,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, expected);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn process_shadows_stay_pure_and_unsupported_context_forms_are_diagnosed() -> Result<()> {
    let source = r#"function process(value:number):number {return value+1;} export function run():number {return process(41);}"#;
    let compiled =
        compile_typescript_waffle(source, "shadow.ts", &WaffleCompileOptions::default())?;
    assert!(!compiled.component_wat.unwrap().contains("wasi:"));
    let (mut store, instance) = instantiate(source, 65536, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, ()).await?.0, 42.0);
    for expression in [
        "process.cwd(1)",
        "process.cwd(...[])",
        "process['c'+'wd']()",
        "process.argv()",
        "process.cwd",
    ] {
        let source = format!("export function run():any {{return {expression};}}");
        assert!(
            compile_typescript_waffle(&source, "invalid.ts", &WaffleCompileOptions::default())
                .is_err(),
            "{expression}"
        );
    }
    Ok(())
}
