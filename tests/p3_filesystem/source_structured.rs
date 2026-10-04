use super::instantiate;
use anyhow::Result;
use std::fs;
use wasmtime::component::{ComponentType, Lift, Lower};
use wasmtime_wasi::{FsPerms, WasiCtxBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ComponentType, Lift, Lower)]
#[component(enum)]
#[repr(u8)]
enum StatsKind {
    #[component(name = "block-device")]
    BlockDevice,
    #[component(name = "character-device")]
    CharacterDevice,
    #[component(name = "directory")]
    Directory,
    #[component(name = "fifo")]
    Fifo,
    #[component(name = "symbolic-link")]
    SymbolicLink,
    #[component(name = "regular-file")]
    RegularFile,
    #[component(name = "socket")]
    Socket,
    #[component(name = "other")]
    Other,
}

#[derive(Debug, Clone, PartialEq, ComponentType, Lift, Lower)]
#[component(record)]
struct Stats {
    size: f64,
    #[component(name = "mtime-ms")]
    mtime_ms: f64,
    kind: StatsKind,
}

#[tokio::test(flavor = "current_thread")]
async fn string_arrays_roundtrip_component_boundaries_and_retained_outcomes() -> Result<()> {
    for stored in [false, true] {
        let source = if stored {
            r#"
        async function retain(value:string[]):Promise<string[]> {return value;}
        export async function run(value:string[]):Promise<string[]> {
            const pending=retain(value);
            const first=await pending;
            let index=0;
            while(index<2000) {const temporary=new Uint8Array(256);index=index+1;}
            const second=await pending;
            if(first!==second) {throw 1;}
            return second;
        }"#
        } else {
            r#"
        function retain(value:string[]):string[] {return value;}
        export function run(value:string[]):string[] {
            const holder={value:retain(value)};
            let index=0;
            while(index<2000) {const temporary=new Uint8Array(256);index=index+1;}
            return holder.value;
        }"#
        };
        let (mut store, instance) = instantiate(source, WasiCtxBuilder::new().build()).await?;
        let run = instance.get_typed_func::<(Vec<String>,), (Vec<String>,)>(&mut store, "run")?;
        for _ in 0..10 {
            for values in [
                vec![],
                vec![String::new()],
                vec!["é\0😀".into(), "中".into(), String::new()],
                (0..80).map(|index| format!("entry-{index}😀")).collect(),
            ] {
                assert_eq!(
                    run.call_async(&mut store, (values.clone(),)).await?.0,
                    values
                );
                store.assert_concurrent_state_empty();
                assert!(store.data().table.is_empty());
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn stats_roundtrip_component_records_and_retained_outcomes_without_filesystem_imports()
-> Result<()> {
    for stored in [false, true] {
        let source = if stored {
            r#"
        async function retain(value:Stats):Promise<Stats> {return value;}
        export async function run(value:Stats):Promise<Stats> {
            const pending=retain(value);
            const first=await pending;
            let index=0;
            while(index<2000) {const temporary=new Uint8Array(256);index=index+1;}
            if(first!==await pending) {throw 1;}
            return await pending;
        }"#
        } else {
            r#"
        export function run(value:Stats):Stats {
            const holder={value};
            let index=0;
            while(index<2000) {const temporary=new Uint8Array(256);index=index+1;}
            return holder.value;
        }"#
        };
        let compiled = perry_wit::compile_typescript_waffle(
            source,
            "stats.ts",
            &perry_wit::waffle_backend::WaffleCompileOptions::default(),
        )?;
        assert!(!compiled.component_wat.unwrap().contains("wasi:filesystem"));
        let (mut store, instance) = instantiate(source, WasiCtxBuilder::new().build()).await?;
        let run = instance.get_typed_func::<(Stats,), (Stats,)>(&mut store, "run")?;
        for _ in 0..10 {
            for kind in [
                StatsKind::BlockDevice,
                StatsKind::CharacterDevice,
                StatsKind::Directory,
                StatsKind::Fifo,
                StatsKind::SymbolicLink,
                StatsKind::RegularFile,
                StatsKind::Socket,
                StatsKind::Other,
            ] {
                let value = Stats {
                    size: -0.0,
                    mtime_ms: -1234.125,
                    kind,
                };
                let actual = run.call_async(&mut store, (value.clone(),)).await?.0;
                assert_eq!(actual, value);
                assert_eq!(actual.size.to_bits(), value.size.to_bits());
                store.assert_concurrent_state_empty();
                assert!(store.data().table.is_empty());
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn filesystem_structured_results_preserve_values_errors_and_finally_through_component_abi()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("é😀"), "é😀")?;
    for stored in [false, true] {
        for arrays in [false, true] {
            let ty = if arrays { "string[]" } else { "Stats" };
            let call = if arrays {
                "fs.readdirSync(path)"
            } else {
                "fs.statSync(path)"
            };
            let source = if stored {
                format!(
                    r#"
                import fs from 'fs';
                async function read(path:string):Promise<{ty}> {{return {call};}}
                export async function run(path:string,fail:boolean):Promise<Result<{ty},number>> {{
                    const pending=read(path);
                    try {{
                        const result=await pending;
                        let index=0;
                        while(index<2000) {{const temporary=new Uint8Array(256);index=index+1;}}
                        if (result!==await pending) {{throw 99;}}
                        if(fail) {{throw 123;}}
                        return result;
                    }} finally {{let index=0;while(index<2000) {{const temporary=new Uint8Array(256);index=index+1;}}}}
                }}"#
                )
            } else {
                format!(
                    r#"
                import fs from 'fs';
                export function run(path:string,fail:boolean):Result<{ty},number> {{
                    try {{const result={call};if(fail) {{throw 123;}}return result;}}
                    finally {{let index=0;while(index<2000) {{const temporary=new Uint8Array(256);index=index+1;}}}}
                }}"#
                )
            };
            let context = WasiCtxBuilder::new()
                .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
                .build();
            let (mut store, instance) = instantiate(&source, context).await?;
            for _ in 0..10 {
                for failure in [0, 1, 2] {
                    let path = if failure == 2 {
                        "/sandbox/missing"
                    } else if arrays {
                        "/sandbox"
                    } else {
                        "/sandbox/é😀"
                    };
                    if arrays {
                        let run = instance
                            .get_typed_func::<(String, bool), (Result<Vec<String>, f64>,)>(
                                &mut store, "run",
                            )?;
                        assert_eq!(
                            run.call_async(&mut store, (path.into(), failure == 1))
                                .await?
                                .0,
                            match failure {
                                1 => Err(123.0),
                                2 => Err(20.0),
                                _ => Ok(vec!["é😀".into()]),
                            }
                        );
                    } else {
                        let run = instance
                            .get_typed_func::<(String, bool), (Result<Stats, f64>,)>(
                                &mut store, "run",
                            )?;
                        match (
                            failure,
                            run.call_async(&mut store, (path.into(), failure == 1))
                                .await?
                                .0,
                        ) {
                            (0, Ok(stats)) => {
                                assert_eq!(stats.size, 6.0);
                                assert_eq!(stats.kind, StatsKind::RegularFile);
                                assert!(stats.mtime_ms > 0.0);
                            }
                            (1, Err(error)) => assert_eq!(error, 123.0),
                            (2, Err(error)) => assert_eq!(error, 20.0),
                            (failure, result) => panic!("failure={failure} result={result:?}"),
                        }
                    }
                    store.assert_concurrent_state_empty();
                    assert!(store.data().table.is_empty());
                }
            }
        }
    }
    Ok(())
}
