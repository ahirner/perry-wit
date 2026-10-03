use anyhow::Result;
use perry_hir::lower_module;
use perry_parser::parse_typescript;

#[test]
fn probe_string_literals_and_escapes() -> Result<()> {
    let cases = [
        ("ascii", r#"let s = "hello";"#),
        ("bmp", r#"let s = "café \u00e9";"#),
        ("non_bmp_direct", r#"let s = "😀";"#),
        ("non_bmp_braced_escape", r#"let s = "\u{1F600}";"#),
        ("paired_surrogates", r#"let s = "\uD83D\uDE00";"#),
        ("unpaired_high_surrogate", r#"let s = "\uD83D";"#),
        ("unpaired_low_surrogate", r#"let s = "\uDE00";"#),
        ("reversed_surrogates", r#"let s = "\uDE00\uD83D";"#),
        ("double_high_surrogate", r#"let s = "\uD83D\uD83D";"#),
        ("braced_lone_surrogate", r#"let s = "\u{D800}";"#),
        (
            "lone_surrogate_with_ascii",
            r#"let s = "prefix\uD83Dsuffix";"#,
        ),
        ("template_lone_surrogate", r#"let s = `template\uD83D`;"#),
        (
            "property_key_lone_surrogate",
            r#"let obj = { "\uD83D": 123 };"#,
        ),
        ("combining_sequence", r#"let s = "e\u0301";"#),
        ("empty_string", r#"let s = "";"#),
        ("embedded_nul", r#"let s = "a\0b";"#),
    ];

    for (name, ts) in cases {
        let parsed = parse_typescript(ts, "probe.ts");
        match parsed {
            Ok(ast) => {
                let lowered = lower_module(&ast, "main", "probe.ts");
                match lowered {
                    Ok(hir) => {
                        println!("=== CASE: {name} ===");
                        for stmt in &hir.init {
                            println!("  init stmt: {stmt:?}");
                        }
                    }
                    Err(e) => {
                        println!("=== CASE: {name} === LOWER ERROR: {e:?}");
                    }
                }
            }
            Err(e) => {
                println!("=== CASE: {name} === PARSE ERROR: {e:?}");
            }
        }
    }

    Ok(())
}

#[test]
fn probe_string_operations_hir() -> Result<()> {
    let cases = [
        (
            "length",
            r#"export function run(s: string): number { return s.length; }"#,
        ),
        (
            "index_access",
            r#"export function run(s: string): string { return s[0]; }"#,
        ),
        (
            "char_at",
            r#"export function run(s: string): string { return s.charAt(0); }"#,
        ),
        (
            "char_code_at",
            r#"export function run(s: string): number { return s.charCodeAt(0); }"#,
        ),
        (
            "code_point_at",
            r#"export function run(s: string): number { return s.codePointAt(0); }"#,
        ),
        (
            "from_code_point",
            r#"export function run(n: number): string { return String.fromCodePoint(n); }"#,
        ),
        (
            "from_char_code",
            r#"export function run(n: number): string { return String.fromCharCode(n); }"#,
        ),
        (
            "slice",
            r#"export function run(s: string): string { return s.slice(1, 3); }"#,
        ),
        (
            "index_of",
            r#"export function run(s: string): number { return s.indexOf("x"); }"#,
        ),
        (
            "split",
            r#"export function run(s: string): any { return s.split(""); }"#,
        ),
        (
            "concat",
            r#"export function run(a: string, b: string): string { return a + b; }"#,
        ),
        (
            "comparison",
            r#"export function run(a: string, b: string): boolean { return a < b; }"#,
        ),
        (
            "template",
            r#"export function run(a: string): string { return `val: ${a}`; }"#,
        ),
    ];

    for (name, ts) in cases {
        let ast = parse_typescript(ts, "probe_ops.ts")?;
        let hir = lower_module(&ast, "main", "probe_ops.ts")?;
        println!("=== OP CASE: {name} ===");
        for func in &hir.functions {
            println!(
                "  func {}: params={:?}, return_type={:?}",
                func.name, func.params, func.return_type
            );
            for stmt in &func.body {
                println!("    body stmt: {stmt:?}");
            }
        }
    }

    Ok(())
}

#[test]
fn probe_component_model_string_abi() -> Result<()> {
    // Probe 1: Core function returning string via retptr:
    // Core run signature: (param i32 i32 i32) -> void (arg_ptr, arg_len, retptr)
    // Or (param i32 i32) -> i32 (returning pointer to [ptr, len])
    let wat1 = r#"
    (component
      (core module $guest
        (memory (export "memory") 1)
        (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
          (i32.const 1024))
        (func (export "run") (param i32 i32 i32)
          ;; arg0: ptr, arg1: len, arg2: retptr
          (i32.store (local.get 2) (local.get 0))
          (i32.store offset=4 (local.get 2) (local.get 1)))
      )
      (core instance $guest (instantiate $guest))
      (func (export "run") (param "input" string) (result string)
        (canon lift (core func $guest "run")
          (memory (core memory $guest "memory"))
          (realloc (core func $guest "cabi_realloc"))))
    )
    "#;

    match wat::parse_str(wat1) {
        Ok(bytes) => {
            println!("wat1 parsed successfully! {} bytes", bytes.len());
            let engine = wasmtime::Engine::default();
            match wasmtime::component::Component::new(&engine, &bytes) {
                Ok(_) => println!("wat1 component compiled successfully!"),
                Err(e) => println!("wat1 compile error: {e}"),
            }
        }
        Err(e) => println!("wat1 parse error: {e}"),
    }

    // Probe 2: Can core function return string via (result i32)?
    let wat2 = r#"
    (component
      (core module $guest
        (memory (export "memory") 1)
        (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
          (i32.const 1024))
        (func (export "run") (param $ptr i32) (param $len i32) (result i32)
          ;; Store result descriptor at offset 2048:
          ;; descriptor.ptr = $ptr (same pointer)
          ;; descriptor.len = $len (same len)
          (i32.store (i32.const 2048) (local.get $ptr))
          (i32.store offset=4 (i32.const 2048) (local.get $len))
          (i32.const 2048))
      )
      (core instance $guest (instantiate $guest))
      (func (export "run") (param "input" string) (result string)
        (canon lift (core func $guest "run")
          (memory (core memory $guest "memory"))
          (realloc (core func $guest "cabi_realloc"))))
    )
    "#;
    let bytes = wat::parse_str(wat2)?;
    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    let engine = wasmtime::Engine::new(&config)?;
    let component = wasmtime::component::Component::new(&engine, &bytes)?;
    let mut store = wasmtime::Store::new(&engine, ());
    let linker = wasmtime::component::Linker::new(&engine);
    let instance = linker.instantiate(&mut store, &component)?;
    let run_fn = instance.get_typed_func::<(String,), (String,)>(&mut store, "run")?;
    let (output,) = run_fn.call(&mut store, ("hello from host 😀".to_string(),))?;
    println!("SUCCESS! Host got string output: {:?}", output);
    assert_eq!(output, "hello from host 😀");

    Ok(())
}
