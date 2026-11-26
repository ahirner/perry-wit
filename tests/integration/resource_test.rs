//! Ownership checks precede canonical calls; resource aliases share one opaque descriptor.
use crate::world::Compiler::MemoryChecked;
use anyhow::Result;
use wasmtime::{
    Engine, Store,
    component::{Component, Linker, Resource, ResourceAny, ResourceType},
};

use crate::engine::async_engine;

const WIT: &str = "package test:boundary;
interface secrets {
    resource secret;
    get: func(value:u32)->secret;
    reveal: func(value:borrow<secret>)->u32;
    take: func(value:secret)->u32;
    pair: func(a:secret,b:secret)->u32;
}
world boundary { import secrets; export run:func(value:u32)->result<u32>; }";
struct Secret;
#[derive(Default)]
struct Counts {
    drops: u32,
    reveals: u32,
    taken: u32,
}

#[test]
fn exported_class_handles_keep_the_instance_until_canonical_drop() -> Result<()> {
    let wit = "package test:counter; interface counters {
        resource counter {constructor(value:u32); get:func()->u32; twice:static func(value:u32)->counter;}
        resource label {constructor(); text:func()->string;}
        copy:func(value:borrow<counter>)->counter;
        visit:func(values:list<u32>);
        drops:func()->u32;
    } world api {export counters;}";
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("counter.ts"),
        "export let disposed=0;
        export function drops() {return disposed;} function read(instance) {return instance.value;}
        export class Local { constructor(value) {this.value=value;} get() {return read(this);}
        static twice(value:number) {return new Local(value*2);}
        [Symbol.dispose]() {disposed++;} }",
    )?;
    let source = "import { Local, drops } from './counter'; class Label { text(){return 'label';} } export const counters={Counter:Local,Label,drops,copy(value){return new Local(value.get());},visit(values){for (const n of values) { if(n) continue; }}};";
    let entry = directory.path().join("entry.ts");
    let engine = Engine::default();
    let bytes = MemoryChecked.component(wit, "api", &entry.to_string_lossy(), source)?;
    let component = Component::new(&engine, bytes)?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let counters = component.get_export_index(None, "test:counter/counters");
    let member = |name| {
        component
            .get_export_index(counters.as_ref(), name)
            .ok_or_else(|| anyhow::anyhow!("missing {name}"))
    };
    let new = instance
        .get_typed_func::<(u32,), (ResourceAny,)>(&mut store, &member("[constructor]counter")?)?;
    let get = instance
        .get_typed_func::<(ResourceAny,), (u32,)>(&mut store, &member("[method]counter.get")?)?;
    let drops = instance.get_typed_func::<(), (u32,)>(&mut store, &member("drops")?)?;
    let twice = instance
        .get_typed_func::<(u32,), (ResourceAny,)>(&mut store, &member("[static]counter.twice")?)?;
    let copy =
        instance.get_typed_func::<(ResourceAny,), (ResourceAny,)>(&mut store, &member("copy")?)?;
    let (a,) = new.call(&mut store, (42,))?;
    let (b,) = twice.call(&mut store, (7,))?;
    let (c,) = copy.call(&mut store, (b,))?;
    a.resource_drop(&mut store)?;
    assert_eq!(get.call(&mut store, (b,))?, (14,));
    assert_eq!(drops.call(&mut store, ())?, (1,));
    b.resource_drop(&mut store)?;
    assert_eq!(get.call(&mut store, (c,))?, (14,));
    c.resource_drop(&mut store)?;
    assert_eq!(drops.call(&mut store, ())?, (3,));
    Ok(())
}

#[test]
fn resource_members_follow_the_receiver_in_retained_and_specialized_bodies() -> Result<()> {
    let wit = "package test:members; interface host {
        resource text { constructor(); read:func()->string; }
        resource count { read:func()->u32; open:static func()->count; }
        next:func()->text;
    } world source { import host; } world solved { import host; export run:func()->u32; }";
    let imports = "import {Text,Count,next} from 'test:members/host';";
    let retained = format!(
        "{imports} function retained():number {{
        new Text()[Symbol.dispose](); return next().read().length + Count.open().read(); }}"
    );
    MemoryChecked.compile(wit, "source", "retained.ts", &retained)?;
    let wrong_arity = retained.replace("next().read()", "next().read(1)");
    let error = MemoryChecked
        .compile(wit, "source", "retained.ts", &wrong_arity)
        .err()
        .ok_or_else(|| anyhow::anyhow!("retained body skipped WIT arity checking"))?;
    assert!(
        error
            .to_string()
            .contains("MethodTextRead expects 0 arguments, got 1"),
        "{error}"
    );
    let solved = format!("{imports} function read(x) {{ return x.read(); }}
        class Local {{ read() {{ return 2; }} }}
        export function run() {{ return read(new Text()).length + read(Count.open()) + read(new Local()); }}");
    MemoryChecked.compile(wit, "solved", "solved.ts", &solved)?;
    Ok(())
}

#[test]
fn imported_error_cases_throw_and_release_owned_payloads() -> Result<()> {
    use wasmtime::component::{ComponentType, Lift, Lower};

    #[derive(ComponentType, Lift, Lower)]
    #[component(variant)]
    enum Fault {
        #[component(name = "closed")]
        Closed,
        #[component(name = "last-operation-failed")]
        Failed(Resource<Secret>),
    }

    let wit = "package demo:io; interface streams {
        resource failure; variant fault {closed,last-operation-failed(failure)}
        resource reader {read:static func(closed:bool)->result<u8,fault>;}
    } world app {import streams;export run:func(closed:bool)->string;}";
    let source = "import {Reader} from 'demo:io/streams';
export function run(closed) {try {Reader.read(closed); return 'ok';}
catch(e) {return e.message;}}";
    let compile = |source: &str| MemoryChecked.component(wit, "app", "read.js", source);
    for (read, column) in [
        ("return e.payload.val;", 18),
        ("const {payload: p = 'none'} = e; return String(p);", 17),
    ] {
        let diagnostic = compile(&source.replace("return e.message;", read))
            .err()
            .ok_or_else(|| anyhow::anyhow!("payload read compiled"))?
            .to_string();
        assert!(
            diagnostic.contains(&format!("read.js:3:{column}: error[unsupported-construct]"))
                && diagnostic.contains("payload"),
            "{diagnostic}"
        );
    }

    let engine = Engine::default();
    let component = Component::new(&engine, compile(source)?)?;
    let mut linker = Linker::<u32>::new(&engine);
    let mut io = linker.instance("demo:io/streams")?;
    io.resource("reader", ResourceType::host::<()>(), |_, _| Ok(()))?;
    io.resource("failure", ResourceType::host::<Secret>(), |mut store, _| {
        *store.data_mut() += 1;
        Ok(())
    })?;
    io.func_wrap("[static]reader.read", |_, (closed,): (bool,)| {
        let fault = if closed {
            Fault::Closed
        } else {
            Fault::Failed(Resource::new_own(2))
        };
        Ok((Err::<u8, _>(fault),))
    })?;
    let mut store = Store::new(&engine, 0);
    let instance = linker.instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(bool,), (String,)>(&mut store, "run")?;
    assert_eq!(run.call(&mut store, (true,))?.0, "closed");
    assert_eq!(*store.data(), 0);
    assert_eq!(run.call(&mut store, (false,))?.0, "last-operation-failed");
    assert_eq!(*store.data(), 1);
    Ok(())
}

#[test]
fn explicit_stdout_without_console_reaches_the_host() -> Result<()> {
    struct OutputStream;
    let bytes = MemoryChecked.component(
        r#"package test:output;
            world app { import wasi:cli/stdout@0.2.0; export run: func(); }
            package wasi:io@0.2.0 { interface streams { resource output-stream; } }
            package wasi:cli@0.2.0 {
                interface stdout { use wasi:io/streams@0.2.0.{output-stream}; get-stdout: func() -> output-stream; }
            }"#,
        "app", "output.ts",
        "import { getStdout } from 'wasi:cli/stdout@0.2.0';
         import { dropOutputStream } from 'wasi:io/streams@0.2.0';
         export function run() { dropOutputStream(getStdout()); }",
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, bytes)?;
    let mut linker = Linker::new(&engine);
    linker
        .instance("wasi:cli/stdout@0.2.0")?
        .func_wrap("get-stdout", |_, ()| {
            Ok((Resource::<OutputStream>::new_own(37),))
        })?;
    linker.instance("wasi:io/streams@0.2.0")?.resource(
        "output-stream",
        ResourceType::host::<OutputStream>(),
        |mut store, rep| {
            *store.data_mut() = Some(rep);
            Ok(())
        },
    )?;
    let mut store = Store::new(&engine, None);
    let instance = linker.instantiate(&mut store, &component)?;
    instance
        .get_typed_func::<(), ()>(&mut store, "run")?
        .call(&mut store, ())?;
    assert_eq!(*store.data(), Some(37));
    Ok(())
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
        let component = Component::new(
            &engine,
            MemoryChecked.component(WIT, "boundary", "resources.ts", &source)?,
        )?;
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
        let run = instance
            .get_typed_func::<(u32,), (std::result::Result<u32, ()>,)>(&mut store, "run")?;
        assert_eq!(
            run.call(&mut store, (42,))
                .map_err(|error| anyhow::anyhow!("{body}: {error:?}"))?
                .0
                .unwrap(),
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
    let wit = "package test:boundary; interface secrets {resource secret; get:func(value:u32)->result<secret>; reveal:func(value:borrow<secret>)->result<u32>; stale:func()->result<u32>;} world boundary {export secrets;}";
    let source = "import type {Secret} from 'test:boundary/secrets'; import {newSecret,secretRep} from 'test:boundary/secrets'; let saved:Secret|undefined=undefined; export function secretsGet(value:number):Secret {return newSecret(value);} export function secretsReveal(value:Secret):number {saved=value;return secretRep(value);} export function secretsStale():number {try {return secretRep(saved);} catch(e) {if(e instanceof TypeError)return 7;return 0;}}";
    let engine = Engine::default();
    let component = Component::new(
        &engine,
        MemoryChecked.component(wit, "boundary", "resources.ts", source)?,
    )?;
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
    let get = instance
        .get_typed_func::<(u32,), (std::result::Result<ResourceAny, ()>,)>(&mut store, get)?;
    let reveal = instance
        .get_typed_func::<(ResourceAny,), (std::result::Result<u32, ()>,)>(&mut store, reveal)?;
    let stale =
        instance.get_typed_func::<(), (std::result::Result<u32, ()>,)>(&mut store, stale)?;
    for value in [0, 42, u32::MAX] {
        let secret = get.call(&mut store, (value,))?.0.unwrap();
        for _ in 0..3 {
            assert_eq!(reveal.call(&mut store, (secret,))?.0.unwrap(), value);
        }
        assert_eq!(stale.call(&mut store, ())?.0.unwrap(), 7);
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
    let wit = "package test:boundary; interface secrets {resource secret;get:func(value:u32)->secret;work:async func(value:borrow<secret>)->u32;} world boundary {import secrets;export run:async func(value:u32)->result<u32>;}";
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
        let component = Component::new(
            &engine,
            MemoryChecked.component(wit, "boundary", "resources.ts", &source)?,
        )?;
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
        let run = instance
            .get_typed_func::<(u32,), (std::result::Result<u32, ()>,)>(&mut store, "run")?;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run.call_async(&mut store, (42,)),
        )
        .await?;
        if detached {
            assert!(
                result?.0.is_err(),
                "detached borrow must fail at task boundary"
            );
        } else {
            assert_eq!(result?.0.unwrap(), 43);
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
        interface service {use secrets.{secret};get:func(value:u32)->result<secret>;run:async func(value:borrow<secret>)->result<u32>;}
        world boundary {import secrets;export service;}";
    let source = "import type {Secret} from 'test:boundary/secrets'; import {get,inspect} from 'test:boundary/secrets'; export function serviceGet(value:number):Secret {return get(value);} export async function serviceRun(value:Secret):Promise<number> {return await inspect(value);}";
    let engine = async_engine()?;
    let component = Component::new(
        &engine,
        MemoryChecked.component(wit, "boundary", "resources.ts", source)?,
    )?;
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
    let get = instance
        .get_typed_func::<(u32,), (std::result::Result<ResourceAny, ()>,)>(&mut store, get)?;
    let run = instance
        .get_typed_func::<(ResourceAny,), (std::result::Result<u32, ()>,)>(&mut store, run)?;
    let a = get.call_async(&mut store, (11,)).await?.0.unwrap();
    let b = get.call_async(&mut store, (29,)).await?.0.unwrap();
    for _ in 0..100 {
        let pair = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            store.run_concurrent(async |accessor| -> wasmtime::Result<_> {
                let (a, b) = tokio::join!(
                    run.call_concurrent(accessor, (a,)),
                    run.call_concurrent(accessor, (b,))
                );
                Ok((a?.0.unwrap(), b?.0.unwrap()))
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
    let error = crate::world::Compiler::MemoryChecked
        .component(wit, "boundary", "resources.ts", "")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("error[contract-mismatch]:")
            && error.contains("resource identities are distinct"),
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
    let component = Component::new(
        &engine,
        MemoryChecked.component(wit, "boundary", "resources.ts", source)?,
    )?;
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
