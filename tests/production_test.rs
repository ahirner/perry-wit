use anyhow::Result;
use perry_wit::{CompileOptions, compile_typescript};

#[test]
fn production_command_runs_with_the_nix_p3_host() -> Result<()> {
    let compiled = compile_typescript(
        r#"
      export function runRun():{ok:true}|{ok:false} {
        console.log('native P3: 漢🙂');
        return {ok:true};
      }
    "#,
        "command.ts",
        &CompileOptions::default(),
    )?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("command.wasm");
    std::fs::write(&path, compiled.stripped.unwrap())?;
    let output = std::process::Command::new("wasmtime")
        .args([
            "run",
            "-C",
            "cache=n",
            "-S",
            "p3=y",
            "-W",
            "component-model-async=y",
            "-W",
            "component-model-more-async-builtins=y",
            "-W",
            "component-model-async-stackful=y",
            "-W",
            "component-model-threading=y",
        ])
        .arg(path)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout)?, "native P3: 漢🙂\n");
    Ok(())
}

#[test]
fn cli_custom_world_preserves_multiple_exports_and_core_output() -> Result<()> {
    use std::{fs, process::Command};
    use wasmtime::component::{Component, Linker};
    use wasmtime::{Engine, Store, StoreLimits, StoreLimitsBuilder};
    let directory = tempfile::tempdir()?;
    let wit = directory.path().join("wit");
    fs::create_dir(&wit)?;
    fs::write(
        wit.join("world.wit"),
        "package test:production; world task { export echo: func(input: string) -> string; export length: func(input: string) -> u32; }",
    )?;
    let source = directory.path().join("component.ts");
    fs::write(
        &source,
        "export function echo(input:string):string { return input+'!'; } export function length(input:string):number { return input.length; }",
    )?;
    for core in [false, true] {
        let output = directory
            .path()
            .join(if core { "task.core.wasm" } else { "task.wasm" });
        let result = Command::new(env!("CARGO_BIN_EXE_perry-wit"))
            .arg(&source)
            .arg("--wit")
            .arg(&wit)
            .args(["--world", "task", "-o"])
            .arg(&output)
            .output()?;
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let bytes = fs::read(output)?;
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)?;
        if core {
            assert!(wasmparser::Parser::is_core_wasm(&bytes));
            continue;
        }
        assert!(wasmparser::Parser::is_component(&bytes));
        let engine = Engine::default();
        let component = Component::new(&engine, bytes)?;
        let mut store = Store::new(
            &engine,
            StoreLimitsBuilder::new().memory_size(262144).build(),
        );
        store.limiter(|limits: &mut StoreLimits| limits);
        let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
        let echo = instance.get_typed_func::<(String,), (String,)>(&mut store, "echo")?;
        let length = instance.get_typed_func::<(String,), (u32,)>(&mut store, "length")?;
        for _ in 0..150 {
            assert_eq!(echo.call(&mut store, ("漢🙂".into(),))?, ("漢🙂!".into(),));
            assert_eq!(length.call(&mut store, ("漢🙂".into(),))?, (2,));
        }
    }
    Ok(())
}

#[test]
fn production_rejects_deferred_operations_before_creating_output() -> Result<()> {
    use std::{fs, process::Command};
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("bad.ts");
    let output = directory.path().join("bad.wasm");
    for (body, diagnostic) in [
        ("process.env.KEY = 'value';", "read-only"),
        ("Promise.all([]);", "D5"),
        ("Promise.race([]);", "D5"),
        ("new Date('2024-01-01');", "Date"),
    ] {
        fs::write(
            &source,
            format!(
                "export function runRun():{{ok:true}}|{{ok:false}} {{ {body} return {{ok:true}}; }}"
            ),
        )?;
        let result = Command::new(env!("CARGO_BIN_EXE_perry-wit"))
            .arg(&source)
            .arg("-o")
            .arg(&output)
            .output()?;
        let error = String::from_utf8(result.stderr)?;
        assert!(!result.status.success());
        assert!(error.contains(diagnostic), "{error}");
        assert!(!output.exists());
    }
    Ok(())
}

#[test]
fn optional_json_text_guards_handle_present_undefined_and_reassignment() -> Result<()> {
    use wasmtime::component::{Component, Linker};
    use wasmtime::{Engine, Store};
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("world.wit"),
        "package test:json-output; world task { export run: func(present: bool) -> string; }",
    )?;
    let compiled = compile_typescript(
        r#"
      export function run(present:boolean):string {
        let value = JSON.stringify(undefined);
        if (present) value = JSON.stringify({name:'漢🙂'});
        if (typeof value === 'undefined') return 'missing';
        if (typeof value !== 'string') throw 90;
        const retained = value;
        value = JSON.stringify(undefined);
        if (value !== undefined) throw 91;
        if (value === undefined) return retained;
        throw 92;
      }
    "#,
        "optional.ts",
        &CompileOptions {
            wit_dir: directory.path().into(),
            world: Some("task".into()),
            core_only: false,
        },
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(bool,), (String,)>(&mut store, "run")?;
    for (present, expected) in [(false, "missing"), (true, "{\"name\":\"漢🙂\"}")] {
        assert_eq!(run.call(&mut store, (present,))?.0, expected);
    }
    Ok(())
}

#[test]
fn suspending_exports_require_async_wit_including_transitive_calls() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let options = CompileOptions {
        wit_dir: directory.path().into(),
        world: Some("task".into()),
        core_only: false,
    };
    let source = r#"
      import {readFileSync} from 'fs';
      function read(path:string):string {return readFileSync(path,'utf8');}
      export function run(path:string):string {return read(path);}
      export function pure(input:string):string {return input+'!';}
    "#;
    for async_export in [false, true] {
        let qualifier = if async_export { "async " } else { "" };
        std::fs::write(
            directory.path().join("world.wit"),
            format!(
                "package test:suspension; world task {{ include wasi:cli/imports@0.3.0; export run: {qualifier}func(path:string)->string; export pure:func(input:string)->string; }}"
            ),
        )?;
        let result = compile_typescript(source, "suspension.ts", &options);
        if async_export {
            result?;
        } else {
            let error = format!(
                "{:#}",
                result.expect_err("blocking sync exports must be diagnosed")
            );
            assert!(error.contains("declare it as 'async func'"), "{error}");
        }
    }
    std::fs::write(
        directory.path().join("world.wit"),
        "package test:suspension; world task { include wasi:cli/imports@0.3.0; export pure:func(input:string)->string; }",
    )?;
    compile_typescript(
        &source.replace("export function run", "function run"),
        "unused.ts",
        &options,
    )?;
    Ok(())
}
