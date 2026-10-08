use super::{Host, instantiate};
use crate::waffle_fixture::compile_typescript_waffle;
use anyhow::Result;
use perry_wit::waffle_backend::WaffleCompileOptions;
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

#[test]
fn context_mutation_is_rejected_through_aliases_calls_and_containers() {
    for source in [
        "export function run():void {process.env.VALUE='x';}",
        "export function run():void {process.env.VALUE={};}",
        "export function run():void {delete process.env.VALUE;}",
        "export function run():void {process['env']={};}",
        "export function run():void {(process.env as any).VALUE='x';}",
        "export function run():void {const alias=process.env;alias.VALUE='x';}",
        "export function run():void {let alias:any={};if(Math.random()>0){alias=process.env;}alias.VALUE='x';}",
        "function edit(env:any):void {env.VALUE='x';} export function run():void {edit(process.env);}",
        "function saved():any {return process.env;} export function run():void {saved().VALUE='x';}",
        "function saved(env:any):any {return env;} export function run():void {saved(process.env).VALUE='x';}",
        "export function run():void {const box={env:process.env};box.env.VALUE='x';}",
        "function edit(box:any):void {box.env.VALUE='x';} export function run():void {edit({env:process.env});}",
        "export function run():void {const box:any={};box.env=process.env;box.env.VALUE='x';}",
        "export function run():void {const {env}=process;env.VALUE='x';}",
        "export function run():void {const [env]=[process.env];env.VALUE='x';}",
        "export function run():void {Object.assign(process.env,{VALUE:'x'});}",
        "export function run():void {const copy=Object.assign({}, {env:process.env});copy.env.VALUE='x';}",
        "export function run():void {process.argv[0]='x';}",
        "export function run():void {const args=process.argv;args.length=0;}",
        "export function run():void {delete process.argv[0];}",
        "export function run():void {process.argv.push('x');}",
        "function edit(args:string[]):void {args[0]='x';} export function run():void {edit(process.argv);}",
        "async function saved():Promise<string[]> {return process.argv;} export async function run():Promise<void> {(await saved())[0]='x';}",
        "export function run():void {const arrays=[process.argv];arrays[0][0]='x';}",
        "export function run():void {const box={env:process.env};const copy={...box};copy.env.VALUE='x';}",
        "export function run():void {const box={env:process.env};const env=box?.env;env.VALUE='x';}",
        "export function run():void {process.argv.push?.('x');}",
        "export function run():void {for(const env of [process.env]) {env.VALUE='x';}}",
    ] {
        let error =
            compile_typescript_waffle(source, "readonly.ts", &WaffleCompileOptions::default())
                .unwrap_err();
        assert!(
            error.to_string().contains("read-only"),
            "{source}: {error:#}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn context_reads_allow_helpers_and_independent_mutable_copies() -> Result<()> {
    let source = r#"
        function read(env:{[key:string]:string|undefined}):string {return env.VALUE;}
        function change(object:{VALUE:string}):void {object.VALUE='copy';}
        export function run():string {
            const env=process.env;
            const copy={VALUE:read(env)};
            change(copy);
            return read(env)+':'+copy.VALUE;
        }
    "#;
    let compiled =
        compile_typescript_waffle(source, "readonly.ts", &WaffleCompileOptions::default())?;
    assert!(!compiled.waffle_ir.contains("json_serialize"));
    assert!(!compiled.waffle_ir.contains("value.to-string"));
    let (mut store, instance) = instantiate(source, 65_536, |linker| {
        linker.instance("wasi:cli/environment@0.3.0")?.func_wrap(
            "get-environment",
            |_: StoreContextMut<'_, Host>, (): ()| {
                Ok((vec![("VALUE".to_string(), "original".to_string())],))
            },
        )?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    for _ in 0..100 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, "original:copy");
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn environment_json_snapshots_are_independent_and_keep_cached_values() -> Result<()> {
    let source = r#"
        export function run(first: boolean): string {
            const copy = JSON.parse(JSON.stringify(process.env));
            copy.VALUE = "snapshot";
            for (let i = 0; i < 1000; i++) { JSON.stringify(copy); }
            return JSON.stringify(process.env) + ":" + JSON.stringify(copy);
        }
    "#;
    let calls = Arc::new(AtomicUsize::new(0));
    let (mut store, instance) = instantiate(source, 262_144, |linker| {
        let calls = calls.clone();
        linker.instance("wasi:cli/environment@0.3.0")?.func_wrap(
            "get-environment",
            move |_: StoreContextMut<'_, Host>, (): ()| {
                assert_eq!(calls.fetch_add(1, Ordering::SeqCst), 0);
                Ok((vec![
                    ("VALUE".to_owned(), "initial".to_owned()),
                    ("UNICODE".to_owned(), "é😀\0".to_owned()),
                ],))
            },
        )?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(bool,), (String,)>(&mut store, "run")?;
    for index in 0..20 {
        assert_eq!(
            run.call_async(&mut store, (index == 0,)).await?.0,
            r#"{"VALUE":"initial","UNICODE":"é😀\u0000"}:{"VALUE":"snapshot","UNICODE":"é😀\u0000"}"#
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn nonnumeric_dictionary_keys_match_node_and_return_independent_snapshots() -> Result<()> {
    let source = r#"
    function build():{[key:string]:string} {
        return {b:'before','key10':'ten','key2':'two','key01':'leading','key4294967294':'large','key4294967295':'ordinary','😀':'unicode','key0':'zero'};
    }
    export function run(mode:number):string[] {
        const object=build();
        const before=Object.values(object);
        const keys=Object.keys(object);
        if(!('key2' in object)) {throw 99;}
        if('missing' in object) {throw 98;}
        Object.assign(object,{'key2':'discarded'},null,{'key2':'changed'});
        Object.assign(object,undefined,{b:'after'});
        let index=0;while(index<3000) {const other={value:'é'+'😀'};index=index+1;}
        if(mode===0) {return keys;}
        if(mode===1) {return before;}
        if(mode===2) {return Object.keys(object);}
        return Object.values(object);
    }"#;
    let directory = tempfile::tempdir()?;
    let script = directory.path().join("enumeration.mts");
    std::fs::write(
        &script,
        format!("{source}\nprocess.stdout.write(JSON.stringify([0,1,2,3].map(run)));"),
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
    let expected: Vec<Vec<String>> = serde_json::from_slice(&node.stdout)?;
    let (mut store, instance) = instantiate(source, 65536, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(f64,), (Vec<String>,)>(&mut store, "run")?;
    for _ in 0..20 {
        for (mode, expected) in expected.iter().enumerate() {
            assert_eq!(
                run.call_async(&mut store, (mode as f64,)).await?.0,
                *expected
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn numeric_dictionary_keys_keep_insertion_order_through_assignment_and_collection()
-> Result<()> {
    let source = r#"
    export function run(keys:boolean):string[] {
        const values:{[key:string]:string}={'10':'ten','2':'two','0':'zero',word:'word','01':'leading','4294967294':'large','4294967295':'outside','😀':'unicode'};
        Object.assign(values,values,{'2':'changed','1':'last'});
        const names=Object.keys(values);const items=Object.values(values);
        let index=0;while(index<3000) {const temporary={value:'é'+'😀'};index=index+1;}
        if(keys) {return names;} return items;
    }"#;
    let (mut store, instance) = instantiate(source, 65536, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(bool,), (Vec<String>,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(
            run.call_async(&mut store, (true,)).await?.0,
            [
                "10",
                "2",
                "0",
                "word",
                "01",
                "4294967294",
                "4294967295",
                "😀",
                "1"
            ]
        );
        assert_eq!(
            run.call_async(&mut store, (false,)).await?.0,
            [
                "ten", "changed", "zero", "word", "leading", "large", "outside", "unicode", "last"
            ]
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn enumeration_and_membership_preserve_argument_effects_and_checked_values() -> Result<()> {
    let source = r#"
    function key(state:{trace:string}):string {state.trace=state.trace+'k';return 'va'+'lue';}
    function select(state:{trace:string},object:{value:string}):{value:string} {
        let index=0;while(index<3000) {const other={text:'é'+'😀'};index=index+1;}
        state.trace=state.trace+'o';return object;
    }
    export function run():Result<string,number> {
        const state={trace:''};const object={value:'kept é😀'};
        const keys=Object.keys(select(state,object));
        if(keys[0]!=='value') {throw 99;}
        if(!(key(state) in select(state,object))) {throw 98;}
        if(state.trace!=='oko') {throw 97;}
        const mixed={value:3, missing:undefined};
        if(Object.keys(mixed).length!==2) {throw 96;}
        try {Object.values(mixed);throw 95;}
        catch(error) {if(error.code !== 12) {throw error;}}
        finally {state.trace=state.trace+'f';}
        return state.trace;
    }"#;
    let (mut store, instance) = instantiate(source, 65536, |_| Ok(())).await?;
    let run = instance.get_typed_func::<(), (Result<String, f64>,)>(&mut store, "run")?;
    for _ in 0..30 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, Ok("okof".into()));
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn environment_snapshots_preserve_native_strings_and_duplicate_keys() -> Result<()> {
    let source = r#"
    export function run(keys:boolean):string[] {
        const env=process.env;
        const names=Object.keys(env);const values=Object.values(env);
        let index=0;
        while(index<1000) {const temporary={value:'prefix'+'😀'};index=index+1;}
        if(keys) {return names;}
        return values;
    }"#;
    let mut entries: Vec<_> = (0..64)
        .map(|index| (format!("key{index}"), format!("{index}:é😀\0").repeat(4)))
        .collect();
    let mut expected_values: Vec<_> = entries.iter().map(|(_, value)| value.clone()).collect();
    let expected_keys: Vec<_> = entries.iter().map(|(key, _)| key.clone()).collect();
    expected_values[17] = "replacement\0é😀".into();
    entries.push(("key17".into(), expected_values[17].clone()));
    let calls = Arc::new(AtomicUsize::new(0));
    let (mut store, instance) = instantiate(source, 65_536, |linker| {
        let calls = calls.clone();
        linker.instance("wasi:cli/environment@0.3.0")?.func_wrap(
            "get-environment",
            move |_: StoreContextMut<'_, Host>, (): ()| {
                assert_eq!(calls.fetch_add(1, Ordering::SeqCst), 0);
                Ok((entries.clone(),))
            },
        )?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(bool,), (Vec<String>,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(run.call_async(&mut store, (true,)).await?.0, expected_keys);
        assert_eq!(
            run.call_async(&mut store, (false,)).await?.0,
            expected_values
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

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
        if(new TextEncoder().encode(process.cwd()).length!==8) {throw 92;}
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
    std::fs::write(
        &script,
        format!(
            "process.argv={};process.cwd=()=>'/é/😀';{source}\nprocess.stdout.write(JSON.stringify(await run(false)));",
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
async fn cached_context_survives_sibling_collection_and_pending_store_disposal() -> Result<()> {
    let arguments = r#"
    import {setTimeout as waitFor} from 'node:timers/promises';
    async function keep():Promise<string[]> {const args=process.argv;await waitFor(2);return args;}
    export async function run():Promise<string[]> {
        const pending=keep();
        let index=0;while(index<3000) {new Date(index).toISOString();index=index+1;}
        Math.random();
        const args=await pending;
        if(args!==process.argv) {throw 99;}
        return args;
    }"#;
    let environment = r#"
    import {setTimeout as waitFor} from 'node:timers/promises';
    async function keep():Promise<{[key:string]:string|undefined}> {
        const env=process.env;const before=env.OLD;const keys=Object.keys(env);
        await waitFor(2);
        if(before!=='kept é😀') {throw 99;}
        if(keys[0]!=='OLD') {throw 98;}
        return env;
    }
    export async function run():Promise<string[]> {
        const pending=keep();
        let index=0;while(index<3000) {new Date(index).toISOString();index=index+1;}
        Math.random();
        const env=await pending;
        if(env!==process.env) {throw 97;}
        return Object.values(env);
    }"#;
    struct WaitOwner(Arc<AtomicUsize>);
    impl Drop for WaitOwner {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    for (source, dispose) in [
        (arguments, false),
        (arguments, true),
        (environment, false),
        (environment, true),
    ] {
        let entered = Arc::new(Notify::new());
        let collected = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let memory_limit = 65_536;
        let (mut store, instance) = instantiate(source, memory_limit, |linker| {
            let argument_calls = calls.clone();
            linker.instance("wasi:cli/environment@0.3.0")?.func_wrap(
                "get-arguments",
                move |_: StoreContextMut<'_, Host>, (): ()| {
                    assert_eq!(argument_calls.fetch_add(1, Ordering::SeqCst), 0);
                    Ok((vec!["kept é😀".to_string()],))
                },
            )?;
            let calls = calls.clone();
            linker.instance("wasi:cli/environment@0.3.0")?.func_wrap(
                "get-environment",
                move |_: StoreContextMut<'_, Host>, (): ()| {
                    assert_eq!(calls.fetch_add(1, Ordering::SeqCst), 0);
                    Ok((vec![("OLD".to_string(), "kept é😀".to_string())],))
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
    for expression in [
        "process.argv.length",
        "process.cwd().length",
        "Object.keys(process.env).length",
    ] {
        let source = format!(
            "export function run():number {{try {{return {expression};}} finally {{Math.random();}}}}"
        );
        let cleanup = Arc::new(AtomicUsize::new(0));
        let memory_limit = 65_536;
        let (mut store, instance) = instantiate(&source, memory_limit, |linker| {
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
            context.func_wrap(
                "get-environment",
                |_: StoreContextMut<'_, Host>,
                 (): ()|
                 -> wasmtime::Result<(Vec<(String, String)>,)> {
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
async fn selected_context_imports_cover_empty_snapshots_and_absent_cwd() -> Result<()> {
    for (source, operation, cwd, expected) in [
        (
            "export function run():string {if(process.argv.length!==0) {throw 1;}return 'empty';}",
            "get-arguments",
            None,
            "empty",
        ),
        (
            "export function run():string {return process.cwd();}",
            "get-initial-cwd",
            None,
            "/",
        ),
        (
            "export function run():string {return process.cwd();}",
            "get-initial-cwd",
            Some(""),
            "",
        ),
        (
            "export function run():string {if(Object.keys(process.env).length!==0) {throw 1;}if(Object.values(process.env).length!==0) {throw 2;}return 'empty';}",
            "get-environment",
            None,
            "empty",
        ),
    ] {
        let compiled =
            compile_typescript_waffle(source, "context.ts", &WaffleCompileOptions::default())?;
        let wat = compiled.component_wat.unwrap();
        assert!(wat.contains(operation));
        for candidate in ["get-arguments", "get-initial-cwd", "get-environment"] {
            assert_eq!(wat.contains(candidate), candidate == operation);
        }
        assert!(!wat.contains("wasi:clocks"));
        let memory_limit = 65_536;
        let (mut store, instance) = instantiate(source, memory_limit, |linker| {
            let mut context = linker.instance("wasi:cli/environment@0.3.0")?;
            context.func_wrap("get-arguments", |_: StoreContextMut<'_, Host>, (): ()| {
                Ok((Vec::<String>::new(),))
            })?;
            context.func_wrap("get-environment", |_: StoreContextMut<'_, Host>, (): ()| {
                Ok((Vec::<(String, String)>::new(),))
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
    let source = r#"function process(value:number):number {return value+1;} function Object(value:number):number {return value+1;} export function run():number {return Object(process(40));}"#;
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
        "process.env()",
        "Object.keys({},Math.random())",
        "Object.values()",
        "Object.keys(...[{}])",
        "Object.assign()",
        "Object.assign({},...[{}])",
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
