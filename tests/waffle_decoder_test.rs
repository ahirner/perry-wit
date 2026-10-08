#[path = "support/waffle.rs"]
mod waffle_fixture;
use std::{fs, process::Command};
use waffle_fixture::compile_typescript_waffle;

use anyhow::Result;
use perry_wit::waffle_backend::WaffleCompileOptions;
use wasmtime::component::{Component, Instance, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

const DECODE: &str = r#"
function make(ignoreBOM: boolean): TextDecoder {
    return new TextDecoder("utf-8", { fatal: true, ignoreBOM });
}
function piece(decoder: TextDecoder, bytes: Uint8Array, stream: boolean): string {
    return decoder.decode(bytes, { stream });
}
export function run(input: Uint8Array, width: number, ignore: boolean): string {
    const decoder = make(ignore);
    const alias = decoder;
    if (alias !== decoder) { return "identity"; }
    if (decoder === make(ignore)) { return "shared"; }
    if (decoder.encoding !== "utf-8") { return "encoding"; }
    if (decoder.fatal !== true) { return "fatal"; }
    if (decoder.ignoreBOM !== ignore) { return "BOM"; }
    let result = "";
    let index = 0;
    try {
        while (index < input.length) {
            result = result + piece(decoder, input.subarray(index, index + width), true);
            index = index + width;
        }
        return result + decoder.decode();
    } catch { return "invalid"; }
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn split_scalars_boms_and_invalid_sequences_match_fatal_node_decoding() -> Result<()> {
    let (mut store, instance) = instantiate(DECODE).await?;
    let run = instance.get_typed_func::<(Vec<u8>, f64, bool), (String,)>(&mut store, "run")?;
    let mut inputs = vec![
        vec![],
        vec![0],
        "Aé中😀e\u{301}\0".as_bytes().to_vec(),
        "\u{feff}A\u{feff}é😀".as_bytes().to_vec(),
    ];
    inputs.extend([
        vec![0x80],
        vec![0xc0, 0x80],
        vec![0xc1, 0xbf],
        vec![0xc2],
        vec![0xe0, 0x80],
        vec![0xe0, 0xa0],
        vec![0xed, 0xa0, 0x80],
        vec![0xf0, 0x80, 0x80, 0x80],
        vec![0xf4, 0x90, 0x80, 0x80],
        vec![0xf5, 0x80, 0x80, 0x80],
        vec![0xff],
        vec![0xf0, 0x9f, 0x98],
        vec![65, 0xe2, 66],
        vec![0xef, 0xbb, 0xbf],
    ]);
    let boundaries: String = [
        0x7f, 0x80, 0x7ff, 0x800, 0xd7ff, 0xe000, 0xffff, 0x10000, 0x10ffff,
    ]
    .into_iter()
    .map(|value| char::from_u32(value).unwrap())
    .collect();
    inputs.push(boundaries.into_bytes());
    let mut seed = 17u32;
    for _ in 0..32 {
        let mut valid = String::new();
        for _ in 0..64 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            if let Some(value) = char::from_u32(seed % 0x110000) {
                valid.push(value);
            }
        }
        inputs.push(valid.into_bytes());
    }
    for length in 0..32 {
        for _ in 0..16 {
            let mut bytes = Vec::new();
            for _ in 0..length {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                bytes.push((seed >> 24) as u8);
            }
            inputs.push(bytes);
        }
    }
    let mut calls = Vec::new();
    let mut actual = Vec::new();
    for input in inputs {
        for width in [1, 2, 3, 4, 7, 33] {
            for ignore in [false, true] {
                actual.push(
                    run.call_async(&mut store, (input.clone(), f64::from(width), ignore))
                        .await?
                        .0,
                );
                calls.push(format!(
                    "run(new Uint8Array({}), {width}, {ignore})",
                    serde_json::to_string(&input)?
                ));
                store.assert_concurrent_state_empty();
            }
        }
    }
    assert_eq!(actual, node_results(DECODE, &calls)?);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn decoder_copies_pending_bytes_and_results_survive_mutation_and_reuse() -> Result<()> {
    let source = r#"
    export function run(): string {
        const decoder = new TextDecoder("utf8", {fatal: true});
        const bytes = new Uint8Array([240,159]);
        let result = decoder.decode(bytes, {stream: true});
        bytes[0] = 0;
        bytes[1] = 0;
        result = result + decoder.decode(new Uint8Array([152,128]));
        const ascii = new Uint8Array([65]);
        const saved = decoder.decode(ascii);
        ascii[0] = 0;
        result = result + saved;
        result = result + decoder.decode(new Uint8Array([239,187,191,66]));
        result = result + decoder.decode();
        return result;
    }"#;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    let mut actual = Vec::new();
    for _ in 0..30 {
        actual.push(run.call_async(&mut store, ()).await?.0);
    }
    assert_eq!(actual, node_results(source, &vec!["run()".into(); 30])?);
    store.assert_concurrent_state_empty();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn constructor_and_decode_options_preserve_argument_order() -> Result<()> {
    let source = r#"
    function label(state: Uint8Array): string { state[0] = 1; return " " + "UTF-8 "; }
    function option(state: Uint8Array, digit: number, value: boolean): boolean {
        state[0] = state[0] * 3 + digit;
        let index = 0;
        while (index < 2000) {
            const scratch = new Uint8Array(64);
            scratch[0] = index;
            index = index + 1;
        }
        return value;
    }
    function input(state: Uint8Array): Uint8Array {
        state[0] = state[0] * 3 + 4;
        return new Uint8Array([65]);
    }
    export function run(): string {
        const state = new Uint8Array(1);
        const decoder = new TextDecoder(label(state), {
            ignoreBOM: option(state, 2, false), fatal: option(state, 3, true)
        });
        const result = decoder.decode(input(state), {
            ignored: option(state, 5, false), stream: option(state, 6, false)
        });
        if (state[0] !== 31) { return "order"; }
        return result;
    }"#;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    let actual = vec![run.call_async(&mut store, ()).await?.0];
    assert_eq!(actual, ["A"]);
    assert_eq!(actual, node_results(source, &["run()".into()])?);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn utf8_labels_are_checked_and_default_decoding_is_strict() -> Result<()> {
    let source = r#"
    export function run(label: string): string {
        try { return new TextDecoder(label, {fatal: true}).encoding; }
        catch { return "invalid"; }
    }"#;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance.get_typed_func::<(String,), (String,)>(&mut store, "run")?;
    let mut actual = Vec::new();
    let mut calls = Vec::new();
    for label in [
        "utf-8",
        "UTF8",
        "unicode-1-1-utf-8",
        "unicode11utf8",
        "unicode20utf8",
        "x-unicode20utf8",
        "\t\n\r\u{c} UtF-8 \r",
        "",
        "utf8\0",
        "utf-8 extra",
        "not-utf8",
        "\u{b}utf8",
        "\u{a0}utf8",
    ] {
        actual.push(run.call_async(&mut store, (label.into(),)).await?.0);
        calls.push(format!("run({})", serde_json::to_string(label)?));
    }
    assert_eq!(actual, node_results(source, &calls)?);
    let strict = r#"
    export function run(): Result<string, number> {
        const decoder = new TextDecoder();
        if (decoder.fatal !== true) { throw 99; }
        let failures = 0;
        try { new TextDecoder("utf-16"); } catch (error) { if (error.code !== 1) { throw 98; } failures = failures + 1; }
        try { new TextDecoder("utf8", {fatal: false}); } catch (error) { if (error.code !== 1) { throw 97; } failures = failures + 1; }
        if (failures !== 2) { throw 96; }
        try {return decoder.decode(new Uint8Array([255]));} catch(error) {throw error.code;}
    }"#;
    let (mut store, instance) = instantiate(strict).await?;
    let run =
        instance.get_typed_func::<(), (std::result::Result<String, f64>,)>(&mut store, "run")?;
    for _ in 0..30 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, Err(2.0));
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn transparent_constructor_syntax_and_shadowed_names_keep_binding_identity() -> Result<()> {
    for constructor in [
        "TextDecoder",
        "(TextDecoder)",
        "(TextDecoder as typeof TextDecoder)",
        "globalThis.TextDecoder",
        "globalThis['TextDecoder']",
    ] {
        let shadow = if constructor.starts_with("globalThis") {
            "function TextDecoder(value: string): string { return value; } function shadow(): string { return TextDecoder(\"A\"); }"
        } else {
            ""
        };
        let source = format!(
            r#"
        {shadow}
        export function run(): string {{
            const decoder = new {constructor}("utf8", {{fatal: true}});
            const prefix = decoder.decode(new Uint8Array([240,159]), {{stream:true}});
            return prefix + decoder.decode(new Uint8Array([152,128]));
        }}"#
        );
        let (mut store, instance) = instantiate(&source).await?;
        let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
        assert_eq!(run.call_async(&mut store, ()).await?.0, "😀");
        assert_eq!(node_results(&source, &["run()".into()])?, ["😀"]);
    }
    let shadow = r#"function TextDecoder(value: string): string { return value; }
        export function run(): string { return TextDecoder("shadow"); }"#;
    let (mut store, instance) = instantiate(shadow).await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, ()).await?.0, "shadow");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_options_and_undefined_defaults_evaluate_once_in_order() -> Result<()> {
    let source = r#"
    function effect(state: Uint8Array, value: boolean): boolean { state[0] = state[0] + 1; return value; }
    function omitted(state: Uint8Array): void { state[0] = state[0] + 1; }
    export function run(): string {
        const state = new Uint8Array(1);
        const decoder = new TextDecoder(undefined, {fatal: effect(state, false), fatal: effect(state, true)});
        let result = decoder.decode(new Uint8Array([240]), {stream: effect(state, false), stream: effect(state, true)});
        result = result + decoder.decode(new Uint8Array([159]), {stream: true});
        result = result + decoder.decode(new Uint8Array([152,128]), {stream: omitted(state)});
        result = result + decoder.decode(omitted(state), null);
        if (state[0] !== 6) { return "effects"; }
        return result;
    }"#;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    let actual = vec![run.call_async(&mut store, ()).await?.0];
    assert_eq!(actual, ["😀"]);
    assert_eq!(actual, node_results(source, &["run()".into()])?);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn streaming_errors_keep_the_standard_input_queue_until_consumed_or_reset() -> Result<()> {
    let source = r#"
    export function run(): string {
        const decoder = new TextDecoder("utf8", {fatal: true});
        let result = "";
        try { result = result + decoder.decode(new Uint8Array([65,224,128,66]), {stream: true}); }
        catch { result = result + "!"; }
        let i = 0;
        while (i < 2000) { const scratch = new Uint8Array(64); scratch[0] = i; i = i + 1; }
        try { result = result + decoder.decode(new Uint8Array([67]), {stream: true}); }
        catch { result = result + "!"; }
        result = result + decoder.decode(new Uint8Array([239,187,191,68]), {stream: true});
        result = result + decoder.decode();
        result = result + decoder.decode(new Uint8Array([239,187,191,69]));
        return result;
    }"#;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    for _ in 0..3 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, "!!BC\u{feff}DE");
    }
    // Node discards unread bytes on a fatal error; the Encoding Standard retains its I/O queue.
    assert_eq!(node_results(source, &["run()".into()])?, ["!C\u{feff}DE"]);
    store.assert_concurrent_state_empty();
    Ok(())
}

#[test]
fn unsupported_decoder_forms_preserve_diagnostics() {
    for source in [
        "export function run(): string { return new TextDecoder().decode('text'); }",
        "export function run(): string { return new TextDecoder().decode(new Uint8Array(1), {get stream() { return true; }}); }",
        "export function run(): string { return new TextDecoder().decode(new Uint8Array(1), {...{stream:true}}); }",
        "export function run(): string { return new TextDecoder().decode(new Uint8Array(1), {['stream']:true}); }",
        "export function run(): string { return new TextDecoder().decode(undefined, {__proto__:new TextDecoder()}); }",
        "export function run(): string { return new TextDecoder().decode(...[new Uint8Array(1)]); }",
        "export function run(): string { return new TextDecoder('utf8', {get fatal() { return true; }}).decode(); }",
        "export function run(): string { return new TextDecoder('utf8', {fatal:true}, 7).decode(); }",
        "export function run(): string { return new TextDecoder().decode(undefined, undefined, 7); }",
        "export function run(): number { return new TextDecoder(); }",
        "function read(d: TextDecoder): string { return d.decode(); } export function run(): string { return read(true); }",
        "export function run(): TextDecoder { return new TextDecoder(); }",
        "export function run(): string { const options = {stream:true}; return new TextDecoder().decode(undefined, options); }",
        "export function run(flag: boolean): string { let decoder = new TextDecoder(); if (flag) { decoder = true; } else { decoder = new TextDecoder(); } return decoder.decode(); }",
        "export function run(flag: boolean): number { let bytes = new Uint8Array(1); if (flag) { bytes = true; } else { bytes = new Uint8Array(1); } return bytes[0]; }",
    ] {
        assert!(
            compile_typescript_waffle(
                source,
                "invalid-decoder.ts",
                &WaffleCompileOptions::default()
            )
            .is_err(),
            "{source}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stored_tasks_retain_decoders_and_share_decoded_text_outcomes() -> Result<()> {
    let source = r#"
    async function work(): Promise<string> {
        const decoder = new TextDecoder("utf8", {fatal: true});
        const prefix = decoder.decode(new Uint8Array([240]), {stream: true});
        let index = 0;
        while (index < 2000) {
            const scratch = new Uint8Array(64);
            scratch[0] = index;
            index = index + 1;
        }
        return prefix + decoder.decode(new Uint8Array([159,152,128]));
    }
    export async function run(): Promise<string> {
        const pending = work();
        const first = await pending;
        const second = await pending;
        return first + second;
    }"#;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    let mut actual = Vec::new();
    for _ in 0..30 {
        actual.push(run.call_async(&mut store, ()).await?.0);
        store.assert_concurrent_state_empty();
    }
    assert_eq!(actual, vec!["😀😀"; 30]);
    assert_eq!(actual, node_results(source, &vec!["run()".into(); 30])?);
    Ok(())
}

async fn instantiate(source: &str) -> Result<(Store<StoreLimits>, Instance)> {
    let compiled =
        compile_typescript_waffle(source, "decoder.ts", &WaffleCompileOptions::default())?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_more_async_builtins(true);
    config.wasm_component_model_threading(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    assert_eq!(component.component_type().imports(&engine).count(), 0);
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(65536).build(),
    );
    store.limiter(|limits| limits);
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    Ok((store, instance))
}

fn node_results(source: &str, calls: &[String]) -> Result<Vec<String>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("decoder.mts");
    fs::write(
        &path,
        format!(
            "{source}\nconsole.log(JSON.stringify(await Promise.all([{}])));",
            calls.join(",")
        ),
    )?;
    let output = Command::new("node").arg(path).output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}
