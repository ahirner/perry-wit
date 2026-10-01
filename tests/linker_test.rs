use perry_wit::component::embed_and_encode;
use perry_wit::linker::merge_core_modules;
use perry_wit::strip;
use std::fs;
use std::path::Path;
use wasmparser::{Parser, Payload, Validator, WasmFeatures};

#[test]
fn test_merge_core_modules() {
    let ts_wasm_path = Path::new("dist/merge_docs.core.wasm");
    let rt_wasm_path = Path::new("target/wasm32-unknown-unknown/release/guest_runtime.wasm");

    if !ts_wasm_path.exists() || !rt_wasm_path.exists() {
        eprintln!("Skipping test: test files not present");
        return;
    }

    let ts_wasm = fs::read(ts_wasm_path).expect("read ts wasm");
    let rt_wasm = fs::read(rt_wasm_path).expect("read rt wasm");

    let merged = merge_core_modules(&ts_wasm, &rt_wasm).expect("merge core modules");

    // 1. Validate merged module
    let mut validator = Validator::new_with_features(WasmFeatures::all());
    validator.validate_all(&merged).expect("validate merged wasm");

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
    assert!(export_names.contains(&"_start".to_string()), "missing _start");
    assert!(export_names.contains(&"memory".to_string()), "missing memory");
    assert!(
        export_names.contains(&"wasi:cli/run@0.2.6#run".to_string()),
        "missing run export"
    );
    assert!(
        export_names.contains(&"cabi_realloc".to_string()),
        "missing cabi_realloc"
    );

    // 3. Test componentization and stripping
    let component_bytes =
        embed_and_encode(&merged, Path::new("wit"), Some("merge-docs")).expect("embed and encode component");
    assert!(!component_bytes.is_empty());

    let stripped_bytes = strip::component(&component_bytes).expect("strip component");
    assert!(!stripped_bytes.is_empty());
    assert!(stripped_bytes.len() < component_bytes.len());

    // Write to dist/perry_merge_docs.stripped.wasm to test with wasmtime
    fs::create_dir_all("dist").expect("create dist");
    fs::write("dist/perry_merge_docs.stripped.wasm", &stripped_bytes).expect("write stripped component");

    // Validate component
    let mut comp_validator = Validator::new_with_features(WasmFeatures::all());
    comp_validator
        .validate_all(&stripped_bytes)
        .expect("validate stripped component");
}
