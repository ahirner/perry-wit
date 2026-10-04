use anyhow::Result;
use perry_wit::{CompileOptions, compile_typescript};

#[test]
fn production_command_runs_with_the_nix_p3_host() -> Result<()> {
    let compiled = compile_typescript(
        r#"
      import {setTimeout} from 'node:timers/promises';
      const started = 'native P3: ';
      await setTimeout(1);
      console.log(started + '漢🙂');
    "#,
        "command.ts",
        &CompileOptions {
            world: Some("command".into()),
            ..Default::default()
        },
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
        ("Promise.resolve(1);", "D5"),
        ("new Promise(() => {});", "Promise"),
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
            .args(["--world", "command"])
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
fn static_modules_preserve_aliases_namespaces_and_multiple_exports() -> Result<()> {
    use std::{fs, process::Command};
    use wasmtime::component::{Component, Linker};
    use wasmtime::{Engine, Store};
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("world.wit"),
        "package test:modules; world task { export echo:func(input:string)->string; export length:func(input:string)->u32; }",
    )?;
    fs::write(
        directory.path().join("text.ts"),
        "let initialized:number=0; initialized++; export function decorate(input:string):string{return input+'!';} export function count(input:string):number{return input.length+initialized-1;}",
    )?;
    fs::write(
        directory.path().join("barrel.ts"),
        "export {decorate as decorated} from './text.ts';",
    )?;
    let source = "import {decorated as format} from './barrel.ts'; import * as text from './text.ts'; function echoImpl(input:string):string{return format(input);} export {echoImpl as echo}; export function length(input:string):number{return text.count(input);}";
    let entry = directory.path().join("entry.ts");
    fs::write(&entry, source)?;
    let compiled = compile_typescript(
        source,
        entry.to_str().unwrap(),
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
    let echo = instance.get_typed_func::<(String,), (String,)>(&mut store, "echo")?;
    let length = instance.get_typed_func::<(String,), (u32,)>(&mut store, "length")?;
    assert_eq!(echo.call(&mut store, ("hello".into(),))?.0, "hello!");
    assert_eq!(length.call(&mut store, ("hello".into(),))?.0, 5);
    fs::write(
        directory.path().join("check.ts"),
        "import {echo,length} from './entry.ts'; if(echo('hello')!=='hello!'||length('hello')!==5)throw new Error('mismatch');",
    )?;
    let output = Command::new("node")
        .arg(directory.path().join("check.ts"))
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_reexports_use_disposable_sdk_types() -> Result<()> {
    use perry_wit::sdk::{SdkOptions, generate_sdk_files};
    use std::{fs, process::Command};
    use wasmtime::component::{Component, Linker};
    use wasmtime::{Config, Engine, Store};
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    fs::create_dir(root.join("wit"))?;
    fs::write(
        root.join("wit/world.wit"),
        "package test:async-modules; world task { record item { text:string } export run:async func(input:item)->string; }",
    )?;
    fs::write(
        root.join("work.ts"),
        "import type {Item} from './.perry/types/world'; export async function work(input:Item):Promise<string>{await 0;return input.text+'!';}",
    )?;
    let source = "export {work as run} from './work.ts';";
    let entry = root.join("entry.ts");
    fs::write(&entry, source)?;
    generate_sdk_files(&SdkOptions {
        wit_dir: root.join("wit"),
        out_dir: root.join(".perry/types"),
        project_root: Some(root.into()),
        entry: "entry.ts".into(),
        initialize_tsconfig: false,
        ..Default::default()
    })?;
    let check = Command::new("tsc")
        .current_dir(root)
        .args(["-p", ".perry/types"])
        .output()?;
    assert!(
        check.status.success(),
        "{}{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
    let compiled = compile_typescript(
        source,
        entry.to_str().unwrap(),
        &CompileOptions {
            wit_dir: root.join("wit"),
            world: Some("task".into()),
            core_only: false,
        },
    )?;
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_threading(true)
        .wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    #[derive(wasmtime::component::ComponentType, wasmtime::component::Lower)]
    #[component(record)]
    struct Item {
        text: String,
    }
    let run = instance.get_typed_func::<(Item,), (String,)>(&mut store, "run")?;
    assert_eq!(
        run.call_async(
            &mut store,
            (Item {
                text: "hello".into()
            },)
        )
        .await?
        .0,
        "hello!"
    );
    fs::write(
        root.join("compare.ts"),
        "import {run} from './entry.ts'; console.log(await run({text:'hello'}));",
    )?;
    let node = Command::new("node").arg(root.join("compare.ts")).output()?;
    assert!(
        node.status.success(),
        "{}",
        String::from_utf8_lossy(&node.stderr)
    );
    assert_eq!(String::from_utf8(node.stdout)?.trim(), "hello!");
    assert!(!root.join("tsconfig.json").exists());
    Ok(())
}

#[test]
fn unsupported_export_forms_receive_direct_diagnostics() -> Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::write(
        root.path().join("world.wit"),
        "package test:exports; world task {export run:func()->f64;}",
    )?;
    for source in [
        "export default function run():number{return 1;}",
        "export const run=():number=>1;",
        "export function* run(){yield 1;}",
        "export function run<T>():number{return 1;}",
        "export function run(value:number=1):number{return value;}",
    ] {
        let error = compile_typescript(
            source,
            "exports.ts",
            &CompileOptions {
                wit_dir: root.path().into(),
                world: Some("task".into()),
                core_only: false,
            },
        )
        .map(|_| ())
        .expect_err("unsupported export form");
        assert!(
            format!("{error:#}").contains("Component export"),
            "{error:#}"
        );
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

#[test]
fn process_exit_uses_standard_wasi_and_matches_node_status() -> Result<()> {
    for argument in [
        "",
        "undefined",
        "0",
        "5",
        "-1",
        "260",
        "-257",
        "9007199254740991",
    ] {
        let source = format!(
            "console.log('before'); try {{ process.exit({argument}); }} finally {{ console.log('finally'); }} console.log('after');"
        );
        let compiled = compile_typescript(
            &source,
            "exit.ts",
            &CompileOptions {
                world: Some("command".into()),
                ..Default::default()
            },
        )?;
        let core = waffle::Module::from_wasm_bytes(&compiled.core, &Default::default())?;
        assert!(core.imports.iter().any(
            |import| import.module == "wasi:cli/exit@0.3.0" && import.name == "exit-with-code"
        ));
        compare_cli_with_node(&source, b"before\n")?;
    }
    Ok(())
}

#[test]
fn named_exports_use_standard_wasi_exit_and_validate_its_arguments() -> Result<()> {
    use wasmtime::component::{Component, Linker};
    use wasmtime::{Engine, Store};
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("world.wit"),
        "package test:exit; world task { import wasi:cli/exit@0.3.0; export terminate:func(code:f64)->f64; }",
    )?;
    let options = CompileOptions {
        wit_dir: directory.path().into(),
        world: Some("task".into()),
        ..Default::default()
    };
    let compiled = compile_typescript(
        "export function terminate(code:number):number {process.exit(code);return 123;}",
        "terminate.ts",
        &options,
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    linker.instance("wasi:cli/exit@0.3.0")?.func_wrap(
        "exit-with-code",
        |mut store: wasmtime::StoreContextMut<'_, usize>, (code,): (u8,)| -> wasmtime::Result<()> {
            *store.data_mut() += 1;
            if code == 42 {
                Ok(())
            } else {
                Err(wasmtime_wasi::I32Exit(i32::from(code)).into())
            }
        },
    )?;
    for (code, expected, host_calls) in [
        (7.0, Some(7), 1),
        (-1.0, Some(255), 1),
        (42.0, None, 1),
        (0.5, None, 0),
        (f64::NAN, None, 0),
        (f64::INFINITY, None, 0),
        (1e100, None, 0),
    ] {
        let mut store = Store::new(&engine, 0usize);
        let instance = linker.instantiate(&mut store, &component)?;
        let terminate = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "terminate")?;
        let error = terminate.call(&mut store, (code,)).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<wasmtime_wasi::I32Exit>()
                .map(|exit| exit.0),
            expected
        );
        assert_eq!(*store.data(), host_calls);
    }
    for expression in ["'7'", "true", "{}", "[]", "1,2"] {
        let error = compile_typescript(&format!("export function terminate(code:number):number {{process.exit({expression});return 123;}}"), "invalid-exit.ts", &options).unwrap_err();
        let diagnostic = format!("{error:#}").to_lowercase();
        assert!(
            diagnostic.contains("argument")
                || diagnostic.contains("parameter")
                || diagnostic.contains("declared static type"),
            "{diagnostic}"
        );
    }
    Ok(())
}

fn compare_cli_with_node(source: &str, stdout: &[u8]) -> Result<()> {
    let compiled = compile_typescript(
        source,
        "command.ts",
        &CompileOptions {
            world: Some("command".into()),
            ..Default::default()
        },
    )?;
    let directory = tempfile::tempdir()?;
    let component = directory.path().join("command.wasm");
    let script = directory.path().join("command.ts");
    std::fs::write(&component, compiled.stripped.unwrap())?;
    std::fs::write(&script, source)?;
    let node = std::process::Command::new("node").arg(script).output()?;
    let wasm = std::process::Command::new("wasmtime")
        .args([
            "run",
            "-C",
            "cache=n",
            "-S",
            "p3=y",
            "-W",
            "component-model-async=y",
            "-W",
            "component-model-async-stackful=y",
            "-W",
            "component-model-more-async-builtins=y",
        ])
        .arg(component)
        .output()?;
    assert_eq!(node.stdout, stdout, "Node: {source}");
    assert_eq!(
        wasm.stdout,
        node.stdout,
        "{source}: {}",
        String::from_utf8_lossy(&wasm.stderr)
    );
    assert_eq!(
        wasm.status.code(),
        node.status.code(),
        "{source}: {}",
        String::from_utf8_lossy(&wasm.stderr)
    );
    Ok(())
}

#[test]
fn process_exit_code_preserves_execution_and_matches_node_command_status() -> Result<()> {
    for source in [
        "if (process.exitCode === undefined) { console.log('true'); } else { console.log('false'); } process.exitCode = 7; if (process.exitCode === 7) { console.log('true'); } else { console.log('false'); } console.log('after');",
        "if (process.exitCode === undefined) { console.log('true'); } else { console.log('false'); } process['exitCode'] = 7; if (process.exitCode === 7) { console.log('true'); } else { console.log('false'); } process.exitCode = undefined; console.log('after');",
        "if (process.exitCode === undefined) { console.log('true'); } else { console.log('false'); } if ((process.exitCode = 260) === 260) { console.log('true'); } else { console.log('false'); } console.log('after');",
        "if (process.exitCode === undefined) { console.log('true'); } else { console.log('false'); } if ((process.exitCode = -1) === -1) { console.log('true'); } else { console.log('false'); } console.log('after');",
        "import {setTimeout} from 'node:timers/promises'; if (process.exitCode === undefined) { console.log('true'); } else { console.log('false'); } process.exitCode = 9; await setTimeout(1); if (process.exitCode === 9) { console.log('true'); } else { console.log('false'); } console.log('after');",
    ] {
        compare_cli_with_node(source, b"true\ntrue\nafter\n")?;
    }
    for argument in ["", "undefined", "0", "3"] {
        compare_cli_with_node(
            &format!(
                "process.exitCode = 7; console.log('before'); process.exit({argument}); console.log('after');"
            ),
            b"before\n",
        )?;
    }
    compare_cli_with_node(
        "process.exitCode = 7; process.exitCode = undefined; if (process.exitCode === undefined) { console.log('true'); } else { console.log('false'); }",
        b"true\n",
    )?;
    Ok(())
}

#[test]
fn named_exports_retain_exit_code_without_terminating_the_host() -> Result<()> {
    use wasmtime::component::{Component, Linker};
    use wasmtime::{Engine, Store};
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("world.wit"),
        "package test:status; world task { export set:func(code:f64)->f64; export clear:func(); export matches:func(code:f64)->bool; export unset:func()->bool; }",
    )?;
    let options = CompileOptions {
        wit_dir: directory.path().into(),
        world: Some("task".into()),
        ..Default::default()
    };
    let source = "export function set(code:number):number { return process.exitCode = code; } export function clear():void { process.exitCode = undefined; } export function matches(code:number):boolean { return process.exitCode === code; } export function unset():boolean { return process.exitCode === undefined; }";
    let compiled = compile_typescript(source, "status.ts", &options)?;
    let core = waffle::Module::from_wasm_bytes(&compiled.core, &Default::default())?;
    assert!(
        core.imports.is_empty(),
        "exitCode storage alone requires no host interface"
    );
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let set = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "set")?;
    let clear = instance.get_typed_func::<(), ()>(&mut store, "clear")?;
    let matches = instance.get_typed_func::<(f64,), (bool,)>(&mut store, "matches")?;
    let unset = instance.get_typed_func::<(), (bool,)>(&mut store, "unset")?;
    for i in 0..100 {
        assert!(unset.call(&mut store, ())?.0);
        assert_eq!(set.call(&mut store, (i as f64,))?.0, i as f64);
        assert!(!unset.call(&mut store, ())?.0);
        assert!(matches.call(&mut store, (i as f64,))?.0);
        clear.call(&mut store, ())?;
    }
    for source in [
        "process.exitCode = '2';",
        "process.exitCode = {};",
        "process.exitCode = true;",
        "process.exitCode += 1;",
        "process.env.VALUE = '2';",
        "process.argv[0] = '2';",
    ] {
        assert!(
            compile_typescript(
                source,
                "invalid-status.ts",
                &CompileOptions {
                    world: Some("command".into()),
                    ..Default::default()
                }
            )
            .is_err(),
            "{source}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn uncaught_cli_failures_return_failed_wit_results_and_preserve_finally() -> Result<()> {
    use wasmtime::component::{Component, Linker};
    use wasmtime::{Config, Engine, Store};
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("world.wit"),
        "package test:command-failure; interface host { trace:func(value:u32); wait:async func(); } world task { import host; export wasi:cli/run@0.3.0; }",
    )?;
    let options = CompileOptions {
        wit_dir: directory.path().into(),
        world: Some("task".into()),
        ..Default::default()
    };
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_async_stackful(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_threading(true);
    let engine = Engine::new(&config)?;
    for (source, trace, once) in [
        ("trace(1); throw 9;", 1, true),
        (
            "export function runRun():{ok:true}|{ok:false} { trace(2); throw 9; }",
            2,
            false,
        ),
        (
            "export async function runRun():Promise<{ok:true}|{ok:false}> { try { await wait(); throw 9; } finally { trace(3); } }",
            3,
            false,
        ),
    ] {
        let source = format!("import {{trace,wait}} from 'test:command-failure/host'; {source}");
        let compiled = compile_typescript(&source, "failure.ts", &options)?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut linker = Linker::new(&engine);
        let mut host = linker.instance("test:command-failure/host")?;
        host.func_wrap(
            "trace",
            |mut store: wasmtime::StoreContextMut<'_, Vec<u32>>, (value,): (u32,)| {
                store.data_mut().push(value);
                Ok(())
            },
        )?;
        host.func_wrap_concurrent("wait", |_, (): ()| {
            Box::pin(async {
                tokio::task::yield_now().await;
                Ok(())
            })
        })?;
        let mut store = Store::new(&engine, Vec::new());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let interface = instance
            .get_export_index(&mut store, None, "wasi:cli/run@0.3.0")
            .unwrap();
        let index = instance
            .get_export_index(&mut store, Some(&interface), "run")
            .unwrap();
        let run =
            instance.get_typed_func::<(), (std::result::Result<(), ()>,)>(&mut store, index)?;
        for count in 1..=100 {
            assert_eq!(run.call_async(&mut store, ()).await?.0, Err(()), "{source}");
            assert_eq!(store.data(), &vec![trace; if once { 1 } else { count }]);
            store.assert_concurrent_state_empty();
        }
    }
    for source in [
        "console.log('before'); throw 9;",
        "process.exitCode = 7; console.log('before'); throw 9;",
        "import {setTimeout} from 'node:timers/promises'; console.log('before'); await setTimeout(1); throw 9;",
    ] {
        compare_cli_with_node(source, b"before\n")?;
    }
    Ok(())
}
