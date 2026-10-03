use anyhow::Result;
use perry_wit::{compile_typescript_waffle, waffle_backend::WaffleCompileOptions};

#[test]
fn unsupported_json_options_are_diagnosed_before_frontend_folding() {
    for (call, diagnostic) in [
        ("JSON.parse()", "argument count"),
        ("JSON.parse(input, null, 3)", "argument count"),
        ("JSON.stringify(input, null, null, 4)", "argument count"),
        ("JSON.stringify(...input)", "Spread JSON"),
        ("JSON.stringify(input, sideEffect())", "JSON revivers"),
        ("JSON.parse(input, (key, value) => value)", "JSON revivers"),
        ("JSON.stringify(input, null, 2)", "JSON revivers"),
        ("JSON[input](input)", "Dynamic JSON"),
        ("JSON.rawJSON(input)", "Unsupported JSON method"),
    ] {
        let source = format!(
            "function sideEffect(): number {{ return 1; }} export function run(input: string): string {{ return {call}; }}"
        );
        let error =
            compile_typescript_waffle(&source, "json_options.ts", &WaffleCompileOptions::default())
                .unwrap_err();
        assert!(error.to_string().contains(diagnostic), "{call}: {error:#}");
    }
}

#[test]
fn a_shadowed_json_identifier_keeps_ordinary_guest_behavior() -> Result<()> {
    let compiled = compile_typescript_waffle(
        "function JSON(value: number): number { return value + 1; } export function run(value: number): number { return JSON(value); }",
        "shadowed_json.ts",
        &WaffleCompileOptions::default(),
    )?;
    let engine = wasmtime::Engine::default();
    let module = wasmtime::Module::new(&engine, compiled.core)?;
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = wasmtime::Instance::new(&mut store, &module, &[])?;
    assert_eq!(
        instance
            .get_typed_func::<f64, f64>(&mut store, "run")?
            .call(&mut store, 4.0)?,
        5.0
    );
    Ok(())
}
