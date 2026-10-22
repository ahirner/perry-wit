//! Embedding consumes explicit bytes; host files and ambient WIT cannot supply dependencies.
use anyhow::Result;
use perry_wit::api::{self, Module, Options, Package, Program, Wit};

fn program(source: &str) -> Program {
    Program {
        source: source.as_bytes().into(),
        modules: vec![],
    }
}

fn wit(source: &str) -> Wit {
    Wit {
        application: source.as_bytes().into(),
        dependencies: vec![],
    }
}

const WORLD: &str = "package test:bytes; world example { export run: func(n: f64) -> f64; }";

#[test]
fn raw_core_and_wit_core_have_distinct_boundaries() -> Result<()> {
    let input = program(
        "export function run(n: number): number { return n; } export function extra(): number { return 7; }",
    );
    let raw = api::compile_core(&input)?;
    let full = api::compile(&input, &wit(WORLD), &Options::default())?;
    let core_only = api::compile(
        &input,
        &wit(WORLD),
        &Options {
            core_only: true,
            ..Default::default()
        },
    )?;
    assert_eq!(core_only.core, full.core);
    assert!(core_only.component.is_none());
    assert!(!core_only.allocation_report.is_empty());
    let component = full.component.unwrap();
    for bytes in [&component.bytes, &component.stripped] {
        wasmparser::Validator::new().validate_all(bytes)?;
    }
    let exports = |bytes: &[u8]| -> Result<Vec<String>> {
        let mut names = Vec::new();
        for payload in wasmparser::Parser::new(0).parse_all(bytes) {
            if let wasmparser::Payload::ExportSection(section) = payload? {
                for export in section {
                    names.push(export?.name.to_owned());
                }
            }
        }
        Ok(names)
    };
    assert!(exports(&raw)?.contains(&"extra".into()));
    assert!(!exports(&core_only.core)?.contains(&"extra".into()));
    assert!(
        api::compile(
            &input,
            &wit("package test:missing; world app { export missing: func(); }"),
            &Options {
                core_only: true,
                ..Default::default()
            }
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn dependency_modules_require_unique_relative_identities_and_utf8() -> Result<()> {
    let mut input = program("export function run(n: number): number { return n; }");
    for name in ["", "../outside.ts", "/absolute.ts", "input.ts"] {
        input.modules = vec![Module {
            name: name.into(),
            contents: vec![],
        }];
        assert!(api::compile_core(&input).is_err(), "accepted {name:?}");
    }
    input.modules = vec![Module {
        name: "other.ts".into(),
        contents: vec![255],
    }];
    assert!(
        format!("{:#}", api::compile_core(&input).unwrap_err()).contains("other.ts is not UTF-8")
    );
    input.modules = vec![
        Module {
            name: "other.ts".into(),
            contents: vec![]
        };
        2
    ];
    assert!(
        format!("{:#}", api::compile_core(&input).unwrap_err())
            .contains("duplicate module identity")
    );
    Ok(())
}

#[test]
fn anonymous_content_matches_the_file_adapter() -> Result<()> {
    let source = "export function run(n: number): number { return n * 2; }";
    let input = program(source);
    let compiled = api::compile(&input, &wit(WORLD), &Options::default())?;
    let core = api::compile_core(&input)?;
    assert_eq!(core, perry_wit::compile_core(source, "example.ts")?);
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("world.wit"), WORLD)?;
    let options = perry_wit::CompileOptions {
        wit_dir: directory.path().into(),
        ..Default::default()
    };
    let from_files = perry_wit::compile_typescript(source, "example.ts", &options)?;
    assert_eq!(compiled.component, from_files.component);
    let artifacts = api::generate_sdk(&wit(WORLD), None)?;
    assert_eq!(
        artifacts
            .iter()
            .map(|artifact| artifact.name.as_str())
            .collect::<Vec<_>>(),
        ["world.d.ts", "imports.d.ts", "runtime.d.ts"]
    );
    assert!(String::from_utf8(artifacts[0].contents.clone())?.contains("run"));
    Ok(())
}

#[test]
fn explicit_modules_and_packages_resolve_without_paths() -> Result<()> {
    let mut input = program(
        "import { double } from './math.js'; export function run(n: number): number { return double(n); }",
    );
    input.modules.push(Module {
        name: "math.ts".into(),
        contents: b"export function double(n: number): number { return n * 2; }".to_vec(),
    });
    let mut contract = wit(
        "package test:bytes; world example { use test:types/numbers.{number}; export run: func(n: number) -> number; }",
    );
    contract.dependencies.push(Package::Sources(vec![
        b"package test:types; interface numbers { type number = f64; }"
            .as_slice()
            .into(),
        b"package test:types; interface unused {}".as_slice().into(),
    ]));
    let compiled = api::compile(&input, &contract, &Options::default())?;
    let engine = wasmtime::Engine::default();
    let component =
        wasmtime::component::Component::new(&engine, compiled.component.unwrap().stripped)?;
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = wasmtime::component::Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
    assert_eq!(run.call(&mut store, (21.0,))?.0, 42.0);
    let missing = Wit {
        application: contract.application.clone(),
        dependencies: vec![],
    };
    assert!(
        format!(
            "{:#}",
            api::compile(&input, &missing, &Options::default()).unwrap_err()
        )
        .contains("test:types")
    );
    if let Package::Sources(sources) = &mut contract.application {
        sources.push(
            b"package test:bytes; interface additional {}"
                .as_slice()
                .into(),
        );
    }
    api::compile(&input, &contract, &Options::default())?;
    input.modules.clear();
    assert!(
        format!(
            "{:#}",
            api::compile(&input, &contract, &Options::default()).unwrap_err()
        )
        .contains("Cannot resolve")
    );
    Ok(())
}

#[test]
fn encoded_wit_and_invalid_bytes_are_reported() -> Result<()> {
    let (resolve, package) = perry_wit::component::wit::resolve_source("input.wit", WORLD)?;
    let encoded = wit_component::encode(&resolve, package, false)?;
    let contract = Wit {
        application: Package::Encoded(encoded),
        dependencies: vec![],
    };
    let input = program("export function run(n: number): number { return n; }");
    api::compile(&input, &contract, &Options::default())?;
    for (input, contract, expected, code) in [
        (
            program("export function {"),
            wit(WORLD),
            "Parsing",
            perry_wit::DiagnosticCode::ParseError,
        ),
        (
            Program {
                source: [255].as_slice().into(),
                modules: vec![],
            },
            wit(WORLD),
            "UTF-8",
            perry_wit::DiagnosticCode::InvalidInput,
        ),
        (
            input.clone(),
            Wit {
                application: [255].as_slice().into(),
                dependencies: vec![],
            },
            "UTF-8",
            perry_wit::DiagnosticCode::WitResolution,
        ),
        (
            input,
            wit("invalid WIT"),
            "expected",
            perry_wit::DiagnosticCode::WitResolution,
        ),
    ] {
        let error = api::compile(&input, &contract, &Options::default()).unwrap_err();
        assert_eq!(error.diagnostics[0].code, code, "{error:#}");
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
    Ok(())
}

#[test]
fn content_api_resolves_only_supplied_files_and_packages() -> Result<()> {
    if std::env::var_os("PERRY_CONTENT_PROBE").is_some() {
        let input = program("export function run(n: number): number { return n; }");
        api::compile(&input, &wit(WORLD), &Options::default())?;
        api::generate_sdk(
            &wit("package test:http; world example { import wasi:http/client@0.3.0; }"),
            None,
        )?;
        assert!(api::compile_core(&program("export { run } from './hidden';")).is_err());
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("hidden.ts"),
        "export function run(): number { return 42; }",
    )?;
    std::fs::write(
        directory.path().join("package.json"),
        r#"{"type":"commonjs"}"#,
    )?;
    let result = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "content_api_test::content_api_resolves_only_supplied_files_and_packages",
        ])
        .env("PERRY_CONTENT_PROBE", "1")
        .env("WASI_WIT_PATH", "/unavailable-wasi")
        .current_dir(directory.path())
        .output()?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(())
}
