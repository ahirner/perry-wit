use perry_wit::component::embed_and_encode;
use perry_wit::linker::merge_core_modules;
use perry_wit::strip;
use std::fs;
use std::path::Path;
use std::process::Command;
use wasmparser::{Parser, Payload, Validator, WasmFeatures};

#[expect(
    dead_code,
    reason = "These core-Wasm fixtures only need Scratch storage."
)]
mod support;

#[test]
fn global_function_references_keep_runtime_functions_and_their_imports() {
    let fixtures = [
        (
            r#"(module (memory (export "memory") 1))"#,
            r#"(module
                (import "wasi:test" "value" (func $value (result i32)))
                (func $dead (result i32) i32.const 0)
                (func $callback (result i32) call $value)
                (global (export "callback") funcref (ref.func $callback)))"#,
        ),
        (
            r#"(module
                (import "rt" "callback" (func $callback (result i32)))
                (memory (export "memory") 1)
                (global (export "callback") funcref (ref.func $callback)))"#,
            r#"(module
                (import "wasi:test" "value" (func $value (result i32)))
                (func $dead (result i32) i32.const 0)
                (func (export "callback") (result i32) call $value))"#,
        ),
        (
            r#"(module (memory (export "memory") 1))"#,
            r#"(module
                (import "wasi:test" "value" (func $value (result i32)))
                (global (export "callback") funcref (ref.func $value)))"#,
        ),
    ];
    for (application, runtime) in fixtures {
        let application = wat::parse_str(application).unwrap();
        let runtime = wat::parse_str(runtime).unwrap();
        for bytes in [&application, &runtime] {
            Validator::new().validate_all(bytes).unwrap();
        }
        let merged = merge_core_modules(&application, &runtime).unwrap();
        Validator::new().validate_all(&merged).unwrap();
        let scratch = support::Scratch::new();
        let path = scratch.0.join("globals.wasm");
        fs::write(&path, merged).unwrap();
        let output = Command::new("node")
            .arg("--eval")
            .arg(
                r#"
                const assert = require('node:assert/strict');
                const bytes = require('node:fs').readFileSync(process.argv[1]);
                const module = new WebAssembly.Module(bytes);
                assert.deepEqual(WebAssembly.Module.imports(module), [
                    {module: 'wasi:test', name: 'value', kind: 'function'}
                ]);
                const instance = new WebAssembly.Instance(module, {'wasi:test': {value: () => 42}});
                assert.equal(instance.exports.callback.value(), 42);
            "#,
            )
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn table_indirect_calls_preserve_runtime_functions_and_imports() {
    let fixtures = [
        // Table in Module A with indirect call to runtime function in Module B
        (
            r#"(module
                (type $sig (func (result i32)))
                (import "rt" "target" (func $target (result i32)))
                (table (export "table") 1 funcref)
                (elem (i32.const 0) $target)
                (memory (export "memory") 1)
                (func (export "dispatch") (result i32)
                    i32.const 0
                    call_indirect (type $sig)))"#,
            r#"(module
                (import "wasi:test" "value" (func $value (result i32)))
                (func $dead (result i32) i32.const 0)
                (func (export "target") (result i32) call $value))"#,
        ),
        // Table in Module B with indirect call to imported host function
        (
            r#"(module
                (import "rt" "dispatch" (func $dispatch (result i32)))
                (table (export "table") 1 funcref)
                (memory (export "memory") 1)
                (func (export "run") (result i32) call $dispatch))"#,
            r#"(module
                (type $sig (func (result i32)))
                (import "wasi:test" "value" (func $value (result i32)))
                (func $dead (result i32) i32.const 0)
                (table (export "table") 1 funcref)
                (elem (i32.const 0) $value)
                (func (export "dispatch") (result i32)
                    i32.const 0
                    call_indirect (type $sig)))"#,
        ),
    ];
    for (application, runtime) in fixtures {
        let application = wat::parse_str(application).unwrap();
        let runtime = wat::parse_str(runtime).unwrap();
        let merged = merge_core_modules(&application, &runtime).unwrap();
        Validator::new().validate_all(&merged).unwrap();
        let scratch = support::Scratch::new();
        let path = scratch.0.join("indirect.wasm");
        fs::write(&path, merged).unwrap();
        let output = Command::new("node")
            .arg("--eval")
            .arg(
                r#"
                const assert = require('node:assert/strict');
                const bytes = require('node:fs').readFileSync(process.argv[1]);
                const module = new WebAssembly.Module(bytes);
                assert.deepEqual(WebAssembly.Module.imports(module), [
                    {module: 'wasi:test', name: 'value', kind: 'function'}
                ]);
                const instance = new WebAssembly.Instance(module, {'wasi:test': {value: () => 99}});
                const fnName = instance.exports.dispatch ? 'dispatch' : 'run';
                assert.equal(instance.exports[fnName](), 99);
            "#,
            )
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn test_merge_core_modules() {
    let ts_source = r#"
        console.log("merging core modules test");
    "#;
    let (ts_wasm, exported_functions, functions) =
        perry_wit::compiler::compile_typescript_raw(ts_source, "merge_docs.ts")
            .expect("compile raw ts");
    let rt_wasm =
        perry_wit::runtime::resolve_guest_runtime_bytes(None).expect("resolve runtime");

    let merged = merge_core_modules(&ts_wasm, &rt_wasm).expect("merge core modules");

    // 1. Validate merged module
    let mut validator = Validator::new_with_features(WasmFeatures::all());
    validator
        .validate_all(&merged)
        .expect("validate merged wasm");

    // 2. Check imports
    let mut import_modules = Vec::new();
    let mut export_names = Vec::new();

    for payload in Parser::new(0).parse_all(&merged) {
        match payload.expect("payload") {
            Payload::ImportSection(reader) => {
                for imp in reader.into_imports() {
                    let imp = imp.expect("import");
                    import_modules.push(imp.module.to_string());
                }
            }
            Payload::ExportSection(reader) => {
                for exp in reader {
                    let exp = exp.expect("export");
                    export_names.push(exp.name.to_string());
                }
            }
            _ => (),
        }
    }

    // Ensure all imports are WASI imports, no rt imports
    assert!(!import_modules.is_empty(), "must have WASI imports");
    for m in &import_modules {
        assert!(m.starts_with("wasi:"), "expected wasi import, found: {m}");
    }

    // Ensure required exports are present
    assert!(
        export_names.contains(&"_start".to_string()),
        "missing _start"
    );
    assert!(
        export_names.contains(&"memory".to_string()),
        "missing memory"
    );
    assert!(
        export_names.contains(&"cabi_realloc".to_string()),
        "missing cabi_realloc"
    );

    // 3. Test componentization and stripping after synthesizing trampolines
    let wit_exports =
        perry_wit::abi::extract_world_exports(Path::new("wit"), Some("merge-docs"))
            .expect("extract world exports");
    let ready_core = perry_wit::abi::synthesize_trampolines(
        &merged,
        &wit_exports,
        &exported_functions,
        &functions,
    )
    .expect("synthesize trampolines");

    let component_bytes = embed_and_encode(&ready_core, Path::new("wit"), Some("merge-docs"))
        .expect("embed and encode component");
    assert!(!component_bytes.is_empty());

    let stripped_bytes = strip::component(&component_bytes).expect("strip component");
    assert!(!stripped_bytes.is_empty());
    assert!(stripped_bytes.len() < component_bytes.len());

    // Validate component
    let mut comp_validator = Validator::new_with_features(WasmFeatures::all());
    comp_validator
        .validate_all(&stripped_bytes)
        .expect("validate stripped component");
}
