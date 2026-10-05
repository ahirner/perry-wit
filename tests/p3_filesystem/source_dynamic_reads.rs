use std::fs;

use anyhow::Result;
use wasmtime_wasi::{FsPerms, WasiCtxBuilder};

use super::instantiate;

#[tokio::test(flavor = "current_thread")]
async fn dynamic_reads_keep_their_kind_through_helpers_guards_and_collection() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("input"), "é😀\0")?;
    let source = r#"
    import {readFile, writeFile} from "fs/promises";
    async function load(path: string, encoding: string): Promise<string | Uint8Array> {
        return (await readFile(path, {encoding, flag: "r"}));
    }
    function inspect(value: Uint8Array | string): number {
        const alias = value;
        let index = 0;
        while (index < 2000) { const temporary = new Uint8Array(128); index = index + 1; }
        if (typeof value === "string") {
            if (value !== "é😀\0") { return -1; }
            return value.length;
        } else {
            value[0] = 71;
            if (value !== alias) { return -2; }
            const view = value.subarray(0, 2);
            return view[0] + value.length;
        }
    }
    export async function run(encoding: string): Promise<number> {
        const value = (await load("/sandbox/input", encoding));
        const result = inspect(value);
        (await writeFile("/sandbox/output", value));
        return result;
    }"#;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String,), (f64,)>(&mut store, "run")?;
    for _ in 0..10 {
        for (encoding, expected, bytes) in [
            ("utf8", 3.0, "é😀\0".as_bytes().to_vec()),
            (
                "BiNaRy",
                78.0,
                [b"G".as_slice(), &"é😀\0".as_bytes()[1..]].concat(),
            ),
        ] {
            assert_eq!(
                run.call_async(&mut store, (encoding.into(),)).await?.0,
                expected
            );
            assert_eq!(fs::read(directory.path().join("output"))?, bytes);
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn union_assignments_keep_tags_across_branches_loops_and_finally() -> Result<()> {
    let source = r#"
    function identity(value: string | Uint8Array): string | Uint8Array { return value; }
    function length(value: string | Uint8Array): number { return value.length; }
    export function run(binary: boolean): number {
        let value: string | Uint8Array = "é😀";
        if (binary) { value = new Uint8Array([4, 5, 6]); }
        const alias = identity(value);
        let count = 0;
        while (count < 2000) {
            if (typeof value === "string") { value = new Uint8Array([8, 9]); }
            else { value = "abc"; }
            count = count + 1;
        }
        let result = length(alias) * 100;
        try {
            if (typeof value !== "object") {
                result = result + value.length;
                value = new Uint8Array([42]);
                throw 7;
            }
            value = "z";
        } catch (error) {
            if (typeof value === "object") { result = result + value[0]; }
        } finally {
            if ("string" === typeof value) { result = result + value.length; }
            else { result = result + value[0]; }
        }
        return result;
    }"#;
    let (mut store, instance) = instantiate(source, WasiCtxBuilder::new().build()).await?;
    let run = instance.get_typed_func::<(bool,), (f64,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, (false,)).await?.0, 287.0);
    assert_eq!(run.call_async(&mut store, (true,)).await?.0, 301.0);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn union_truthiness_equality_and_typeof_distinguish_text_from_views() -> Result<()> {
    let source = r#"
    function identity(value: string | Uint8Array): string | Uint8Array { return value; }
    export function run(): number {
        const emptyText = identity("");
        const emptyBytes = identity(new Uint8Array(0));
        let result = 0;
        if (emptyText) { return -1; }
        if (!emptyBytes) { return -2; }
        if (emptyText !== "") { return -3; }
        if (emptyBytes === "") { return -4; }
        if (emptyBytes !== emptyBytes) { return -5; }
        if (emptyBytes === identity(new Uint8Array(0))) { return -6; }
        if (emptyText !== identity("")) { return -7; }
        if (emptyBytes === 0) { return -8; }
        if (emptyText === false) { return -9; }
        if (identity("é") !== "é"[0]) { return -10; }
        if (identity("é") === "é"[1]) { return -11; }
        if (typeof emptyText === "string") { result = result + 1; }
        if (typeof emptyBytes === "object") { result = result + 2; }
        return result;
    }"#;
    let (mut store, instance) = instantiate(source, WasiCtxBuilder::new().build()).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, ()).await?.0, 3.0);
    Ok(())
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    wasmtime::component::ComponentType,
    wasmtime::component::Lift,
    wasmtime::component::Lower,
)]
#[component(variant)]
enum TextOrBytes {
    #[component(name = "text")]
    Text(String),
    #[component(name = "bytes")]
    Bytes(Vec<u8>),
}

#[tokio::test(flavor = "current_thread")]
async fn component_variants_roundtrip_direct_and_stored_outcomes() -> Result<()> {
    for body in [
        "export function run(value: string | Uint8Array): Uint8Array | string { return value; }",
        r#"
        async function retain(value: string | Uint8Array): Promise<string | Uint8Array> { return value; }
        async function forward(pending: Promise<Uint8Array | string>): Promise<string | Uint8Array> { return pending; }
        export async function run(value: string | Uint8Array): Promise<Uint8Array | string> {
            const pending = retain(value);
            const forwarded = forward(pending);
            let index = 0;
            while (index < 2000) { const temporary = new Uint8Array(128); index = index + 1; }
            const first = await forwarded;
            const second = await pending;
            if (first !== second) { throw 42; }
            return second;
        }"#,
    ] {
        let (mut store, instance) = instantiate(body, WasiCtxBuilder::new().build()).await?;
        let run = instance.get_typed_func::<(TextOrBytes,), (TextOrBytes,)>(&mut store, "run")?;
        for _ in 0..10 {
            for value in [
                TextOrBytes::Text("\u{feff}é😀\0".into()),
                TextOrBytes::Text(String::new()),
                TextOrBytes::Bytes(vec![0, 255, 128, 42]),
                TextOrBytes::Bytes(vec![]),
            ] {
                assert_eq!(run.call_async(&mut store, (value.clone(),)).await?.0, value);
                store.assert_concurrent_state_empty();
                assert!(store.data().table.is_empty());
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn dynamic_file_results_cross_component_result_variants() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("input"), "é😀\0")?;
    for declaration in [
        "export async function run(encoding: string): Promise<Result<string | Uint8Array, number>> { return (await readFile('/sandbox/input', encoding)); }",
        r#"
        async function read(encoding: string): Promise<string | Uint8Array> { return (await readFile('/sandbox/input', encoding)); }
        export async function run(encoding: string): Promise<Result<string | Uint8Array, number>> {
            const pending = read(encoding);
            const value = await pending;
            return value;
        }"#,
    ] {
        let source = format!("import {{readFile}} from 'fs/promises'; {declaration}");
        let context = WasiCtxBuilder::new()
            .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
            .build();
        let (mut store, instance) = instantiate(&source, context).await?;
        let run =
            instance.get_typed_func::<(String,), (Result<TextOrBytes, f64>,)>(&mut store, "run")?;
        for _ in 0..10 {
            for (encoding, expected) in [
                ("utf8", Ok(TextOrBytes::Text("é😀\0".into()))),
                (
                    "binary",
                    Ok(TextOrBytes::Bytes("é😀\0".as_bytes().to_vec())),
                ),
                ("hex", Err(12.0)),
            ] {
                assert_eq!(
                    run.call_async(&mut store, (encoding.into(),)).await?.0,
                    expected
                );
                store.assert_concurrent_state_empty();
                assert!(store.data().table.is_empty());
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn runtime_options_preserve_effects_and_validate_the_selected_data_kind() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("input"), "é😀\0")?;
    let source = r#"
    import {readFile, writeFile} from 'node:fs/promises';
    function step(state: Uint8Array, value: string): string { state[0] = state[0] + 1; return value; }
    export async function run(encoding: string, writeEncoding: string): Promise<number> {
        const state = new Uint8Array(1);
        try {
            const value = (await readFile(step(state, '/sandbox/input'), {encoding: step(state, encoding), flag: step(state, 'r')}));
            (await writeFile(step(state, '/sandbox/output'), value, {encoding: step(state, writeEncoding), flag: step(state, 'w')}));
            return value.length * 100 + state[0];
        } catch (error) { return 0 - error * 100 - state[0]; }
    }"#;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String, String), (f64,)>(&mut store, "run")?;
    for (encoding, write_encoding, expected, written) in [
        ("utf8", "utf8", 306.0, true),
        ("binary", "binary", 706.0, true),
        ("utf8", "binary", -1206.0, false),
        ("hex", "utf8", -1203.0, false),
        ("utf8\0", "utf8", -1203.0, false),
    ] {
        fs::write(directory.path().join("output"), b"keep")?;
        assert_eq!(
            run.call_async(&mut store, (encoding.into(), write_encoding.into()))
                .await?
                .0,
            expected
        );
        assert_eq!(
            fs::read(directory.path().join("output"))?,
            if written {
                "é😀\0".as_bytes()
            } else {
                b"keep"
            }
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    fs::write(directory.path().join("input"), [255, 0])?;
    assert_eq!(
        run.call_async(&mut store, ("utf8".into(), "utf8".into()))
            .await?
            .0,
        -903.0
    );
    assert_eq!(
        run.call_async(&mut store, ("binary".into(), "binary".into()))
            .await?
            .0,
        206.0
    );
    assert_eq!(fs::read(directory.path().join("output"))?, [255, 0]);
    Ok(())
}

#[test]
fn union_consumers_require_current_type_guards_and_preserve_binding_types() {
    for body in [
        "return value[0];",
        "return value.byteLength;",
        "return value.slice(0).length;",
        "return value < value;",
        "return value == '';",
        "return value + 1;",
        "if (typeof value === 'string') { value = new Uint8Array(1); return value.charAt(0).length; } return 0;",
        "if (typeof value === 'object') { value = ''; return value[0]; } return 0;",
        "if (typeof value === 'object') { while (true) { value = ''; break; } return value.byteLength; } return 0;",
        "if (typeof value === 'string') { try { value = new Uint8Array(1); } finally { return value.charAt(0).length; } } return 0;",
        "value = 1; return 0;",
        "let text = ''; text = value; return 0;",
    ] {
        let source =
            format!("export function run(value: string | Uint8Array): number {{ {body} }}");
        assert!(
            crate::waffle_fixture::compile_typescript_waffle(
                &source,
                "union-errors.ts",
                &Default::default()
            )
            .is_err(),
            "{body}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn local_union_declarations_register_string_comparisons_without_string_literals() -> Result<()>
{
    let source = "export function run(): boolean { let value: string | Uint8Array = new Uint8Array(0); return value === value; }";
    let (mut store, instance) = instantiate(source, WasiCtxBuilder::new().build()).await?;
    let run = instance.get_typed_func::<(), (bool,)>(&mut store, "run")?;
    assert!(run.call_async(&mut store, ()).await?.0);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn stored_byte_results_retain_identity_and_backing_storage() -> Result<()> {
    let source = r#"
    async function create(): Promise<Uint8Array> { return new Uint8Array([7, 8, 9]).subarray(1); }
    export async function run(): Promise<Uint8Array> {
        const pending = create();
        let index = 0;
        while (index < 2000) { const temporary = new Uint8Array(128); index = index + 1; }
        const first = await pending;
        first[0] = 42;
        const second = await pending;
        if (first !== second) { throw 1; }
        return second;
    }"#;
    let (mut store, instance) = instantiate(source, WasiCtxBuilder::new().build()).await?;
    let run = instance.get_typed_func::<(), (Vec<u8>,)>(&mut store, "run")?;
    for _ in 0..10 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, [42, 9]);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}
