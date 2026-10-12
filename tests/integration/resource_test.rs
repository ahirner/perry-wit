//! Ownership checks precede canonical calls; resource aliases share one opaque descriptor.
use anyhow::Result;
use wasmtime::{
    Engine, Store,
    component::{Component, Linker, Resource, ResourceAny, ResourceType},
};

use crate::engine::async_engine;

fn compile(source: &str, wit: &str) -> Result<Vec<u8>> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("resources.wit", wit)?;
    let world = resolve.select_world(&[package], Some("boundary"))?;
    Ok(perry_wit::compiler::compile_for_world_with_memory_checks(
        source,
        "resources.ts",
        &resolve,
        world,
    )?
    .component
    .unwrap()
    .stripped)
}
const WIT: &str = "package test:boundary;
interface secrets {
    resource secret;
    get: func(value:u32)->secret;
    reveal: func(value:borrow<secret>)->u32;
    take: func(value:secret)->u32;
    pair: func(a:secret,b:secret)->u32;
}
world boundary { import secrets; export run:func(value:u32)->u32; }";
struct Secret;
#[derive(Default)]
struct Counts {
    drops: u32,
    reveals: u32,
    taken: u32,
}

#[test]
fn imported_resources_transfer_borrow_collect_and_rollback() -> Result<()> {
    for (body, expected, drops, reveals, taken) in [
        (
            "const a=get(value); const n=reveal(a); dropSecret(a); return n;",
            42,
            1,
            1,
            0,
        ),
        ("const a=get(value); return take(a);", 42, 0, 0, 1),
        ("const a=get(value); return reveal(a);", 42, 1, 1, 0),
        (
            "const a=get(value); const alias=a; dropSecret(a); try { return reveal(alias); } catch(e) { if(e instanceof TypeError) return 7; return 0; }",
            7,
            1,
            0,
            0,
        ),
        (
            "const a=get(value); try { pair(a,a); return 0; } catch(e) { if (!(e instanceof TypeError)) return 1; } const n=reveal(a); return take(a)+n;",
            84,
            0,
            1,
            1,
        ),
        (
            "let n=0; for(let i=0;i<3000;i++){ const a=get(value); n=reveal(a); } return n;",
            42,
            3000,
            3000,
            0,
        ),
    ] {
        let source = format!(
            "import {{get,reveal,take,pair,dropSecret}} from 'test:boundary/secrets'; export function run(value:number):number {{{body}}}"
        );
        let engine = Engine::default();
        let component = Component::new(&engine, compile(&source, WIT)?)?;
        let mut linker = Linker::<Counts>::new(&engine);
        let mut host = linker.instance("test:boundary/secrets")?;
        host.resource("secret", ResourceType::host::<Secret>(), |mut store, _| {
            store.data_mut().drops += 1;
            Ok(())
        })?;
        host.func_wrap("get", |_, (value,): (u32,)| {
            Ok((Resource::<Secret>::new_own(value),))
        })?;
        host.func_wrap("reveal", |mut store, (value,): (Resource<Secret>,)| {
            assert!(!value.owned());
            store.data_mut().reveals += 1;
            Ok((value.rep(),))
        })?;
        host.func_wrap("take", |mut store, (value,): (Resource<Secret>,)| {
            assert!(value.owned());
            store.data_mut().taken += 1;
            Ok((value.rep(),))
        })?;
        host.func_wrap(
            "pair",
            |_, _: (Resource<Secret>, Resource<Secret>)| -> wasmtime::Result<(u32,)> {
                panic!("invalid duplicate transfer reached host")
            },
        )?;
        let mut store = Store::new(&engine, Counts::default());
        let instance = linker.instantiate(&mut store, &component)?;
        let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;
        assert_eq!(
            run.call(&mut store, (42,))
                .map_err(|error| anyhow::anyhow!("{body}: {error:?}"))?
                .0,
            expected,
            "{body}"
        );
        assert_eq!(
            (store.data().drops, store.data().reveals, store.data().taken),
            (drops, reveals, taken),
            "{body}"
        );
    }
    Ok(())
}

#[test]
fn exported_resources_preserve_representations_and_expire_saved_borrows() -> Result<()> {
    let wit = "package test:boundary; interface secrets {resource secret; get:func(value:u32)->secret; reveal:func(value:borrow<secret>)->u32; stale:func()->u32;} world boundary {export secrets;}";
    let source = "import type {Secret} from 'test:boundary/secrets'; import {newSecret,secretRep} from 'test:boundary/secrets'; let saved:Secret|undefined=undefined; export function secretsGet(value:number):Secret {return newSecret(value);} export function secretsReveal(value:Secret):number {saved=value;return secretRep(value);} export function secretsStale():number {try {return secretRep(saved);} catch(e) {if(e instanceof TypeError)return 7;return 0;}}";
    let engine = Engine::default();
    let component = Component::new(&engine, compile(source, wit)?)?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let (_, interface) = instance
        .get_export(&mut store, None, "test:boundary/secrets")
        .unwrap();
    let (_, get) = instance
        .get_export(&mut store, Some(&interface), "get")
        .unwrap();
    let (_, reveal) = instance
        .get_export(&mut store, Some(&interface), "reveal")
        .unwrap();
    let (_, stale) = instance
        .get_export(&mut store, Some(&interface), "stale")
        .unwrap();
    let get = instance.get_typed_func::<(u32,), (ResourceAny,)>(&mut store, get)?;
    let reveal = instance.get_typed_func::<(ResourceAny,), (u32,)>(&mut store, reveal)?;
    let stale = instance.get_typed_func::<(), (u32,)>(&mut store, stale)?;
    for value in [0, 42, u32::MAX] {
        let secret = get.call(&mut store, (value,))?.0;
        for _ in 0..3 {
            assert_eq!(reveal.call(&mut store, (secret,))?.0, value);
        }
        assert_eq!(stale.call(&mut store, ())?.0, 7);
        secret.resource_drop(&mut store)?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_resource_borrows_release_only_after_acknowledgement() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    };
    struct Acknowledged(Arc<AtomicU32>);
    impl Drop for Acknowledged {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let wit = "package test:boundary; interface secrets {resource secret;get:func(value:u32)->secret;work:async func(value:borrow<secret>)->u32;} world boundary {import secrets;export run:async func(value:u32)->u32;}";
    for detached in [false, true] {
        let body = if detached {
            "return 7;"
        } else {
            "let checked=0;try {dropSecret(a);} catch(e){if(e instanceof TypeError)checked=1;} const n=await p; dropSecret(a); return n+checked;"
        };
        let source = format!(
            "import {{get,work,dropSecret}} from 'test:boundary/secrets'; export async function run(value:number):Promise<number> {{const a=get(value);const p=work(a);{body}}}"
        );
        let engine = async_engine()?;
        let component = Component::new(&engine, compile(&source, wit)?)?;
        let mut linker = Linker::<Counts>::new(&engine);
        let ack = Arc::new(AtomicU32::new(0));
        let destroyed_after = ack.clone();
        let started = ack.clone();
        let mut host = linker.instance("test:boundary/secrets")?;
        host.resource(
            "secret",
            ResourceType::host::<Secret>(),
            move |mut store, _| {
                assert_eq!(
                    destroyed_after.load(Ordering::SeqCst),
                    1,
                    "borrow owner released before host acknowledgement"
                );
                store.data_mut().drops += 1;
                Ok(())
            },
        )?;
        host.func_wrap("get", |_, (value,): (u32,)| {
            Ok((Resource::<Secret>::new_own(value),))
        })?;
        host.func_wrap_concurrent("work", move |_, (value,): (Resource<Secret>,)| {
            let completed = Acknowledged(started.clone());
            assert!(!value.owned());
            Box::pin(async move {
                let _completed = completed;
                if detached {
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                Ok((value.rep(),))
            })
        })?;
        let mut store = Store::new(&engine, Counts::default());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run.call_async(&mut store, (42,)),
        )
        .await?;
        if detached {
            assert!(
                result.is_err(),
                "detached borrow must fail at task boundary"
            );
        } else {
            assert_eq!(result?.0, 43);
        }
        assert_eq!(ack.load(Ordering::SeqCst), 1);
        assert_eq!(store.data().drops, 1);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn imported_borrows_pass_through_overlapping_host_transfers() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    };
    let wit = "package test:boundary;
        interface secrets {resource secret;get:func(value:u32)->secret;inspect:async func(value:borrow<secret>)->u32;}
        interface service {use secrets.{secret};get:func(value:u32)->secret;run:async func(value:borrow<secret>)->u32;}
        world boundary {import secrets;export service;}";
    let source = "import type {Secret} from 'test:boundary/secrets'; import {get,inspect} from 'test:boundary/secrets'; export function serviceGet(value:number):Secret {return get(value);} export async function serviceRun(value:Secret):Promise<number> {return await inspect(value);}";
    let engine = async_engine()?;
    let component = Component::new(&engine, compile(source, wit)?)?;
    let mut store = Store::new(&engine, 0u32);
    let mut linker = Linker::new(&engine);
    let calls = Arc::new(AtomicU32::new(0));
    let started = calls.clone();
    let mut host = linker.instance("test:boundary/secrets")?;
    host.resource("secret", ResourceType::host::<Secret>(), |mut store, _| {
        *store.data_mut() += 1;
        Ok(())
    })?;
    host.func_wrap("get", |_, (value,): (u32,)| {
        Ok((Resource::<Secret>::new_own(value),))
    })?;
    host.func_wrap_concurrent("inspect", move |_, (value,): (Resource<Secret>,)| {
        let calls = started.clone();
        let pair = calls.fetch_add(1, Ordering::SeqCst) / 2;
        Box::pin(async move {
            while calls.load(Ordering::SeqCst) < pair * 2 + 2 {
                tokio::task::yield_now().await;
            }
            Ok((value.rep(),))
        })
    })?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let (_, interface) = instance
        .get_export(&mut store, None, "test:boundary/service")
        .unwrap();
    let (_, get) = instance
        .get_export(&mut store, Some(&interface), "get")
        .unwrap();
    let (_, run) = instance
        .get_export(&mut store, Some(&interface), "run")
        .unwrap();
    let get = instance.get_typed_func::<(u32,), (ResourceAny,)>(&mut store, get)?;
    let run = instance.get_typed_func::<(ResourceAny,), (u32,)>(&mut store, run)?;
    let a = get.call_async(&mut store, (11,)).await?.0;
    let b = get.call_async(&mut store, (29,)).await?.0;
    for _ in 0..100 {
        let pair = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            store.run_concurrent(async |accessor| -> wasmtime::Result<_> {
                let (a, b) = tokio::join!(
                    run.call_concurrent(accessor, (a,)),
                    run.call_concurrent(accessor, (b,))
                );
                Ok((a?.0, b?.0))
            }),
        )
        .await???;
        assert_eq!(pair, (11, 29));
        store.assert_concurrent_state_empty();
    }
    a.resource_drop_async(&mut store).await?;
    b.resource_drop_async(&mut store).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 200);
    assert_eq!(*store.data(), 2);
    Ok(())
}

#[test]
fn ambiguous_imported_and_exported_resource_declarations_are_diagnosed() {
    let wit = "package test:boundary; interface tokens {resource token;} world boundary {import tokens;export tokens;}";
    let error = compile("", wit).unwrap_err().to_string();
    assert!(
        error.contains("resource identities are distinct"),
        "{error}"
    );
}

#[test]
fn nested_tagged_results_transfer_each_resource_once() -> Result<()> {
    let wit = "package test:boundary;
        interface secrets {resource secret; batch:func(fail:bool)->result<list<secret>,string>;}
        interface service {use secrets.{secret};run:func(fail:bool)->result<list<secret>,string>;}
        world boundary {import secrets;export service;}";
    let source = "import {batch} from 'test:boundary/secrets'; import type {Secret} from 'test:boundary/secrets';
        export function serviceRun(fail:boolean):{ok:true,value:Secret[]}|{ok:false,error:string} {
            const result=batch(fail);
            if(result.ok) return {ok:true,value:result.value};
            return {ok:false,error:result.error};
        }";
    let engine = Engine::default();
    let component = Component::new(&engine, compile(source, wit)?)?;
    let mut linker = Linker::<Counts>::new(&engine);
    let mut host = linker.instance("test:boundary/secrets")?;
    host.resource("secret", ResourceType::host::<Secret>(), |mut store, _| {
        store.data_mut().drops += 1;
        Ok(())
    })?;
    host.func_wrap("batch", |_, (fail,): (bool,)| {
        let result: Result<Vec<Resource<Secret>>, String> = if fail {
            Err("failed".into())
        } else {
            Ok(vec![Resource::new_own(13), Resource::new_own(29)])
        };
        Ok((result,))
    })?;
    let mut store = Store::new(&engine, Counts::default());
    let instance = linker.instantiate(&mut store, &component)?;
    let (_, interface) = instance
        .get_export(&mut store, None, "test:boundary/service")
        .unwrap();
    let (_, run) = instance
        .get_export(&mut store, Some(&interface), "run")
        .unwrap();
    let run =
        instance.get_typed_func::<(bool,), (Result<Vec<ResourceAny>, String>,)>(&mut store, run)?;
    for _ in 0..20 {
        assert_eq!(run.call(&mut store, (true,))?.0.unwrap_err(), "failed");
        let resources = run.call(&mut store, (false,))?.0.unwrap();
        assert_eq!(resources.len(), 2);
        for resource in resources {
            resource.resource_drop(&mut store)?;
        }
    }
    assert_eq!(store.data().drops, 40);
    Ok(())
}
