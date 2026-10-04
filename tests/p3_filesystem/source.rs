use anyhow::Result;
use perry_wit::{compile_typescript_waffle, waffle_backend::WaffleCompileOptions};
use std::{fs, process::Command};
use wasmtime::component::{Component, Instance, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimitsBuilder};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder};

use super::Host;

#[path = "source_lifecycle.rs"]
mod lifecycle;

#[tokio::test(flavor = "current_thread")]
async fn source_writes_preserve_binary_subviews_and_exact_utf8_under_repeated_calls() -> Result<()>
{
    let source = r#"
    import disk from "node:fs";
    import {writeFileSync as save} from "fs";
    export function run(path: string, bytes: Uint8Array, text: string): number {
        disk.writeFileSync(path, bytes.subarray(1, bytes.length - 1), {encoding: "BiNaRy", flag: "w"});
        save(path + ".txt", text, "UTF-8");
        return bytes.length;
    }"#;
    let directory = tempfile::tempdir()?;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String, Vec<u8>, String), (f64,)>(&mut store, "run")?;
    for _ in 0..30 {
        for size in [0, 1, 8191, 17] {
            let bytes: Vec<_> = (0..size + 2).map(|index| (index * 37) as u8).collect();
            let text = "é中😀\0".repeat(size / 11);
            assert_eq!(
                run.call_async(
                    &mut store,
                    ("/sandbox/data".into(), bytes.clone(), text.clone())
                )
                .await?
                .0,
                bytes.len() as f64
            );
            assert_eq!(
                std::fs::read(directory.path().join("data"))?,
                bytes[1..bytes.len() - 1]
            );
            assert_eq!(
                std::fs::read(directory.path().join("data.txt"))?,
                text.as_bytes()
            );
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_options_preserve_files_and_argument_effects_before_preopen_resolution()
-> Result<()> {
    let source = r#"
    import {writeFileSync} from "node:fs";
    function path(state: Uint8Array, value: string): string { state[0] = state[0] * 3 + 1; return value; }
    function data(state: Uint8Array): string { state[0] = state[0] * 3 + 2; return "replace"; }
    function option(state: Uint8Array, value: string): string { state[0] = state[0] * 3 + 3; return value; }
    export function run(target: string, flag: string): number {
        const state = new Uint8Array(1);
        try { writeFileSync(path(state, target), data(state), {flag: option(state, flag)}); return 0; }
        catch (error) { return error * 100 + state[0]; }
    }"#;
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("keep"), b"original")?;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String, String), (f64,)>(&mut store, "run")?;
    for _ in 0..30 {
        for path in ["/sandbox/keep", "/sandbox/missing", "/outside/denied"] {
            for flag in ["a", "wx", "r+", "W", ""] {
                assert_eq!(
                    run.call_async(&mut store, (path.into(), flag.into()))
                        .await?
                        .0,
                    1218.0
                );
                store.assert_concurrent_state_empty();
                assert!(store.data().table.is_empty());
            }
        }
        assert_eq!(std::fs::read(directory.path().join("keep"))?, b"original");
        assert!(!directory.path().join("missing").exists());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn filesystem_paths_select_the_longest_mount_and_reject_escapes_without_leaks() -> Result<()>
{
    let source = r#"
    import * as disk from "fs";
    export function run(path: string): Result<number, number> { disk.writeFileSync(path, "é😀"); return 1; }
    "#;
    let outer = tempfile::tempdir()?;
    let inner = tempfile::tempdir()?;
    let unused = tempfile::tempdir()?;
    fs::create_dir(outer.path().join("sub"))?;
    let context = WasiCtxBuilder::new()
        .preopened_dir(outer.path(), "/sandbox", FsPerms::ReadWrite)?
        .preopened_dir(inner.path(), "/sandbox/sub", FsPerms::ReadWrite)?
        .preopened_dir(unused.path(), "/unrelated", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String,), (Result<f64, f64>,)>(&mut store, "run")?;
    for _ in 0..30 {
        for path in ["/sandbox/sub/out", "/sandbox//./sub/no/../out"] {
            assert_eq!(run.call_async(&mut store, (path.into(),)).await?.0, Ok(1.0));
            assert_eq!(fs::read(inner.path().join("out"))?, "é😀".as_bytes());
            assert!(!outer.path().join("sub/out").exists());
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
        for (path, error) in [
            ("/sandboxish/out", 1.0),
            ("/sandbox/../../outside", 1.0),
            ("../sandbox/out", 1.0),
            ("/sandbox/no\0file", 12.0),
            ("/sandbox/missing/out", 20.0),
            ("/sandbox/sub", 14.0),
        ] {
            assert_eq!(
                run.call_async(&mut store, (path.into(),)).await?.0,
                Err(error),
                "{path:?}"
            );
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    for mount in ["/", "."] {
        let context = WasiCtxBuilder::new()
            .preopened_dir(outer.path(), mount, FsPerms::ReadWrite)?
            .build();
        let (mut store, instance) = instantiate(source, context).await?;
        let run = instance.get_typed_func::<(String,), (Result<f64, f64>,)>(&mut store, "run")?;
        assert_eq!(
            run.call_async(&mut store, ("sub/../relative".into(),))
                .await?
                .0,
            Ok(1.0)
        );
        assert_eq!(fs::read(outer.path().join("relative"))?, "é😀".as_bytes());
        assert!(store.data().table.is_empty());
    }
    let context = WasiCtxBuilder::new()
        .preopened_dir(outer.path(), "/sandbox", FsPerms::ReadOnly)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String,), (Result<f64, f64>,)>(&mut store, "run")?;
    assert_eq!(
        run.call_async(&mut store, ("/sandbox/relative".into(),))
            .await?
            .0,
        Err(31.0)
    );
    assert_eq!(fs::read(outer.path().join("relative"))?, "é😀".as_bytes());
    store.assert_concurrent_state_empty();
    assert!(store.data().table.is_empty());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn write_options_defaults_duplicates_and_coercion_limits_are_explicit() -> Result<()> {
    let source = r#"
    import {writeFileSync as save} from "node:fs";
    function effect(state: Uint8Array, value: string): string { state[0] = state[0] + 1; return value; }
    export function run(path: string, encoding: string, binary: boolean): Result<number, number> {
        const state = new Uint8Array(1);
        if (binary) { save(path, new Uint8Array([0,255,128]), {encoding, flag: "w"}); }
        else { save(path, "😀\0é", {flag: effect(state, "a"), flag: effect(state, "w"), encoding}); }
        return state[0];
    }"#;
    let directory = tempfile::tempdir()?;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance
        .get_typed_func::<(String, String, bool), (Result<f64, f64>,)>(&mut store, "run")?;
    for binary in [false, true] {
        for encoding in [
            "utf8", "UTF8", "utf-8", "uTf-8", "binary", "BiNaRy", "hex", "ascii", "", "utf\r8",
            "utf8\0", "utf8\0\0", "utf-8\0",
        ] {
            fs::write(directory.path().join("value"), b"keep")?;
            let valid = encoding.eq_ignore_ascii_case("utf8")
                || encoding.eq_ignore_ascii_case("utf-8")
                || binary && encoding.eq_ignore_ascii_case("binary");
            assert_eq!(
                run.call_async(
                    &mut store,
                    ("/sandbox/value".into(), encoding.into(), binary)
                )
                .await?
                .0,
                if valid {
                    Ok(if binary { 0.0 } else { 2.0 })
                } else {
                    Err(12.0)
                }
            );
            let expected: &[u8] = if !valid {
                b"keep"
            } else if binary {
                &[0, 255, 128]
            } else {
                "😀\0é".as_bytes()
            };
            assert_eq!(fs::read(directory.path().join("value"))?, expected);
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    for options in [
        "undefined",
        "null",
        "{}",
        "{encoding: undefined}",
        "{encoding: null}",
        "{encoding: 'utf8', flag: 'w'}",
        "42",
        "false",
        "{mode: 384}",
        "{unknown: true}",
        "{flag: null}",
        "{flag: undefined}",
    ] {
        let source = format!(
            "import {{writeFileSync}} from 'fs'; export function run(): Result<number, number> {{ writeFileSync('/sandbox/options', '', {options}); return 1; }}"
        );
        let context = WasiCtxBuilder::new()
            .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
            .build();
        let (mut store, instance) = instantiate(&source, context).await?;
        let run = instance.get_typed_func::<(), (Result<f64, f64>,)>(&mut store, "run")?;
        let valid = [
            "undefined",
            "null",
            "{}",
            "{encoding: undefined}",
            "{encoding: null}",
            "{encoding: 'utf8', flag: 'w'}",
        ]
        .contains(&options);
        assert_eq!(
            run.call_async(&mut store, ()).await?.0,
            if valid { Ok(1.0) } else { Err(12.0) },
            "{options}"
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn allocating_write_loops_and_retained_async_tasks_preserve_files_with_bounded_memory()
-> Result<()> {
    let source = r#"
    import {writeFileSync} from "fs";
    async function save(path: string, text: string): Promise<string> {
        writeFileSync(path, text);
        return text;
    }
    export async function run(): Promise<string> {
        const pending = save("/sandbox/retained", "😀é");
        let index = 0;
        while (index < 2000) {
            writeFileSync("/sandbox/loop", new Uint8Array([index, 255, 0]));
            index = index + 1;
        }
        return await pending + await pending;
    }"#;
    let directory = tempfile::tempdir()?;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    for _ in 0..3 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, "😀é😀é");
        assert_eq!(
            fs::read(directory.path().join("retained"))?,
            "😀é".as_bytes()
        );
        assert_eq!(
            fs::read(directory.path().join("loop"))?,
            [1999u32 as u8, 255, 0]
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn supported_overwrite_behavior_matches_node() -> Result<()> {
    let source = r#"
    import * as fs from "node:fs";
    export function run(path: string): number {
        fs.writeFileSync(path, "longer original text");
        fs.writeFileSync(path, "é😀\0", {encoding: "utf8", flag: "w"});
        const bytes = new Uint8Array([71,0,128,255,72]);
        fs.writeFileSync(path + ".bin", bytes.subarray(1,4));
        fs.writeFileSync(path + ".empty", new Uint8Array(0), null);
        return 1;
    }"#;
    let directory = tempfile::tempdir()?;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String,), (f64,)>(&mut store, "run")?;
    assert_eq!(
        run.call_async(&mut store, ("/sandbox/guest".into(),))
            .await?
            .0,
        1.0
    );
    let node_path = directory.path().join("node");
    let entry = directory.path().join("compare.mts");
    fs::write(
        &entry,
        format!("{source}\nrun({});", serde_json::to_string(&node_path)?),
    )?;
    let node = Command::new("node")
        .args([
            "--disable-warning=ExperimentalWarning",
            "--experimental-strip-types",
        ])
        .arg(entry)
        .output()?;
    assert!(
        node.status.success(),
        "{}",
        String::from_utf8_lossy(&node.stderr)
    );
    for suffix in ["", ".bin", ".empty"] {
        assert_eq!(
            fs::read(directory.path().join(format!("guest{suffix}")))?,
            fs::read(directory.path().join(format!("node{suffix}")))?
        );
    }
    store.assert_concurrent_state_empty();
    assert!(store.data().table.is_empty());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn earlier_write_arguments_survive_collection_during_later_argument_evaluation() -> Result<()>
{
    let source = r#"
    import {writeFileSync} from "fs";
    function option(): string {
        let index = 0;
        while (index < 2000) { const scratch = new Uint8Array(1024); index = index + 1; }
        return "utf8";
    }
    export function run(): number {
        writeFileSync("/sandbox/" + "text", "é" + "😀", {encoding: option()});
        writeFileSync("/sandbox/" + "bytes", new Uint8Array([0,128,255]).subarray(1), option());
        return 1;
    }"#;
    let directory = tempfile::tempdir()?;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..3 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, 1.0);
        assert_eq!(fs::read(directory.path().join("text"))?, "é😀".as_bytes());
        assert_eq!(fs::read(directory.path().join("bytes"))?, [128, 255]);
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn symlink_resolution_stays_within_the_selected_preopen() -> Result<()> {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    fs::write(directory.path().join("inside"), b"before")?;
    fs::write(outside.path().join("file"), b"outside")?;
    symlink("inside", directory.path().join("allowed"))?;
    symlink(outside.path().join("file"), directory.path().join("escape"))?;
    symlink(outside.path(), directory.path().join("escaped-dir"))?;
    symlink("cycle", directory.path().join("cycle"))?;
    let source = "import {writeFileSync} from 'fs'; export function run(path: string): Result<number, number> { writeFileSync(path, 'after'); return 1; }";
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String,), (Result<f64, f64>,)>(&mut store, "run")?;
    for _ in 0..30 {
        assert_eq!(
            run.call_async(&mut store, ("/sandbox/allowed".into(),))
                .await?
                .0,
            Ok(1.0)
        );
        assert_eq!(fs::read(directory.path().join("inside"))?, b"after");
        for path in [
            "/sandbox/escape",
            "/sandbox/escaped-dir/file",
            "/sandbox/cycle",
        ] {
            assert!(
                run.call_async(&mut store, (path.into(),)).await?.0.is_err(),
                "{path}"
            );
            assert_eq!(fs::read(outside.path().join("file"))?, b"outside");
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn filesystem_bindings_preserve_user_functions_and_prune_unused_imports() -> Result<()> {
    for source in [
        "function writeFileSync(value: number): number { return value + 1; } export function run(): number { return writeFileSync(3); }",
        "import {writeFileSync} from 'fs'; function local(writeFileSync: number): number { return writeFileSync + 1; } export function run(): number { return local(3); }",
        "import fs from 'node:fs'; function local(fs: number): number { return fs + 1; } export function run(): number { return local(3); }",
        "import {writeFileSync as save} from 'node:fs'; export function run(): number { return 4; }",
    ] {
        let compiled =
            compile_typescript_waffle(source, "shadow.ts", &WaffleCompileOptions::default())?;
        assert!(!compiled.component_wat.unwrap().contains("wasi:filesystem/"));
        let (mut store, instance) = instantiate(source, WasiCtxBuilder::new().build()).await?;
        let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
        assert_eq!(run.call_async(&mut store, ()).await?.0, 4.0);
    }
    let source = "import fs from 'node:fs'; export function run(): Result<number, number> { fs['writeFileSync']('/missing', 'text'); return 1; }";
    let compiled =
        compile_typescript_waffle(source, "imports.ts", &WaffleCompileOptions::default())?;
    let wat = compiled.component_wat.unwrap();
    assert!(wat.contains("wasi:filesystem/types@0.3.0"));
    assert!(wat.contains("wasi:filesystem/preopens@0.3.0"));
    for unrelated in [
        "wasi:cli/",
        "wasi:clocks/",
        "wasi:random/",
        "wasi:http/",
        "@0.2.",
    ] {
        assert!(!wat.contains(unrelated), "{unrelated}");
    }
    let (mut store, instance) = instantiate(source, WasiCtxBuilder::new().build()).await?;
    let run = instance.get_typed_func::<(), (Result<f64, f64>,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, ()).await?.0, Err(1.0));
    store.assert_concurrent_state_empty();
    assert!(store.data().table.is_empty());
    Ok(())
}

#[test]
fn unsupported_filesystem_syntax_is_diagnosed_before_frontend_effects_are_lost() {
    for body in [
        "fs.writeFileSync('/file')",
        "fs.writeFileSync('/file', true)",
        "fs.writeFileSync(42, 'text')",
        "fs.writeFileSync('/file', 'text', 'utf8', 'extra')",
        "fs.writeFileSync('/file', 'text', {get flag() { return 'w'; }})",
        "fs.writeFileSync('/file', 'text', ({get flag() { return 'w'; }}))",
        "fs.writeFileSync('/file', 'text', ({get flag() { return 'w'; }} as any))",
        "fs.writeFileSync('/file', 'text', {...{flag: 'w'}})",
        "fs.writeFileSync('/file', 'text', ({...{flag: 'w'}} as any))",
        "fs.writeFileSync('/file', 'text', {['flag']: 'w'})",
        "fs.writeFileSync('/file', 'text', {__proto__: {}})",
        "fs.writeFileSync('/file', 'text', {flag() { return 'w'; }})",
        "fs.writeFileSync(...['/file', 'text'])",
        "const save = fs.writeFileSync; save('/file', 'text')",
        "const options = {flag: 'w'}; fs.writeFileSync('/file', 'text', options)",
        "fs.readFileSync('/file')",
        "fs.mkdirSync('/dir')",
        "fs['write' + 'FileSync']('/file', 'text')",
    ] {
        let source =
            format!("import fs from 'fs'; export function run(): number {{ {body}; return 1; }}");
        assert!(
            compile_typescript_waffle(&source, "invalid.ts", &WaffleCompileOptions::default())
                .is_err(),
            "{body}"
        );
    }
}

async fn instantiate(source: &str, context: WasiCtx) -> Result<(Store<Host>, Instance)> {
    instantiate_with(source, context, |_| Ok(())).await
}

async fn instantiate_with(
    source: &str,
    context: WasiCtx,
    configure: impl FnOnce(&mut Linker<Host>) -> Result<()>,
) -> Result<(Store<Host>, Instance)> {
    let compiled =
        compile_typescript_waffle(source, "filesystem.ts", &WaffleCompileOptions::default())?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_threading(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    configure(&mut linker)?;
    let mut store = Store::new(
        &engine,
        Host {
            context,
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(65_536).build(),
        },
    );
    store.limiter(|host| &mut host.limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    Ok((store, instance))
}
