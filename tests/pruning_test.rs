//! Verification for Item C: Safe Runtime Pruning (C.1 Separable Capability Dispatch & C.2 Pruned Components).

mod support;

#[test]
fn test_pure_typescript_prunes_http_and_clocks() {
    let ts_source = r#"
        const a = 10;
        const b = 20;
        console.log("sum=" + (a + b));
    "#;
    let output = support::run(ts_source, None, None);
    assert_eq!(support::stdout(&output).trim(), "sum=30");

    let scratch = support::Scratch::new();
    let compiled = scratch.compile_artifacts(ts_source, None);
    let component_wasm = compiled.component.expect("component artifact");
    let wat = wasmprinter::print_bytes(&component_wasm).expect("print component wat");

    // Pure component must NOT import wasi:http or wasi:clocks
    assert!(!wat.contains("wasi:http"), "pure component should not contain wasi:http imports");
    assert!(!wat.contains("wasi:clocks"), "pure component should not contain wasi:clocks imports");
}

#[test]
fn test_clocks_only_component_prunes_http() {
    let ts_source = r#"
        const now = Date.now();
        const p = performance.now();
        console.log("clocks_ok");
    "#;
    let output = support::run(ts_source, None, None);
    assert_eq!(support::stdout(&output).trim(), "clocks_ok");

    let scratch = support::Scratch::new();
    let compiled = scratch.compile_artifacts(ts_source, None);
    let component_wasm = compiled.component.expect("component artifact");
    let wat = wasmprinter::print_bytes(&component_wasm).expect("print component wat");

    // Clocks component must import wasi:clocks, but must NOT import wasi:http
    assert!(wat.contains("wasi:clocks"), "clocks component should import wasi:clocks");
    assert!(!wat.contains("wasi:http"), "clocks component should not contain wasi:http imports");
}

#[test]
fn test_http_component_retains_http_imports() {
    let ts_source = r#"
        const res = fetch("https://example.com");
        console.log(res.status);
    "#;
    let scratch = support::Scratch::new();
    let compiled = scratch.compile_artifacts(ts_source, None);
    let component_wasm = compiled.component.expect("component artifact");
    let wat = wasmprinter::print_bytes(&component_wasm).expect("print component wat");

    assert!(wat.contains("wasi:http"), "http component should retain wasi:http imports");
}
