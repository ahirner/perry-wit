use std::fs;

use crate::waffle_fixture::compile_typescript_waffle;
use anyhow::Result;
use perry_wit::waffle_backend::WaffleCompileOptions;
use wasmtime_wasi::{FsPerms, WasiCtxBuilder};

use super::instantiate;

#[path = "source_directory_lifecycle.rs"]
mod lifecycle;

#[tokio::test(flavor = "current_thread")]
async fn directory_materialization_grows_beyond_one_page_without_a_fixed_entry_limit() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    for index in 0..1000 {
        fs::write(directory.path().join(format!("entry-{index}-😀")), b"")?;
    }
    let source = r#"import {readdir} from 'fs/promises'; export async function run(): Promise<number> { return (await readdir('/sandbox')).length; }"#;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    store.data_mut().limits = wasmtime::StoreLimitsBuilder::new()
        .memory_size(2 * 1024 * 1024)
        .build();
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..3 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, 1000.0);
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn supported_metadata_and_directory_operations_match_node() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = r#"
    import fs from 'node:fs/promises';
    export async function run(path: string): Promise<string> {
        (await fs.mkdir(path + '/child'));
        (await fs.writeFile(path + '/child/é😀', 'é😀'));
        const names = (await fs.readdir(path + '/child', 'utf8'));
        const file = (await fs.stat(path + '/child/é😀'));
        if (!file.isFile()) { return 'wrong type'; }
        if (file.size !== 6) { return 'wrong size'; }
        if (names.length !== 1) { return 'wrong count'; }
        (await fs.unlink(path + '/child/é😀'));
        (await fs.rmdir(path + '/child'));
        if ((await exists(path + '/child'))) { return 'still exists'; }
        return names[0].slice(0);
    }
import { stat as existenceStat } from "node:fs/promises";
async function exists(path: string): Promise<boolean> { try { await existenceStat(path); return true; } catch { return false; } }
"#;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String,), (String,)>(&mut store, "run")?;
    let actual = run.call_async(&mut store, ("/sandbox".into(),)).await?.0;
    let entry = directory.path().join("compare.mts");
    fs::write(
        &entry,
        format!(
            "{source}\nprocess.stdout.write(await run({}));",
            serde_json::to_string(&directory.path())?
        ),
    )?;
    let node = std::process::Command::new("node")
        .arg("--disable-warning=ExperimentalWarning")
        .arg(&entry)
        .output()?;
    assert!(
        node.status.success(),
        "{}",
        String::from_utf8_lossy(&node.stderr)
    );
    assert_eq!(actual.as_bytes(), node.stdout);
    store.assert_concurrent_state_empty();
    assert!(store.data().table.is_empty());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn stat_layout_preserves_signed_timestamps_sizes_and_descriptor_variants() -> Result<()> {
    use wasmtime::component::Resource;
    use wasmtime_wasi::filesystem::Descriptor;
    use wasmtime_wasi::p3::bindings::{
        clocks::system_clock::Instant,
        filesystem::types::{DescriptorStat, DescriptorType, ErrorCode, PathFlags},
    };
    type Arguments = (Resource<Descriptor>, PathFlags, String);
    let directory = tempfile::tempdir()?;
    let source = r#"
    import {stat} from 'fs/promises';
    export async function run(path: string, field: number): Promise<number> {
        if (field === 4) { if ((await exists(path))) { return 1; } return 0; }
        try {
            const stats = (await stat(path));
            let index = 0;
            while (index < 2000) { const temporary = new Uint8Array(256); index = index + 1; }
            if (field === 0) { return stats.size; }
            if (field === 1) { return stats.mtimeMs; }
            if (stats.isFile()) { return 5; }
            if (stats.isDirectory()) { return 2; }
            return 7;
        } catch (error) { return 0 - error; }
    }
import { stat as existenceStat } from "node:fs/promises";
async function exists(path: string): Promise<boolean> { try { await existenceStat(path); return true; } catch { return false; } }
"#;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
        .build();
    let (mut store, instance) = super::instantiate_with(source, context, |linker| {
        linker.allow_shadowing(true);
        linker
            .instance("wasi:filesystem/types@0.3.0")?
            .func_wrap_concurrent(
                "[method]descriptor.stat-at",
                |_, (_, flags, path): Arguments| {
                    Box::pin(async move {
                        assert_eq!(flags, PathFlags::SYMLINK_FOLLOW);
                        let result = if path == "error" {
                            Err(ErrorCode::Other(Some("é😀".repeat(300))))
                        } else {
                            Ok(DescriptorStat {
                                type_: match path.as_str() {
                                    "file" => DescriptorType::RegularFile,
                                    "." => DescriptorType::Directory,
                                    _ => DescriptorType::Other(Some("kind 😀".repeat(100))),
                                },
                                link_count: 17,
                                size: u64::MAX,
                                data_access_timestamp: Some(Instant {
                                    seconds: 888,
                                    nanoseconds: 123,
                                }),
                                data_modification_timestamp: if path == "file" {
                                    Some(Instant {
                                        seconds: -2,
                                        nanoseconds: 750_000_000,
                                    })
                                } else {
                                    None
                                },
                                status_change_timestamp: Some(Instant {
                                    seconds: 999,
                                    nanoseconds: 234,
                                }),
                            })
                        };
                        Ok((result,))
                    })
                },
            )?;
        Ok(())
    })
    .await?;
    let run = instance.get_typed_func::<(String, f64), (f64,)>(&mut store, "run")?;
    for _ in 0..10 {
        for (path, field, expected) in [
            ("file", 0.0, u64::MAX as f64),
            ("file", 1.0, -1250.0),
            ("file", 2.0, 5.0),
            ("", 1.0, 0.0),
            ("", 2.0, 2.0),
            ("other", 2.0, 7.0),
            ("error", 0.0, -37.0),
            ("error", 4.0, 0.0),
            ("file", 4.0, 1.0),
        ] {
            assert_eq!(
                run.call_async(&mut store, (format!("/sandbox/{path}"), field))
                    .await?
                    .0,
                expected
            );
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn metadata_confines_paths_denies_mutations_and_protects_preopen_roots() -> Result<()> {
    let outer = tempfile::tempdir()?;
    let inner = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    fs::create_dir(outer.path().join("nested"))?;
    fs::write(outer.path().join("nested/file"), b"outer")?;
    fs::write(inner.path().join("file"), b"in")?;
    fs::write(outside.path().join("keep"), b"outside")?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), inner.path().join("escape"))?;
    let source = r#"
    import fs from 'fs/promises';
    export async function run(path: string, op: number): Promise<Result<number, number>> {
        if (op === 0) { const stats = (await fs.stat(path)); if (stats.isDirectory()) { return 100; } return stats.size; }
        if (op === 1) { if ((await exists(path))) { return 1; } return 0; }
        if (op === 2) { (await fs.mkdir(path, undefined)); }
        if (op === 3) { (await fs.unlink(path, undefined)); }
        if (op === 4) { (await fs.rmdir(path, undefined)); }
        if (op === 5) { return (await fs.readdir(path)).length; }
        return 0;
    }
import { stat as existenceStat } from "node:fs/promises";
async function exists(path: string): Promise<boolean> { try { await existenceStat(path); return true; } catch { return false; } }
"#;
    for permissions in [FsPerms::ReadWrite, FsPerms::ReadOnly] {
        let context = WasiCtxBuilder::new()
            .preopened_dir(outer.path(), "/sandbox", permissions)?
            .preopened_dir(inner.path(), "/sandbox/nested", permissions)?
            .build();
        let (mut store, instance) = instantiate(source, context).await?;
        let run =
            instance.get_typed_func::<(String, f64), (Result<f64, f64>,)>(&mut store, "run")?;
        for _ in 0..20 {
            for (path, op, expected) in [
                ("/sandbox/nested/file", 0.0, Ok(2.0)),
                ("/sandbox//nested/no/../file", 0.0, Ok(2.0)),
                ("/sandbox/nested", 0.0, Ok(100.0)),
                ("/sandbox/nested", 1.0, Ok(1.0)),
                ("/sandbox/nested", 2.0, Err(7.0)),
                ("/sandbox/nested", 3.0, Err(31.0)),
                ("/sandbox/nested", 4.0, Err(31.0)),
                ("/sandboxish", 0.0, Err(1.0)),
                ("/sandbox/../../outside", 0.0, Err(1.0)),
                ("../sandbox", 1.0, Ok(0.0)),
                ("/sandbox/nested/missing", 0.0, Err(20.0)),
                ("/sandbox/nested/missing", 1.0, Ok(0.0)),
                ("/sandbox/nested/no\0file", 0.0, Err(12.0)),
                ("/sandbox/nested/no\0file", 1.0, Ok(0.0)),
                ("/sandbox/nested/file", 5.0, Err(24.0)),
                (
                    "/sandbox/nested/file",
                    4.0,
                    Err(if permissions == FsPerms::ReadOnly {
                        31.0
                    } else {
                        24.0
                    }),
                ),
            ] {
                assert_eq!(
                    run.call_async(&mut store, (path.into(), op)).await?.0,
                    expected,
                    "{path} op={op}"
                );
                store.assert_concurrent_state_empty();
                assert!(store.data().table.is_empty());
            }
            #[cfg(unix)]
            for (op, expected) in [
                (0.0, Err(31.0)),
                (1.0, Ok(0.0)),
                (3.0, Err(31.0)),
                (5.0, Err(31.0)),
            ] {
                assert_eq!(
                    run.call_async(&mut store, ("/sandbox/nested/escape/keep".into(), op))
                        .await?
                        .0,
                    expected
                );
                store.assert_concurrent_state_empty();
                assert!(store.data().table.is_empty());
            }
        }
        if permissions == FsPerms::ReadOnly {
            for (path, op) in [("/sandbox/nested/new", 2.0), ("/sandbox/nested/file", 3.0)] {
                assert_eq!(
                    run.call_async(&mut store, (path.into(), op)).await?.0,
                    Err(31.0)
                );
            }
        }
    }
    assert_eq!(fs::read(outside.path().join("keep"))?, b"outside");
    assert_eq!(fs::read(inner.path().join("file"))?, b"in");
    assert_eq!(fs::read(outer.path().join("nested/file"))?, b"outer");
    let root = tempfile::tempdir()?;
    for mount in [".", "/"] {
        let context = WasiCtxBuilder::new()
            .preopened_dir(root.path(), mount, FsPerms::ReadWrite)?
            .build();
        let (mut store, instance) = instantiate(source, context).await?;
        let run =
            instance.get_typed_func::<(String, f64), (Result<f64, f64>,)>(&mut store, "run")?;
        for path in ["", ".", "child/.."] {
            for (op, expected) in [
                (0.0, Ok(100.0)),
                (1.0, Ok(1.0)),
                (2.0, Err(7.0)),
                (3.0, Err(31.0)),
                (4.0, Err(31.0)),
                (5.0, Ok(0.0)),
            ] {
                assert_eq!(
                    run.call_async(&mut store, (path.into(), op)).await?.0,
                    expected,
                    "mount={mount} path={path} op={op}"
                );
                store.assert_concurrent_state_empty();
                assert!(store.data().table.is_empty());
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn metadata_and_directory_mutations_preserve_live_stats_and_release_resources() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let source = r#"
    import disk from 'node:fs/promises';
    import type {Stats} from 'fs';
    function retain(value: Stats): Stats { return value; }
    export async function run(): Promise<number> {
        let index = 0;
        let count = 0;
        const root = retain((await disk.stat('/sandbox', {bigint:false, throwIfNoEntry:true})));
        const alias = root;
        while (index < 1000) {
            (await disk.mkdir('/sandbox/child'));
            (await disk.writeFile('/sandbox/child/file', 'é😀'));
            const file = retain((await disk.stat('/sandbox/child/file')));
            const temporary = new Uint8Array(256);
            if (file.isFile()) { if (!file.isDirectory()) { if (file.mtimeMs > 0) { count = count + file.size; } } }
            (await disk.unlink('/sandbox/child/file'));
            (await disk.rmdir('/sandbox/child'));
            if ((await exists('/sandbox/child'))) { return -1; }
            index = index + 1;
        }
        if (root !== alias) { return -2; }
        if (!root.isDirectory()) { return -3; }
        if (root.isFile()) { return -4; }
        if (typeof root !== 'object') { return -5; }
        return count;
    }
import { stat as existenceStat } from "node:fs/promises";
async function exists(path: string): Promise<boolean> { try { await existenceStat(path); return true; } catch { return false; } }
"#;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..3 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, 6000.0);
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
        assert_eq!(fs::read_dir(directory.path())?.count(), 0);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn directory_names_survive_growth_helper_calls_and_collection() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let names: Vec<_> = (0..80).map(|index| format!("é😀-{index}")).collect();
    for name in &names {
        fs::write(directory.path().join(name), b"")?;
    }
    fs::create_dir(directory.path().join("empty"))?;
    let source = r#"
    import {readdir} from 'fs/promises';
    function retain(names: string[]): string[] { return names; }
    function text(names: string[]): string {
        let result = '';
        let index = 0;
        while (index < names.length) {
            if (index > 0) { result = result + '|'; }
            result = result + names[index].slice(0);
            index = index + 1;
        }
        return result;
    }
    export async function run(path: string): Promise<string> {
        const names = retain((await readdir(path, {encoding:'UTF-8', recursive:false, withFileTypes:false})));
        let index = 0;
        while (index < 2000) { const temporary = new Uint8Array(256); index = index + 1; }
        if (names.length > 0) { if (names[0] === undefined) { return 'lost'; } }
        return text(names);
    }"#;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadOnly)?
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(String,), (String,)>(&mut store, "run")?;
    let mut expected = names;
    expected.push("empty".into());
    expected.sort();
    for _ in 0..30 {
        let result = run.call_async(&mut store, ("/sandbox".into(),)).await?.0;
        let mut actual: Vec<_> = result.split('|').map(str::to_owned).collect();
        actual.sort();
        assert_eq!(actual, expected);
        assert_eq!(
            run.call_async(&mut store, ("/sandbox/empty".into(),))
                .await?
                .0,
            ""
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn metadata_options_are_checked_after_effects_and_before_io() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("keep"), b"keep")?;
    for (call, expected) in [
        (
            "(await stat(path(state), {bigint: flag(state, true), bigint: flag(state, false), throwIfNoEntry: flag(state,true)})).size",
            4.0,
        ),
        (
            "(await stat(path(state), {bigint: flag(state, true), throwIfNoEntry: flag(state,true)})).size",
            -1221.0,
        ),
        (
            "(await stat(path(state), {unknown: flag(state,false), bigint: flag(state,false)})).size",
            -1221.0,
        ),
        (
            "(await stat(path(state), {bigint: undefined})).size",
            -1201.0,
        ),
        (
            "(await stat(path(state), {throwIfNoEntry: false})).size",
            -1201.0,
        ),
        ("(await stat(path(state), null)).size", 4.0),
        ("(await stat(path(state), undefined)).size", 4.0),
        (
            "(await readdir(path(state), {recursive: flag(state,true), recursive: flag(state,false), withFileTypes: flag(state,true)})).length",
            -1231.0,
        ),
        ("(await readdir(path(state), 'binary')).length", -1201.0),
        (
            "(await readdir(path(state), {encoding: 42})).length",
            -1201.0,
        ),
        (
            "(await mkdir(path(state), {recursive: flag(state,false)}))",
            -1211.0,
        ),
        (
            "(await unlink(path(state), {unknown: flag(state,true)}))",
            -1211.0,
        ),
        ("(await rmdir(path(state), null))", -1201.0),
    ] {
        let call = if call.starts_with("(await stat") || call.starts_with("(await readdir") {
            format!("return {call}")
        } else {
            format!("{call}; return 0")
        };
        let source = format!(
            r#"
        import {{stat, readdir, mkdir, unlink, rmdir}} from 'fs/promises';
        function path(state: Uint8Array): string {{ state[0] = state[0] + 1; return '/sandbox/keep'; }}
        function flag(state: Uint8Array, value: boolean): boolean {{ state[0] = state[0] + 10; return value; }}
        export async function run(): Promise<number> {{
            const state = new Uint8Array(1);
            try {{ {call}; }} catch (error) {{ return 0 - error * 100 - state[0]; }}
        }}"#
        );
        let context = WasiCtxBuilder::new()
            .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
            .build();
        let (mut store, instance) = instantiate(&source, context).await?;
        let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
        assert_eq!(run.call_async(&mut store, ()).await?.0, expected, "{call}");
        assert_eq!(fs::read(directory.path().join("keep"))?, b"keep");
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

#[test]
fn metadata_types_and_unsupported_source_forms_are_diagnosed() {
    for body in [
        "return (await fs.stat('/file'))",
        "let stats = (await fs.stat('/file')); stats = true; return stats.size",
        "return (await fs.stat('/file')).unknown",
        "return (await fs.stat('/file')).isFile(1)",
        "return (await fs.stat('/file')).isSymbolicLink()",
        "return (await fs.stat('/file', {get bigint() {return false}})).size",
        "return (await exists('/file', {}))",
        "return (await fs.readdir('/dir', {...{recursive:false}})).length",
        "const options = {get recursive() {return false;}}; return (await fs.readdir('/dir', options)).length",
        "return (await fs.readdir('/dir'))",
        "let names = (await fs.readdir('/dir')); names = true; return names.length",
    ] {
        let source = format!(
            "import fs from 'fs/promises'; export async function run(): Promise<number> {{ {body}; }}"
        );
        assert!(
            compile_typescript_waffle(
                &source,
                "invalid-metadata.ts",
                &WaffleCompileOptions::default()
            )
            .is_err(),
            "{body}"
        );
    }
}
