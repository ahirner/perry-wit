//! Native filesystem operations retain their arguments and result storage across callbacks.
use anyhow::Result;
use perry_wit::{compile_for_world, component::wit::resolve_wit};
use wasmtime::{
    Config, Engine, ResourceLimiter, Store,
    component::{Component, Linker, ResourceTable},
};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};

struct Host {
    ctx: WasiCtx,
    table: ResourceTable,
    peak: usize,
}
impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}
impl ResourceLimiter for Host {
    fn memory_growing(
        &mut self,
        _: usize,
        desired: usize,
        _: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.peak = self.peak.max(desired);
        Ok(true)
    }
    fn table_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}
fn compile(source: &str, exports: &str) -> Result<Vec<u8>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("world.wit"),
        format!(
            "package test:filesystem; world filesystem {{ import wasi:filesystem/types@0.3.0; import wasi:filesystem/preopens@0.3.0; {exports} }}"
        ),
    )?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("filesystem"))?;
    let compiled = compile_for_world(source, "filesystem.ts", &resolve, world, true)?;
    Ok(compiled.stripped.unwrap())
}

const SOURCE: &str = r#"
import fs from 'node:fs/promises';
import {stat as metadata} from 'fs/promises';
export async function run(path:string):Promise<number[]> {
  const first=metadata(path+'/input');
  const directory=fs.mkdir(path+'/child');
  const result:number[]=[];
  const before=await first;
  await directory;
  const info=await metadata(path+'/./child/..//input');
  if(info.size!==before.size||!info.isFile()||info.isDirectory())throw new Error('bad file stat');
  result.push(info.size);result.push(info.mtimeMs);
  const child=await fs.stat(path+'/child');
  if(!child.isDirectory()||child.isFile())throw new Error('bad directory stat');
  await fs.rmdir(path+'/child');
  let errors=0;
  const missing=metadata(path+'/missing');
  try{await missing;}catch(e){if(e===20)errors++;}
  try{await missing;}catch(e){if(e===20)errors++;}
  try{await fs.mkdir(path+'/input');}catch(e){if(e===7)errors++;}
  try{await fs.rmdir(path+'/input');}catch(e){if(e===24)errors++;}
  try{await metadata('/outside/input');}catch(e){if(e===1)errors++;}
  try{await metadata(path+'/bad\0name');}catch(e){if(e===12)errors++;}
  result.push(errors);
  return result;
}
export async function remove(path:string):Promise<void>{await fs.unlink(path);}
"#;

#[tokio::test(flavor = "current_thread")]
async fn metadata_and_directory_lifecycle_share_safe_native_task_ownership() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(
        &engine,
        compile(
            SOURCE,
            "export run: async func(path:string) -> list<f64>; export remove: async func(path:string);",
        )?,
    )?;
    let root = tempfile::tempdir()?;
    for (name, data) in [("a", b"alpha".as_slice()), ("b", b"beta".as_slice())] {
        std::fs::create_dir(root.path().join(name))?;
        std::fs::write(root.path().join(name).join("input"), data)?;
    }
    let ctx = WasiCtx::builder()
        .preopened_dir(root.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let mut store = Store::new(
        &engine,
        Host {
            ctx,
            table: ResourceTable::new(),
            peak: 0,
        },
    );
    store.limiter(|host| host);
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (Vec<f64>,)>(&mut store, "run")?;
    let remove = instance.get_typed_func::<(&str,), ()>(&mut store, "remove")?;
    let mut peak = 0;
    for round in 0..12 {
        let (a, b) = store
            .run_concurrent(async |accessor| -> wasmtime::Result<_> {
                let (a, b) = tokio::join!(
                    run.call_concurrent(accessor, ("/sandbox/a",)),
                    run.call_concurrent(accessor, ("/sandbox/b",))
                );
                Ok((a?.0, b?.0))
            })
            .await??;
        assert_eq!(a[0], 5.0);
        assert_eq!(b[0], 4.0);
        assert!(a[1] > 0.0 && b[1] > 0.0);
        assert_eq!(a[2], 6.0);
        assert_eq!(b[2], 6.0);
        assert!(store.data().table.is_empty(), "native descriptors leaked");
        store.assert_concurrent_state_empty();
        if round == 2 {
            peak = store.data().peak;
        } else if round > 2 {
            assert_eq!(store.data().peak, peak, "native task memory leaked");
        }
    }
    remove.call_async(&mut store, ("/sandbox/a/input",)).await?;
    assert!(!root.path().join("a/input").exists());
    assert!(store.data().table.is_empty());
    Ok(())
}

#[path = "support/cancellation_caller.rs"]
mod cancellation;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use wasmtime::component::Resource;
use wasmtime_wasi::{
    filesystem::Descriptor,
    p3::bindings::{
        clocks::system_clock::Instant,
        filesystem::types::{DescriptorStat, DescriptorType, ErrorCode, PathFlags},
    },
};

#[derive(Default)]
struct Activity {
    active: AtomicUsize,
    started: AtomicUsize,
}
struct Pending(Arc<Activity>);
impl Drop for Pending {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}
fn delayed_stat(linker: &mut Linker<Host>, blocked: bool) -> Result<Arc<Activity>> {
    let activity = Arc::new(Activity::default());
    let tracked = activity.clone();
    linker.allow_shadowing(true);
    linker
        .instance("wasi:filesystem/types@0.3.0")?
        .func_wrap_concurrent(
            "[method]descriptor.stat-at",
            move |_, (_, flags, path): (Resource<Descriptor>, PathFlags, String)| {
                assert_eq!(flags, PathFlags::SYMLINK_FOLLOW);
                tracked.active.fetch_add(1, Ordering::SeqCst);
                tracked.started.fetch_add(1, Ordering::SeqCst);
                let pending = Pending(tracked.clone());
                Box::pin(async move {
                    let _pending = pending;
                    if blocked {
                        std::future::pending::<()>().await;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    let other = Some("é🙂".repeat(32768));
                    let result = if path.ends_with("error") {
                        Err(ErrorCode::Other(other))
                    } else {
                        Ok(DescriptorStat {
                            type_: DescriptorType::Other(other),
                            link_count: 17,
                            size: u64::MAX,
                            data_access_timestamp: Some(Instant {
                                seconds: 888,
                                nanoseconds: 123,
                            }),
                            data_modification_timestamp: if path.ends_with("absent") {
                                None
                            } else {
                                Some(Instant {
                                    seconds: -2,
                                    nanoseconds: 750000000,
                                })
                            },
                            status_change_timestamp: None,
                        })
                    };
                    Ok((result,))
                })
            },
        )?;
    let tracked = activity.clone();
    linker
        .instance("wasi:filesystem/types@0.3.0")?
        .func_wrap_concurrent(
            "[method]descriptor.create-directory-at",
            move |_, (_, path): (Resource<Descriptor>, String)| {
                assert!(path.ends_with("error"));
                tracked.active.fetch_add(1, Ordering::SeqCst);
                tracked.started.fetch_add(1, Ordering::SeqCst);
                let pending = Pending(tracked.clone());
                Box::pin(async move {
                    let _pending = pending;
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    Ok((Err::<(), _>(ErrorCode::Other(Some(
                        "large mutation error".repeat(16384),
                    ))),))
                })
            },
        )?;
    Ok(activity)
}

#[tokio::test(flavor = "current_thread")]
async fn delayed_results_release_import_buffers_independently_of_the_current_task() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let source = r#"
import {stat,mkdir} from 'node:fs/promises';
import type {Stats as Metadata} from 'node:fs';
type Box = {info:Metadata};
async function observeError():Promise<number>{let count=0;try{await stat('/sandbox/error');}catch(e){if(e===37)count++;}try{await mkdir('/sandbox/error');}catch(e){if(e===37)count++;}return count;}
function observe(info:Metadata):number[] {return [info.size,info.mtimeMs,info.isFile()?1:0,info.isDirectory()?1:0];}
export async function run(path:string):Promise<number[]> {
  const pending=stat(path);
  const error=observeError();
  const info:Metadata=await pending;
  const box:Box={info};
  const result=observe(box.info);
  result.push(await error);
  return result;
}
"#;
    let component = Component::new(
        &engine,
        compile(source, "export run: async func(path:string) -> list<f64>;")?,
    )?;
    let root = tempfile::tempdir()?;
    let ctx = WasiCtx::builder()
        .preopened_dir(root.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let mut store = Store::new(
        &engine,
        Host {
            ctx,
            table: ResourceTable::new(),
            peak: 0,
        },
    );
    store.limiter(|host| host);
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let activity = delayed_stat(&mut linker, false)?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (Vec<f64>,)>(&mut store, "run")?;
    let mut peak = 0;
    for round in 0..12 {
        let (a, b) = store
            .run_concurrent(async |accessor| -> wasmtime::Result<_> {
                let (a, b) = tokio::join!(
                    run.call_concurrent(accessor, ("/sandbox/value",)),
                    run.call_concurrent(accessor, ("/sandbox/absent",))
                );
                Ok((a?.0, b?.0))
            })
            .await??;
        assert_eq!(a, [u64::MAX as f64, -1250.0, 0.0, 0.0, 2.0]);
        assert_eq!(b, [u64::MAX as f64, 0.0, 0.0, 0.0, 2.0]);
        assert_eq!(activity.active.load(Ordering::SeqCst), 0);
        assert!(store.data().table.is_empty());
        store.assert_concurrent_state_empty();
        if round == 2 {
            peak = store.data().peak;
        } else if round > 2 {
            assert_eq!(store.data().peak, peak, "imported result buffers leaked");
        }
    }
    assert_eq!(activity.started.load(Ordering::SeqCst), 72);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn caller_cancellation_acknowledges_filesystem_borrows_before_releasing_tasks() -> Result<()>
{
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let worker = compile(
        r#"
import {stat} from 'fs/promises';
export async function run():Promise<void>{
  stat('/sandbox/first');
  try{await stat('/sandbox/second');}finally{throw new Error('cancelled continuation ran');}
}
"#,
        "export run: async func();",
    )?;
    let component = Component::new(&engine, cancellation::cancellation_caller(&worker)?)?;
    let root = tempfile::tempdir()?;
    let ctx = WasiCtx::builder()
        .preopened_dir(root.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let mut store = Store::new(
        &engine,
        Host {
            ctx,
            table: ResourceTable::new(),
            peak: 0,
        },
    );
    store.limiter(|host| host);
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let activity = delayed_stat(&mut linker, true)?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(), (u32,)>(&mut store, "run")?;
    let mut peak = 0;
    for round in 0..12 {
        let (status,) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run.call_async(&mut store, ()),
        )
        .await??;
        assert_eq!(status, 4);
        assert_eq!(activity.active.load(Ordering::SeqCst), 0);
        assert!(
            store.data().table.is_empty(),
            "descriptor remained borrowed after cancellation"
        );
        store.assert_concurrent_state_empty();
        if round == 0 {
            peak = store.data().peak;
        } else {
            assert_eq!(store.data().peak, peak);
        }
    }
    assert_eq!(activity.started.load(Ordering::SeqCst), 24);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn preopens_confine_normalized_paths_and_options_preserve_source_effects() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let source = r#"
import * as fs from 'node:fs/promises';
type Trace={value:number};
function flag(t:Trace,digit:number,value:boolean):boolean {t.value=t.value*10+digit;return value;}
function path(t:Trace):string {t.value=t.value*10+1;return '/sandbox/nested/no/../input';}
function absent():boolean|undefined{return undefined;}
export async function run():Promise<number[]> {
  const trace:Trace={value:0};
  const info=await fs.stat(path(trace),{bigint:flag(trace,2,true),bigint:flag(trace,3,false),throwIfNoEntry:flag(trace,4,true),unknown:flag(trace,5,true)});
  const result=[trace.value,info.size];
  const optional=await fs.stat('/sandbox/input',{bigint:absent(),throwIfNoEntry:absent()});result.push(optional.size);
  const root=await fs.stat('/sandbox/nested');result.push(root.isDirectory()?1:0);
  try{await fs.mkdir('/sandbox');}catch(e){if(e===7)result.push(7);}
  try{await fs.unlink('/sandbox/nested/');}catch(e){if(e===31)result.push(31);}
  try{await fs.rmdir('/sandbox/.');}catch(e){if(e===31)result.push(31);}
  try{await fs.stat('/sandboxish/input');}catch(e){if(e===1)result.push(1);}
  try{await fs.stat('/sandbox/../../outside');}catch(e){if(e===1)result.push(1);}
  try{await fs.stat('../input');}catch(e){if(e===1)result.push(1);}
  try{await fs.stat('/sandbox/input',{bigint:true});}catch(e){if(e===12)result.push(12);}
  try{await fs.stat('/sandbox/missing',{throwIfNoEntry:false});}catch(e){if(e===12)result.push(12);}
  try{await fs.mkdir('/sandbox/child',{recursive:true});}catch(e){if(e===12)result.push(12);}
  try{await fs.rmdir('/sandbox/nested',{recursive:true});}catch(e){if(e===12)result.push(12);}
  try{await fs.mkdir('/readonly/child');}catch(e){if(e===31)result.push(31);}
  try{await fs.unlink('/readonly/input');}catch(e){if(e===31)result.push(31);}
  try{await fs.stat('/sandbox/escape/input');}catch(e){if(e===31)result.push(31);}
  await fs.mkdir('/sandbox/child',{recursive:false});await fs.rmdir('/sandbox/child');
  await fs.unlink('/sandbox/nested/input');
  return result;
}
"#;
    let component = Component::new(
        &engine,
        compile(source, "export run: async func() -> list<f64>;")?,
    )?;
    let outer = tempfile::tempdir()?;
    let inner = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    std::fs::create_dir(outer.path().join("nested"))?;
    std::fs::write(outer.path().join("input"), b"outer")?;
    std::fs::write(outer.path().join("nested/input"), b"unselected")?;
    std::fs::write(inner.path().join("input"), b"inner-file")?;
    std::fs::write(outside.path().join("input"), b"protected")?;
    std::os::unix::fs::symlink(outside.path(), outer.path().join("escape"))?;
    let ctx = WasiCtx::builder()
        .preopened_dir(outer.path(), "/sandbox", FsPerms::ReadWrite)?
        .preopened_dir(inner.path(), "/sandbox/nested", FsPerms::ReadWrite)?
        .preopened_dir(outer.path(), "/readonly", FsPerms::ReadOnly)?
        .build();
    let mut store = Store::new(
        &engine,
        Host {
            ctx,
            table: ResourceTable::new(),
            peak: 0,
        },
    );
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(), (Vec<f64>,)>(&mut store, "run")?;
    let (values,) = run.call_async(&mut store, ()).await?;
    assert_eq!(
        values,
        [
            12345.0, 10.0, 5.0, 1.0, 7.0, 31.0, 31.0, 1.0, 1.0, 1.0, 12.0, 12.0, 12.0, 12.0, 31.0,
            31.0, 31.0
        ]
    );
    assert!(!inner.path().join("input").exists());
    assert!(!outer.path().join("child").exists());
    assert_eq!(
        std::fs::read(outer.path().join("nested/input"))?,
        b"unselected"
    );
    assert_eq!(std::fs::read(outside.path().join("input"))?, b"protected");
    assert!(store.data().table.is_empty());
    store.assert_concurrent_state_empty();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn empty_and_relative_preopens_keep_roots_protected() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(
        &engine,
        compile(
            r#"
import {stat,unlink,rmdir} from 'fs/promises';
export async function run():Promise<number[]> {
  const directory=await stat('.');const file=await stat('./é🙂');
  const result=[directory.isDirectory()?1:0,file.size];
  try{await unlink('.');}catch(e){if(e===31)result.push(31);}
  try{await rmdir('./');}catch(e){if(e===31)result.push(31);}
  return result;
}
"#,
            "export run: async func() -> list<f64>;",
        )?,
    )?;
    for mount in ["", ".", "/"] {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("é🙂"), b"content")?;
        let ctx = WasiCtx::builder()
            .preopened_dir(root.path(), mount, FsPerms::ReadWrite)?
            .build();
        let mut store = Store::new(
            &engine,
            Host {
                ctx,
                table: ResourceTable::new(),
                peak: 0,
            },
        );
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(), (Vec<f64>,)>(&mut store, "run")?;
        assert_eq!(
            run.call_async(&mut store, ()).await?.0,
            [1.0, 7.0, 31.0, 31.0],
            "mount {mount:?}"
        );
        assert!(store.data().table.is_empty());
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[test]
fn native_filesystem_requires_async_exports_and_explicit_wit_imports() -> Result<()> {
    let synchronous =
        "import {stat} from 'fs/promises'; export function run():void{stat('input');}";
    let error = compile(synchronous, "export run: func();")
        .expect_err("synchronous filesystem call accepted");
    assert!(
        format!("{error:#}").contains("filesystem operations require async WIT exports"),
        "{error:#}"
    );
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("world.wit"),
        "package test:filesystem; world filesystem {export run: async func();}",
    )?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("filesystem"))?;
    let source = "import {stat} from 'fs/promises'; export async function run():Promise<void>{await stat('input');}";
    let error = compile_for_world(source, "missing-import.ts", &resolve, world, true)
        .expect_err("filesystem without imports accepted");
    assert!(
        format!("{error:#}").contains("wasi:filesystem"),
        "{error:#}"
    );
    Ok(())
}
