use super::compile;
use crate::wasi_host::{self, Host};
use anyhow::Result;
use wasmtime::{Store, component::Instance};

async fn instantiate_async(bytes: &[u8]) -> Result<(Store<Host>, Instance)> {
    wasi_host::instantiate(&crate::engine::async_engine()?, bytes, Host::default()).await
}

/// Exact integers survive promise frames, result slots and host timer suspensions.
#[tokio::test(flavor = "current_thread")]
async fn integer_domains_survive_async_frames_and_result_slots() -> Result<()> {
    use wasmtime::component::Val;
    for (domain, source_type, a, b, expected) in [
        ("u8", "number", Val::U8(255), Val::U8(2), Val::U8(1)),
        (
            "s32",
            "number",
            Val::S32(i32::MAX),
            Val::S32(2),
            Val::S32(i32::MIN + 1),
        ),
        (
            "u64",
            "bigint",
            Val::U64(u64::MAX),
            Val::U64(2),
            Val::U64(1),
        ),
        (
            "s64",
            "bigint",
            Val::S64(9_007_199_254_740_993),
            Val::S64(2),
            Val::S64(9_007_199_254_740_995),
        ),
    ] {
        for source in [
            format!(
                "function add(a:{source_type},b:{source_type}):{source_type} {{ return a+b; }} export async function run(a:{source_type},b:{source_type}):Promise<{source_type}> {{ await Promise.resolve(1); return add(a,b); }}"
            ),
            format!(
                "async function add(a:{source_type},b:{source_type}):Promise<{source_type}> {{ await Promise.resolve(1); return a+b; }} export async function run(a:{source_type},b:{source_type}):Promise<{source_type}> {{ return await add(a,b); }}"
            ),
            format!(
                "export async function run(a:{source_type},b:{source_type}):Promise<{source_type}> {{ return await Promise.resolve(a+b); }}"
            ),
            format!(
                "import {{ setTimeout }} from 'node:timers/promises'; async function add(a:{source_type},b:{source_type}):Promise<{source_type}> {{ const saved = await setTimeout(1,a); await setTimeout(1); return saved+b; }} export async function run(a:{source_type},b:{source_type}):Promise<{source_type}> {{ return await add(a,b); }}"
            ),
        ] {
            let compiled = compile(
                &source,
                &format!("async func(a:{domain},b:{domain})->{domain}"),
            )?;
            let (mut store, instance) =
                instantiate_async(&compiled.component.unwrap().stripped).await?;
            let run = instance.get_func(&mut store, "run").unwrap();
            let mut results = [Val::Bool(false)];
            run.call_async(&mut store, &[a.clone(), b.clone()], &mut results)
                .await?;
            assert_eq!(results[0], expected, "{domain}");
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn promise_joins_and_escaping_payloads_converge_on_compatible_representations() -> Result<()>
{
    use wasmtime::component::Val;
    let helper = "async function add(a:number,b:number):Promise<number> { await Promise.resolve(1); return a+b; } async function sum(n:number):Promise<number> { if(n<=0)return 0; return n+await sum(n-1); }";
    for (body, result_type, yes, no) in [
        (
            "const pending = flag ? add(a,b) : Promise.resolve(a+b); return await pending;",
            "u8",
            Val::U8(1),
            Val::U8(1),
        ),
        (
            "const pending = flag ? add(a,b) : Promise.resolve(0.5); return await pending;",
            "f64",
            Val::Float64(257.0),
            Val::Float64(0.5),
        ),
        (
            "const pending = flag ? add(a,b) : add(0.5,b); return await pending;",
            "f64",
            Val::Float64(257.0),
            Val::Float64(2.5),
        ),
        (
            "const values = await Promise.all([add(a,b), Promise.resolve(0.5)]); return values[0] + values[1];",
            "f64",
            Val::Float64(257.5),
            Val::Float64(257.5),
        ),
        (
            "let pending = Promise.resolve(a); for(let i=0;i<b;i++){ pending=add(await pending,a); } return await pending;",
            "u8",
            Val::U8(253),
            Val::U8(253),
        ),
        ("return await sum(b);", "u32", Val::U32(3), Val::U32(3)),
    ] {
        let source = format!(
            "{helper} export async function run(a:number,b:number,flag:boolean):Promise<number> {{ {body} }}"
        );
        let compiled = compile(
            &source,
            &format!("async func(a:u8,b:u8,flag:bool)->{result_type}"),
        )
        .map_err(|error| anyhow::anyhow!("{body}: {error:#}"))?;
        let (mut store, instance) =
            instantiate_async(&compiled.component.unwrap().stripped).await?;
        let run = instance.get_func(&mut store, "run").unwrap();
        for (flag, expected) in [(true, yes), (false, no)] {
            let mut results = [Val::Bool(false)];
            run.call_async(
                &mut store,
                &[Val::U8(255), Val::U8(2), Val::Bool(flag)],
                &mut results,
            )
            .await?;
            assert_eq!(results[0], expected, "{body}; flag={flag}");
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_dispatch_roots_only_constructed_frame_handlers() -> Result<()> {
    for (body, combinator) in [
        ("return await value(n);", false),
        (
            "const values = await Promise.all([value(n), Promise.resolve(n)]); return values[0] + values[1];",
            true,
        ),
    ] {
        let compiled = compile(
            &format!(
                "async function value(n:number):Promise<number> {{ return n+1; }} export async function run(n:number):Promise<number> {{ {body} }}"
            ),
            "async func(n:f64)->f64",
        )?;
        let wat = wasmprinter::print_bytes(&compiled.core)?;
        assert_eq!(wat.contains("(func $perry_combinator_step "), combinator);
        assert!(
            !wat.contains("wasi:cli/stderr"),
            "fulfilled jobs need no error reporting"
        );
        let component = compiled.component.as_ref().unwrap();
        for bytes in [&component.bytes, &component.stripped] {
            let (mut store, instance) = instantiate_async(bytes).await?;
            let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
            let (value,) = run.call_async(&mut store, (3.0,)).await?;
            assert_eq!(value, if combinator { 7.0 } else { 4.0 });
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    Ok(())
}
