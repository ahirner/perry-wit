use std::{fs, process::Command};

use anyhow::Result;
use wasmtime::StoreLimitsBuilder;
use wasmtime_wasi::{FsPerms, WasiCtxBuilder};

use super::instantiate;

use super::input::{ControlledProducer, Observations};
use super::{Host, instantiate_with};
use std::sync::{Arc, Mutex, atomic::Ordering};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio::time::timeout;
use wasmtime::StoreContextMut;
use wasmtime::component::{FutureReader, Resource, StreamReader};
use wasmtime_wasi::filesystem::Descriptor;
use wasmtime_wasi::p3::bindings::filesystem::types::ErrorCode;

type ReadArguments = (Resource<Descriptor>, u64);

#[tokio::test(flavor = "current_thread")]
async fn binary_reads_return_independent_views_and_preserve_all_bytes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = r#"
    import fs from "node:fs";
    export function run(path: string): Uint8Array {
        const bytes = fs.readFileSync(path);
        const other = fs.readFileSync(path, {encoding: 'BiNaRy', flag: 'r'});
        if (other.length > 0) { other[0] = 71; }
        let index = 0;
        while (index < 2000) { const temporary = new Uint8Array(128); index = index + 1; }
        return bytes;
    }"#;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String,), (Vec<u8>,)>(&mut store, "run")?;
    for _ in 0..10 {
        for size in [0, 1, 8191, 8192, 8193] {
            let bytes: Vec<_> = (0..size).map(|index| (index * 37) as u8).collect();
            fs::write(directory.path().join("input"), &bytes)?;
            assert_eq!(
                run.call_async(&mut store, ("/sandbox/input".into(),))
                    .await?
                    .0,
                bytes
            );
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn text_reads_preserve_boms_nuls_and_scalar_lengths_and_reject_invalid_utf8() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = r#"
    import {readFileSync as read} from "fs";
    export function run(path: string): Result<string, number> {
        const text = read(path, {encoding: 'UTF-8', flag: 'r'});
        let index = 0;
        while (index < 2000) { const temporary = new Uint8Array(256); index = index + 1; }
        return text;
    }"#;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String,), (Result<String, f64>,)>(&mut store, "run")?;
    let cases: Vec<Vec<u8>> = vec![
        vec![],
        b"ascii\0".to_vec(),
        "\u{feff}é中😀\0".as_bytes().to_vec(),
        ("a".repeat(8191) + "😀é").into_bytes(),
        vec![128],
        vec![192, 128],
        vec![237, 160, 128],
        vec![244, 144, 128, 128],
        vec![240, 159, 152],
        [b"valid".as_slice(), &[255]].concat(),
    ];
    for _ in 0..10 {
        for bytes in &cases {
            fs::write(directory.path().join("input"), bytes)?;
            let expected = String::from_utf8(bytes.clone()).map_err(|_| 9.0);
            assert_eq!(
                run.call_async(&mut store, ("/sandbox/input".into(),))
                    .await?
                    .0,
                expected
            );
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    let source = "import {readFileSync} from 'fs'; export function run(): number { return readFileSync('/sandbox/input', 'utf8').length; }";
    fs::write(directory.path().join("input"), "\u{feff}é😀\0")?;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, ()).await?.0, 4.0);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn read_options_preserve_effects_and_fail_before_preopen_resolution() -> Result<()> {
    let source = r#"
    import {readFileSync} from "node:fs";
    function path(state: Uint8Array, value: string): string { state[0] = state[0] * 3 + 1; return value; }
    function flag(state: Uint8Array, value: string): string { state[0] = state[0] * 3 + 2; return value; }
    export function run(target: string, value: string): number {
        const state = new Uint8Array(1);
        try { readFileSync(path(state, target), {encoding: "utf8", flag: flag(state, value)}); return state[0]; }
        catch (error) { return error * 100 + state[0]; }
    }"#;
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("file"), "keep")?;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String, String), (f64,)>(&mut store, "run")?;
    for flag in ["w", "r+", "a", "R", "", "r\0"] {
        for path in ["/sandbox/file", "/sandbox/missing", "/outside/file"] {
            assert_eq!(
                run.call_async(&mut store, (path.into(), flag.into()))
                    .await?
                    .0,
                1205.0
            );
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    assert_eq!(
        run.call_async(&mut store, ("/sandbox/file".into(), "r".into()))
            .await?
            .0,
        5.0
    );
    assert_eq!(
        run.call_async(&mut store, ("/sandbox/missing".into(), "r".into()))
            .await?
            .0,
        2005.0
    );
    assert_eq!(
        run.call_async(&mut store, ("/outside/file".into(), "r".into()))
            .await?
            .0,
        105.0
    );
    assert_eq!(
        run.call_async(&mut store, ("/sandbox".into(), "r".into()))
            .await?
            .0,
        1405.0
    );
    assert_eq!(
        run.call_async(&mut store, ("/sandbox/../outside".into(), "r".into()))
            .await?
            .0,
        105.0
    );
    assert_eq!(
        run.call_async(&mut store, ("/sandbox/no\0file".into(), "r".into()))
            .await?
            .0,
        1205.0
    );
    assert_eq!(fs::read(directory.path().join("file"))?, b"keep");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn static_read_options_and_repeated_reads_keep_their_result_contract() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("text"), "\u{feff}é😀\0")?;
    fs::write(directory.path().join("bad"), [255])?;
    for (options, expected) in [
        ("undefined", 10.0),
        ("null", 10.0),
        ("{}", 10.0),
        ("{encoding: null}", 10.0),
        ("{encoding: undefined}", 10.0),
        ("{flag: 'r'}", 10.0),
        ("'binary'", 10.0),
        ("'BiNaRy'", 10.0),
        ("'UTF8'", 4.0),
        ("'uTf-8'", 4.0),
        ("{encoding:'utf8', flag:'r'}", 4.0),
        ("{encoding:'utf8', encoding:null}", 10.0),
        ("{encoding:null, encoding:'utf8'}", 4.0),
        ("'utf8\\0'", -12.0),
        ("'utf-8\\0'", -12.0),
        ("'hex'", -12.0),
        ("''", -12.0),
        ("false", -12.0),
        ("42", -12.0),
        ("{mode: 384}", -12.0),
        ("{flag: null}", -12.0),
        ("{flag: undefined}", -12.0),
        ("{encoding: 42}", -12.0),
        ("{unknown: true}", -12.0),
    ] {
        let source = format!(
            "import {{readFileSync}} from 'fs'; export function run(): number {{ try {{ return readFileSync('/sandbox/text', {options}).length; }} catch (error) {{ return 0 - error; }} }}"
        );
        let context = WasiCtxBuilder::new()
            .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
            .build();
        let (mut store, instance) = instantiate(&source, context).await?;
        let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
        assert_eq!(
            run.call_async(&mut store, ()).await?.0,
            expected,
            "{options}"
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    let source = r#"
    import {readFileSync, writeFileSync} from 'fs';
    export function run(): number {
        let index = 0;
        let count = 0;
        let bad = true;
        while (index < 2000) {
            try {
                if (bad) { readFileSync('/sandbox/bad', 'utf8'); }
                else { count = count + readFileSync('/sandbox/text', 'utf8').length; }
            } catch (error) { count = count + error; }
            bad = bad === false;
            index = index + 1;
        }
        const data = readFileSync('/sandbox/text');
        writeFileSync('/sandbox/copy', data.subarray(3));
        return count;
    }"#;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..3 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, 13000.0);
        assert_eq!(fs::read(directory.path().join("copy"))?, "é😀\0".as_bytes());
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[test]
fn read_result_types_and_unsupported_options_are_explicit() {
    use crate::waffle_fixture::compile_typescript_waffle;
    use perry_wit::waffle_backend::WaffleCompileOptions;

    for function in [
        "export function run(): string { return readFileSync('/file'); }",
        "export function run(): Uint8Array { return readFileSync('/file', 'utf8'); }",
        "export function run(): number { return readFileSync('/file', 'utf8'); }",
        "export function run(encoding: string): string { return readFileSync('/file', encoding); }",
        "export function run(encoding: string): string { return readFileSync('/file', {encoding}); }",
        "export function run(): number { const options = {get encoding() {return 'utf8';}}; return readFileSync('/file', options).length; }",
        "export function run(): number { return readFileSync('/file', ({get encoding() { return 'utf8'; }} as any)).length; }",
        "export function run(): number { return readFileSync('/file', {...{encoding:'utf8'}}).length; }",
        "export function run(): number { return readFileSync('/file', {['encoding']:'utf8'}).length; }",
        "export function run(): number { return readFileSync('/file', {__proto__:{}}).length; }",
        "export function run(): number { return readFileSync('/file', 'utf8', 'extra').length; }",
        "export function run(): number { return readFileSync().length; }",
    ] {
        let source = format!("import {{readFileSync}} from 'fs'; {function}");
        assert!(
            compile_typescript_waffle(&source, "invalid-read.ts", &WaffleCompileOptions::default())
                .is_err(),
            "{function}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn file_reads_materialize_large_results_without_a_fixed_size_limit() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let bytes: Vec<_> = (0..4 * 1024 * 1024 + 7)
        .map(|index| (index * 37) as u8)
        .collect();
    fs::write(directory.path().join("input"), &bytes)?;
    let source = "import {readFileSync} from 'fs'; export function run(): Uint8Array { return readFileSync('/sandbox/input'); }";
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    store.data_mut().limits = StoreLimitsBuilder::new()
        .memory_size(32 * 1024 * 1024)
        .build();
    let run = instance.get_typed_func::<(), (Vec<u8>,)>(&mut store, "run")?;
    for _ in 0..3 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, bytes);
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn supported_file_reads_match_node() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("input"), "\u{feff}é😀\0")?;
    let source = r#"
    import {readFileSync} from "fs";
    export function run(path: string): string { return readFileSync(path, 'utf8'); }
    "#;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String,), (String,)>(&mut store, "run")?;
    let result = run
        .call_async(&mut store, ("/sandbox/input".into(),))
        .await?
        .0;
    let entry = directory.path().join("compare.mts");
    fs::write(
        &entry,
        format!(
            "{source}\nprocess.stdout.write(run({}));",
            serde_json::to_string(&directory.path().join("input"))?
        ),
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
    assert_eq!(result.as_bytes(), node.stdout);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn read_owners_survive_partial_input_sibling_collection_and_separate_completion() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("input"),
        b"replaced by controlled producer",
    )?;
    let source = r#"
    import {readFileSync} from "fs";
    async function read(): Promise<string> {
        try { return readFileSync('/sandbox/input', 'utf8'); }
        catch (error) { if (error === 37) { return 'failed'; } throw error; }
    }
    export async function run(encoding: string): Promise<string> {
        const pending = read();
        let index = 0;
        while (index < 2000) { const temporary = new Uint8Array(1024); index = index + 1; }
        console.log('collected');
        return await pending + await pending;
    }"#;
    let dynamic = r#"
    import {readFileSync} from "fs";
    async function read(encoding: string): Promise<string | Uint8Array> {
        try { return readFileSync('/sandbox/input', {encoding}); }
        catch (error) { if (error === 37) { return 'failed'; } throw error; }
    }
    function text(value: string | Uint8Array): string {
        if (typeof value === "string") { return value; }
        return new TextDecoder('utf8', {ignoreBOM: true}).decode(value);
    }
    export async function run(encoding: string): Promise<string> {
        const pending = read(encoding);
        let index = 0;
        while (index < 2000) { const temporary = new Uint8Array(1024); index = index + 1; }
        console.log('collected');
        const first = text(await pending);
        index = 0;
        while (index < 2000) { const temporary = new Uint8Array(1024); index = index + 1; }
        return first + text(await pending);
    }"#;
    for (source, encoding, failure) in [
        (source, "utf8", false),
        (source, "utf8", true),
        (dynamic, "utf8", false),
        (dynamic, "binary", false),
        (dynamic, "binary", true),
    ] {
        let observations = Arc::new(Observations::default());
        let (sender, receiver) = mpsc::channel(8);
        sender.send(Ok(vec![239, 187, 191, 240])).await?;
        let receiver = Arc::new(Mutex::new(Some(receiver)));
        let finish = Arc::new(Notify::new());
        let completion_pending = Arc::new(Notify::new());
        let output = wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(1024);
        let context = WasiCtxBuilder::new()
            .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
            .stdout(output.clone())
            .build();
        let (mut store, instance) = instantiate_with(source, context, |linker| {
            linker.allow_shadowing(true);
            let observations = observations.clone();
            let finish = finish.clone();
            let completion_pending = completion_pending.clone();
            linker.instance("wasi:filesystem/types@0.3.0")?.func_wrap(
                "[method]descriptor.read-via-stream",
                move |mut store: StoreContextMut<'_, Host>, (descriptor, offset): ReadArguments| {
                    store.data().table.get(&descriptor)?;
                    assert_eq!(offset, 0);
                    let stream = StreamReader::new(
                        &mut store,
                        ControlledProducer {
                            receiver: receiver.lock().unwrap().take().unwrap(),
                            observations: observations.clone(),
                        },
                    )?;
                    let observations = observations.clone();
                    let finish = finish.clone();
                    let completion_pending = completion_pending.clone();
                    let completion = FutureReader::new(&mut store, async move {
                        observations.closed.notified().await;
                        completion_pending.notify_one();
                        finish.notified().await;
                        wasmtime::error::Ok(if failure {
                            Err(ErrorCode::Other(Some("é😀".repeat(300))))
                        } else {
                            Ok(())
                        })
                    })?;
                    Ok(((stream, completion),))
                },
            )?;
            Ok(())
        })
        .await?;
        let run = instance.get_typed_func::<(String,), (String,)>(&mut store, "run")?;
        let mut invocation = Box::pin(run.call_async(&mut store, (encoding.into(),)));
        tokio::select! {
            result = &mut invocation => panic!("completed before input: {result:?}"),
            () = observations.pending.notified() => {}
        }
        tokio::select! {
            result = &mut invocation => panic!("completed before sibling collection: {result:?}"),
            result = timeout(Duration::from_secs(5), async { while output.contents().is_empty() { tokio::task::yield_now().await; } }) => { result?; }
        }
        assert_eq!(observations.bytes.load(Ordering::SeqCst), 4);
        sender.send(Ok(vec![])).await?;
        sender.send(Ok(vec![159, 152])).await?;
        sender.send(Ok(vec![128, 195, 169, 0])).await?;
        drop(sender);
        tokio::select! {
            result = &mut invocation => panic!("returned at EOF before completion: {result:?}"),
            () = completion_pending.notified() => {}
        }
        assert!(
            timeout(Duration::from_millis(1), &mut invocation)
                .await
                .is_err()
        );
        finish.notify_one();
        let result = timeout(Duration::from_secs(5), invocation).await?;
        assert_eq!(
            result?.0,
            if failure {
                "failedfailed"
            } else {
                "\u{feff}😀é\0\u{feff}😀é\0"
            }
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
        assert!(observations.dropped.load(Ordering::SeqCst));
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn pending_read_disposal_and_producer_traps_release_owners_without_guest_finally()
-> Result<()> {
    let source = r#"
    import {readFileSync} from 'fs';
    export function run(): string {
        try { return readFileSync('/sandbox/input', 'utf8'); }
        finally { console.log('unexpected cleanup'); }
    }"#;
    for trap in [false, true] {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("input"), b"controlled")?;
        let observations = Arc::new(Observations::default());
        let (sender, receiver) = mpsc::channel(1);
        sender.send(Ok(vec![195])).await?;
        let receiver = Arc::new(Mutex::new(Some(receiver)));
        let output = wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(1024);
        let context = WasiCtxBuilder::new()
            .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
            .stdout(output.clone())
            .build();
        let (mut store, instance) = instantiate_with(source, context, |linker| {
            linker.allow_shadowing(true);
            let observations = observations.clone();
            linker.instance("wasi:filesystem/types@0.3.0")?.func_wrap(
                "[method]descriptor.read-via-stream",
                move |mut store: StoreContextMut<'_, Host>, (descriptor, _): ReadArguments| {
                    store.data().table.get(&descriptor)?;
                    let stream = StreamReader::new(
                        &mut store,
                        ControlledProducer {
                            receiver: receiver.lock().unwrap().take().unwrap(),
                            observations: observations.clone(),
                        },
                    )?;
                    let future = FutureReader::new(&mut store, async {
                        wasmtime::error::Ok(Ok::<(), ErrorCode>(()))
                    })?;
                    Ok(((stream, future),))
                },
            )?;
            Ok(())
        })
        .await?;
        let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
        let mut invocation = Box::pin(run.call_async(&mut store, ()));
        tokio::select! {
            result = &mut invocation => panic!("completed before input: {result:?}"),
            () = observations.pending.notified() => {}
        }
        if trap {
            sender.send(Err("controlled read trap".into())).await?;
            let error = timeout(Duration::from_secs(5), invocation)
                .await?
                .unwrap_err();
            assert!(format!("{error:#}").contains("controlled read trap"));
        } else {
            drop(invocation);
        }
        drop(store);
        assert!(sender.is_closed());
        assert!(observations.dropped.load(Ordering::SeqCst));
        assert!(output.contents().is_empty());
    }
    Ok(())
}
