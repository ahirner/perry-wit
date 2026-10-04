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
async fn environment_number_coercions_match_node_and_reclaim_temporary_storage() -> Result<()> {
    let source = r#"
        function write(value: any): any { return process.env.VALUE = value; }
        export function run(value: number): string {
            for (let index = 0; index < 20; index++) { write(value); }
            return process.env.VALUE;
        }
    "#;
    let mut values = vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        1e-7,
        1e-6,
        1e20,
        1e21,
        f64::MAX,
        f64::MIN,
        f64::MIN_POSITIVE,
        f64::from_bits(1),
        9007199254740991.0,
        0.1,
    ];
    let mut bits = 7u64;
    for _ in 0..300 {
        bits = bits.wrapping_mul(6364136223846793005).wrapping_add(1);
        let value = f64::from_bits(bits);
        if value.is_finite() {
            values.push(value);
        }
    }
    let node = Command::new("node")
        .args([
            "-e",
            &format!(
                "process.stdout.write(JSON.stringify({}.map(String)))",
                serde_json::to_string(&values)?
            ),
        ])
        .output()?;
    assert!(
        node.status.success(),
        "{}",
        String::from_utf8_lossy(&node.stderr)
    );
    let expected: Vec<String> = serde_json::from_slice(&node.stdout)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let (mut store, instance) = instantiate(source, 262_144, |linker| {
        let calls = calls.clone();
        linker.instance("wasi:cli/environment@0.3.0")?.func_wrap(
            "get-environment",
            move |_: StoreContextMut<'_, Host>, (): ()| {
                assert_eq!(calls.fetch_add(1, Ordering::SeqCst), 0);
                Ok((Vec::<(String, String)>::new(),))
            },
        )?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(f64,), (String,)>(&mut store, "run")?;
    for (value, expected) in values.into_iter().zip(expected) {
        assert_eq!(
            run.call_async(&mut store, (value,)).await?.0,
            expected,
            "{value:?}"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn environment_nested_and_cyclic_arrays_match_node() -> Result<()> {
    let source = r#"
        function text(value: any): string {
            process.env.PERRY_TEST_VALUE = value;
            return process.env.PERRY_TEST_VALUE;
        }
        export function run(): string {
            const shared = ["é😀", null, undefined, true, 1e21];
            const cycle = ["start", undefined, "end"];
            cycle[1] = cycle;
            const root = [shared, shared, cycle, {}, [], -0, 0/0, 1/0, -1/0];
            root.length = 12;
            const first = text(root);
            const keys = text(Object.keys({b:"2", a:"1"}));
            delete shared[0];
            shared[3] = "changed";
            for (let i=0; i<1000; i++) {text(root);}
            return first + ":" + keys + ":" + text(root);
        }
    "#;
    let directory = tempfile::tempdir()?;
    let script = directory.path().join("coercion.mts");
    std::fs::write(
        &script,
        format!("{source}\nprocess.stdout.write(JSON.stringify(run()));"),
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
    let expected: String = serde_json::from_slice(&node.stdout)?;
    let (mut store, instance) = instantiate(source, 262_144, |linker| {
        linker
            .instance("wasi:cli/environment@0.3.0")?
            .func_wrap("get-environment", |_: StoreContextMut<'_, Host>, (): ()| {
                Ok((Vec::<(String, String)>::new(),))
            })?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, expected);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn environment_coercion_failures_preserve_values_and_allocating_cleanup() -> Result<()> {
    let source = r#"
        function write(value:any):any {return process.env.VALUE=value;}
        export function run():Result<string,number> {
            process.env.VALUE="unchanged";
            let deep:any=[];
            for(let i=0;i<128;i++) {deep=[deep];}
            let cleaned=0;
            for(let i=0;i<1000;i++) {
                try {write(["prefix",new Uint8Array(1)]);throw 99;}
                catch(error) {if(error!==12) {throw error;}}
                finally {process.env.TEMP=["😀", i];cleaned=cleaned+1;}
                try {write(deep);throw 98;}
                catch(error) {if(error!==3) {throw error;}}
                if(process.env.VALUE!=="unchanged") {throw 97;}
            }
            if(cleaned!==1000) {throw 96;}
            return process.env.VALUE+":"+process.env.TEMP;
        }
    "#;
    let (mut store, instance) = instantiate(source, 262_144, |linker| {
        linker
            .instance("wasi:cli/environment@0.3.0")?
            .func_wrap("get-environment", |_: StoreContextMut<'_, Host>, (): ()| {
                Ok((Vec::<(String, String)>::new(),))
            })?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(), (Result<String, f64>,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(
            run.call_async(&mut store, ()).await?.0,
            Ok("unchanged:😀,999".into())
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn environment_large_array_coercions_reuse_measured_output_storage() -> Result<()> {
    let source = r#"
        export function run(value:string):string {
            const array=[value,value,value,value];
            for(let i=0;i<20;i++) {process.env.VALUE=array;}
            return process.env.VALUE;
        }
    "#;
    let (mut store, instance) = instantiate(source, 2 * 1024 * 1024, |linker| {
        linker
            .instance("wasi:cli/environment@0.3.0")?
            .func_wrap("get-environment", |_: StoreContextMut<'_, Host>, (): ()| {
                Ok((Vec::<(String, String)>::new(),))
            })?;
        Ok(())
    })
    .await?;
    let input = "é😀\0".repeat(20_000);
    let expected = [input.as_str(); 4].join(",");
    let run = instance.get_typed_func::<(String,), (String,)>(&mut store, "run")?;
    for _ in 0..10 {
        assert_eq!(
            run.call_async(&mut store, (input.clone(),)).await?.0,
            expected
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn environment_json_snapshots_are_independent_and_keep_instance_mutations() -> Result<()> {
    let source = r#"
        export function run(first: boolean): string {
            if (first) { process.env.VALUE = "changed"; }
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
            r#"{"VALUE":"changed","UNICODE":"é😀\u0000"}:{"VALUE":"snapshot","UNICODE":"é😀\u0000"}"#
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn environment_aliases_preserve_mutation_deletion_and_supported_coercions() -> Result<()> {
    let source = r#"
    function write(env:{[key:string]:string|undefined},key:string,value:any):any {return env[key]=value;}
    export function run(first:boolean):Result<string,number> {
        const env=process.env;
        if(env!==process['env']) {throw 99;}
        if(env.BASE!=='é😀') {throw 98;}
        if(env.EMPTY!=='') {throw 97;}
        if(env.MISSING!==undefined) {throw 96;}
        if(first) {if(env.KEPT!==undefined) {throw 95;}}
        else {if(env.KEPT!=='changed') {throw 94;}}
        if(write(env,'FLAG',true)!==true) {throw 93;}
        if(env.FLAG!=='true') {throw 92;}
        write(env,'FLAG',false);if(env.FLAG!=='false') {throw 91;}
        write(env,'FLAG',null);if(env.FLAG!=='null') {throw 90;}
        write(env,'FLAG',undefined);if(env.FLAG!=='undefined') {throw 89;}
        write(env,'FLAG',0/0);if(env.FLAG!=='NaN') {throw 88;}
        write(env,'FLAG',1/0);if(env.FLAG!=='Infinity') {throw 87;}
        write(env,'FLAG',-1/0);if(env.FLAG!=='-Infinity') {throw 86;}
        write(env,'FLAG',{});if(env.FLAG!=='[object Object]') {throw 85;}
        write(env,'FLAG',42);if(env.FLAG!=='42') {throw 84;}
        write(env,'FLAG',Object.keys(env));
        if(first) {if(env.FLAG!=='BASE,EMPTY,FLAG') {throw 83;}}
        try {write(env,'FLAG',new Uint8Array(1));throw 79;}
        catch(error) {if(error!==12) {throw error;}}
        if(first) {if(env.FLAG!=='BASE,EMPTY,FLAG') {throw 78;}}
        let index=0;
        while(index<3000) {env.TEMP='prefix'+'😀';delete env.TEMP;index=index+1;}
        if(env.TEMP!==undefined) {throw 82;}
        env.KEPT='changed';
        const before=env.BASE;
        process.env.BASE='next';
        if(before!=='é😀') {throw 81;}
        env.BASE='é😀';
        delete env.FLAG;
        if(env.FLAG!==undefined) {throw 80;}
        return env.KEPT;
    }"#;
    let calls = Arc::new(AtomicUsize::new(0));
    let (mut store, instance) = instantiate(source, 262_144, |linker| {
        let calls = calls.clone();
        linker.instance("wasi:cli/environment@0.3.0")?.func_wrap(
            "get-environment",
            move |_: StoreContextMut<'_, Host>, (): ()| {
                assert_eq!(calls.fetch_add(1, Ordering::SeqCst), 0);
                Ok((vec![
                    ("BASE".to_string(), "é😀".to_string()),
                    ("EMPTY".into(), "".into()),
                ],))
            },
        )?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(bool,), (Result<String, f64>,)>(&mut store, "run")?;
    for index in 0..100 {
        assert_eq!(
            run.call_async(&mut store, (index == 0,)).await?.0,
            Ok("changed".into())
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn object_enumeration_matches_node_and_returns_independent_snapshots() -> Result<()> {
    let source = r#"
    function build():{[key:string]:string} {
        return {b:'before', '10':'ten','2':'two','01':'leading','4294967294':'last index','4294967295':'ordinary','😀':'unicode','0':'zero'};
    }
    export function run(mode:number):string[] {
        const object=build();
        const before=Object.values(object);
        const keys=Object.keys(object);
        if(!('2' in object)) {throw 99;}
        delete object['2'];
        if('2' in object) {throw 98;}
        Object.assign(object,{'2':'discarded'},null,{'2':'changed'});
        delete object.b;
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
        catch(error) {if(error!==12) {throw error;}}
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
async fn object_assign_preserves_environment_policy_order_and_partial_failure() -> Result<()> {
    let source = r#"
    function later(env:{[key:string]:string|undefined}):{FLAG:boolean} {
        if(env.FIRST!==undefined) {throw 99;}
        let index=0;while(index<3000) {const other={text:'é'+'😀'};index=index+1;}
        return {FLAG:true};
    }
    export function run():Result<string,number> {
        const env=process.env;
        delete env.FIRST;delete env.FLAG;delete env['1'];
        if(Object.assign(env)!==env) {throw 98;}
        if(Object.assign(env,{FIRST:'é'+'😀'},null,undefined,later(env))!==env) {throw 97;}
        if(env.FIRST!=='é😀') {throw 96;}
        if(env.FLAG!=='true') {throw 95;}
        env['2']='unchanged';
        try {Object.assign(env,{'10':'late','2':new Uint8Array(1),'1':false});throw 94;}
        catch(error) {if(error!==12) {throw error;}}
        if(env['1']!=='false') {throw 93;}
        if(env['2']!=='unchanged') {throw 92;}
        if(env['10']!==undefined) {throw 91;}
        if(Object.assign(env,env)!==env) {throw 90;}
        const ordinary={value:0};
        if(Object.assign(ordinary,{value:42})!==ordinary) {throw 89;}
        if(ordinary.value!==42) {throw 88;}
        let index=0;while(index<3000) {const other={text:'é'+'😀'};index=index+1;}
        return env.FIRST;
    }"#;
    let (mut store, instance) = instantiate(source, 262_144, |linker| {
        linker
            .instance("wasi:cli/environment@0.3.0")?
            .func_wrap("get-environment", |_: StoreContextMut<'_, Host>, (): ()| {
                Ok((Vec::<(String, String)>::new(),))
            })?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(), (Result<String, f64>,)>(&mut store, "run")?;
    for _ in 0..60 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, Ok("é😀".into()));
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
        while(index<1000) {env.TEMP='prefix'+'😀';delete env.TEMP;index=index+1;}
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
    let (mut store, instance) = instantiate(source, 262_144, |linker| {
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
async fn cached_context_survives_sibling_collection_and_pending_store_disposal() -> Result<()> {
    let arguments = r#"
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
    let environment = r#"
    import {waitFor} from 'perry:clocks';
    async function keep():Promise<{[key:string]:string|undefined}> {
        const env=process.env;const before=env.OLD;const keys=Object.keys(env);
        await waitFor(2);
        if(before!=='kept é😀') {throw 99;}
        if(keys[0]!=='OLD') {throw 98;}
        return env;
    }
    export async function run():Promise<string[]> {
        process.env.OLD=['kept é😀'];delete process.env.CURRENT;
        const pending=keep();
        delete process.env.OLD;process.env.CURRENT=['kept é😀'];
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
        let memory_limit = if source == environment {
            262_144
        } else {
            65_536
        };
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
                    Ok((Vec::<(String, String)>::new(),))
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
        let memory_limit = if expression.contains("process.env") {
            262_144
        } else {
            65_536
        };
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
        let memory_limit = if operation == "get-environment" {
            262_144
        } else {
            65_536
        };
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
