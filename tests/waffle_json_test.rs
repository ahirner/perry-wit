#[path = "support/waffle.rs"]
mod waffle_fixture;
use anyhow::Result;
use perry_wit::waffle_backend::WaffleCompileOptions;
use std::{fs, process::Command, time::Duration};
use waffle_fixture::compile_typescript_waffle;
use wasmtime::component::{Component, Linker, ResourceTable, Val};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

#[tokio::test(flavor = "current_thread")]
async fn source_json_round_trips_shared_values_and_reclaims_temporary_graphs() -> Result<()> {
    let source = r#"
        function copy(value: any): any { return value; }
        export function run(input: string, count: number): string {
            const retained = JSON.parse(input);
            let result = "";
            for (let i = 0; i < count; i++) {
                const temporary = JSON.parse(input);
                result = JSON.stringify(copy(temporary));
            }
            return JSON.stringify(copy(retained));
        }
    "#;
    run_json_cases(
        source,
        &[
            (
                vec![
                    Val::String(
                        r#"{"2":2,"1":"😀","b":false,"a":[null,1e21,{},[],"é\u0000"],"b":true}"#
                            .into(),
                    ),
                    Val::Float64(1500.0),
                ],
                Val::String(
                    "{\"1\":\"😀\",\"2\":2,\"b\":true,\"a\":[null,1e+21,{},[],\"é\\u0000\"]}"
                        .into(),
                ),
            ),
            (
                vec![Val::String("-0".into()), Val::Float64(1000.0)],
                Val::String("0".into()),
            ),
            (
                vec![
                    Val::String("[1e400,false,\"\\uD83D\\uDE00\"]".into()),
                    Val::Float64(1000.0),
                ],
                Val::String("[null,false,\"😀\"]".into()),
            ),
        ],
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_source_json_unwinds_through_catch_and_allocating_finally() -> Result<()> {
    let source = r#"
        export function run(input: string, count: number): number {
            let result = 0;
            for (let i = 0; i < count; i++) {
                try { JSON.parse(input); }
                catch (error) { result = error; }
                finally { const cleanup = JSON.stringify({ok: "😀"}); result = result + cleanup.length; }
            }
            return result;
        }
    "#;
    run_json_cases(
        source,
        &[
            (
                vec![Val::String("[1,]".into()), Val::Float64(1000.0)],
                Val::Float64(11.0),
            ),
            (
                vec![Val::String("\"\\uD800\"".into()), Val::Float64(1000.0)],
                Val::Float64(12.0),
            ),
        ],
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn parsed_graphs_preserve_aliases_mutations_and_held_values() -> Result<()> {
    let source = r#"
        function identity(value: any): any { return value; }
        export function run(input: string, count: number): string {
            const root = JSON.parse(input);
            const alias = identity(root);
            const array = alias.items;
            const held = array[0];
            array[0] = {text: "changed"};
            if (!Array.isArray(array)) throw 95;
            if (Array.isArray(root)) throw 96;
            if (!(0 in array)) throw 97;
            if (2 in array) throw 98;
            if (!("items" in alias)) throw 99;
            if ("missing" in alias) throw 100;
            for (let i = 0; i < count; i++) {
                const garbage = JSON.parse("[1,2,3]");
                JSON.stringify(garbage);
            }
            if (array[2] !== undefined) throw 90;
            if (array[1].text !== 'é') throw 91;
            if (root !== alias) throw 92;
            if (array !== root.items) throw 93;
            const text = held.text;
            if(typeof text!=="string")throw 94;
            if (new TextEncoder().encode(text).length !== 5) throw 94;
            return JSON.stringify({root: root, held: held, keys: Object.keys(root)});
        }
    "#;
    let input = r#"{"items":[{"text":"😀x"},{"text":"é"}]}"#;
    let directory = tempfile::tempdir()?;
    let script = directory.path().join("json.mts");
    fs::write(
        &script,
        format!(
            "{source}\nprocess.stdout.write(JSON.stringify(run({}, 1000)));",
            serde_json::to_string(input)?
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
    let expected: String = serde_json::from_slice(&node.stdout)?;
    assert_eq!(
        expected,
        r#"{"root":{"items":[{"text":"changed"},{"text":"é"}]},"held":{"text":"😀x"},"keys":["items"]}"#
    );
    run_json_cases(
        source,
        &[(
            vec![Val::String(input.into()), Val::Float64(1000.0)],
            Val::String(expected),
        )],
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn stringify_omits_undefined_and_rejects_cycles_without_losing_cleanup() -> Result<()> {
    let source = r#"
        export function run(input: string, count: number): string {
            const root = JSON.parse(input);
            root.self = root;
            let result = "";
            for (let i = 0; i < count; i++) {
                try { result = JSON.stringify(root); }
                catch (error) { result = "caught"; }
                finally { const cleanup = JSON.stringify([null, undefined]); }
            }
            root.self = undefined;
            if (JSON.stringify(undefined) !== undefined) throw 90;
            return result + ":" + JSON.stringify(root);
        }
    "#;
    run_json_cases(
        source,
        &[(
            vec![Val::String(r#"{"ok":true}"#.into()), Val::Float64(1000.0)],
            Val::String("caught:{\"ok\":true}".into()),
        )],
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn stringify_preserves_date_and_undefined_semantics() -> Result<()> {
    run_json_cases(
        r#"
        export function run(): string {
            const value = {date: new Date(0), invalid: new Date(NaN), omitted: undefined};
            return JSON.stringify(value) + ":" + JSON.stringify(undefined);
        }
    "#,
        &[(
            vec![],
            Val::String(
                "{\"date\":\"1970-01-01T00:00:00.000Z\",\"invalid\":null}:undefined".into(),
            ),
        )],
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn inert_json_options_and_erased_type_arguments_preserve_runtime_values() -> Result<()> {
    run_json_cases(
        r#"
        export function run(input: string): string {
            const first = JSON.parse(input, null);
            const second = JSON.parse<{text: string}>(input);
            return JSON.stringify(first, null, undefined) + ":" + JSON.stringify(second);
        }
    "#,
        &[(
            vec![Val::String(r#"{"text":"😀","extra":42}"#.into())],
            Val::String(r#"{"text":"😀","extra":42}:{"text":"😀","extra":42}"#.into()),
        )],
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn json_and_text_helpers_share_memory_and_keep_wit_error_channels() -> Result<()> {
    run_json_cases(
        r#"
        export function run(input: string): Result<string, number> {
            const value = JSON.parse(input);
            value.text = value.text.toUpperCase();
            if (value.text.indexOf("SS") !== 1) throw 90;
            return JSON.stringify(value);
        }
    "#,
        &[
            (
                vec![Val::String("[1,]".into())],
                Val::Result(Err(Some(Box::new(Val::Float64(1.0))))),
            ),
            (
                vec![Val::String(r#"{"text":"aß😀"}"#.into())],
                Val::Result(Ok(Some(Box::new(Val::String(
                    r#"{"text":"ASS😀"}"#.into(),
                ))))),
            ),
            (
                vec![Val::String(r#"{"text":false}"#.into())],
                Val::Result(Err(Some(Box::new(Val::Float64(12.0))))),
            ),
        ],
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn parsed_values_survive_pending_tasks_and_repeated_awaits() -> Result<()> {
    run_json_cases(
        r#"
        import {setTimeout as waitFor} from "node:timers/promises";
        async function parse(input: string): Promise<any> {
            const value = JSON.parse(input);
            await waitFor(10);
            return value;
        }
        export async function run(input: string, count: number): Promise<string> {
            const pending = parse(input);
            for (let i = 0; i < count; i++) { JSON.stringify(JSON.parse(input)); }
            const value = await pending;
            value.items[0] = "changed";
            for (let i = 0; i < count; i++) { JSON.stringify(JSON.parse(input)); }
            const again = await pending;
            if (value !== again) throw 90;
            return JSON.stringify(again);
        }
    "#,
        &[(
            vec![
                Val::String(r#"{"items":["initial",{"text":"😀"}]}"#.into()),
                Val::Float64(1000.0),
            ],
            Val::String(r#"{"items":["changed",{"text":"😀"}]}"#.into()),
        )],
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn large_json_materializes_without_a_fixed_buffer_and_reclaims_between_calls() -> Result<()> {
    let (mut store, instance) = instantiate_json(
        "export function run(input: string): string { return JSON.stringify(JSON.parse(input)); }",
        2_097_152,
    )
    .await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    let input = format!(
        "[\"{}\",true,null,{{\"text\":\"😀\"}}]",
        "🦀é".repeat(25_000)
    );
    for _ in 0..20 {
        assert_eq!(run.call_async(&mut store, (&input,)).await?.0, input);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn pending_json_graphs_are_disposable_with_their_store() -> Result<()> {
    let (mut store, instance) = instantiate_json(
        r#"
        import {setTimeout as waitFor} from "node:timers/promises";
        export async function run(input: string): Promise<string> {
            const value = JSON.parse(input);
            await waitFor(10000);
            return JSON.stringify(value);
        }
    "#,
        262_144,
    )
    .await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            run.call_async(&mut store, (r#"{"items":["😀",{}]}"#,))
        )
        .await
        .is_err()
    );
    drop(store);
    Ok(())
}

#[test]
fn unsupported_json_options_are_diagnosed_before_frontend_folding() {
    for (call, diagnostic) in [
        ("JSON.parse()", "argument count"),
        ("JSON.parse(input, null, 3)", "argument count"),
        ("JSON.stringify(input, null, null, 4)", "argument count"),
        ("JSON.stringify(...input)", "Spread JSON"),
        ("JSON.stringify(input, sideEffect())", "JSON revivers"),
        ("JSON.parse(input, (key, value) => value)", "JSON revivers"),
        ("(JSON as any).parse(input, sideEffect())", "JSON revivers"),
        ("JSON.stringify(input, null, 2)", "JSON revivers"),
        ("JSON[input](input)", "Dynamic JSON"),
        ("JSON.rawJSON(input)", "Unsupported JSON method"),
        ("JSON.stringify([,input])", "Array elisions are unsupported"),
        (
            "Array.isArray(input, sideEffect())",
            "Array.isArray requires",
        ),
    ] {
        let source = format!(
            "function sideEffect(): number {{ return 1; }} export function run(input: string): string {{ return {call}; }}"
        );
        let error =
            compile_typescript_waffle(&source, "json_options.ts", &WaffleCompileOptions::default())
                .unwrap_err();
        assert!(error.to_string().contains(diagnostic), "{call}: {error:#}");
    }
}

#[test]
fn a_shadowed_json_identifier_keeps_ordinary_guest_behavior() -> Result<()> {
    let compiled = compile_typescript_waffle(
        "function JSON(value: number): number { return value + 1; } export function run(value: number): number { return JSON(value); }",
        "shadowed_json.ts",
        &WaffleCompileOptions::default(),
    )?;
    let engine = wasmtime::Engine::default();
    let module = wasmtime::Module::new(&engine, compiled.core)?;
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = wasmtime::Instance::new(&mut store, &module, &[])?;
    assert_eq!(
        instance
            .get_typed_func::<f64, f64>(&mut store, "run")?
            .call(&mut store, 4.0)?,
        5.0
    );
    Ok(())
}

async fn run_json_cases(source: &str, cases: &[(Vec<Val>, Val)]) -> Result<()> {
    let (mut store, instance) = instantiate_json(source, 262_144).await?;
    let run = instance.get_func(&mut store, "run").unwrap();
    for (params, expected) in cases {
        let mut results = [Val::Bool(false)];
        tokio::time::timeout(
            Duration::from_secs(10),
            run.call_async(&mut store, params, &mut results),
        )
        .await??;
        assert_eq!(&results[0], expected, "{params:?}");
    }
    Ok(())
}

async fn instantiate_json(
    source: &str,
    limit: usize,
) -> Result<(Store<JsonHost>, wasmtime::component::Instance)> {
    let compiled =
        compile_typescript_waffle(source, "json_values.ts", &WaffleCompileOptions::default())?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    config.wasm_component_model_threading(true);
    config.wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let core = wasmtime::Module::new(&engine, &compiled.core)?;
    if compiled.uses_p3_clocks {
        assert!(core.imports().all(|import| matches!(
            import.module(),
            "wasi:clocks/monotonic-clock@0.3.0" | "$root"
        )));
    } else {
        assert_eq!(core.imports().count(), 0);
    }
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::<JsonHost>::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let mut store = Store::new(
        &engine,
        JsonHost {
            limits: StoreLimitsBuilder::new().memory_size(limit).build(),
            ..Default::default()
        },
    );
    store.limiter(|host| &mut host.limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    Ok((store, instance))
}

#[derive(Default)]
struct JsonHost {
    limits: StoreLimits,
    context: WasiCtx,
    table: ResourceTable,
}

impl WasiView for JsonHost {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.context,
            table: &mut self.table,
        }
    }
}
