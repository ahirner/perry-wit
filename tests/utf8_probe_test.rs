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
        ("lone_surrogate_with_ascii", r#"let s = "prefix\uD83Dsuffix";"#),
        ("template_lone_surrogate", r#"let s = `template\uD83D`;"#),
        ("property_key_lone_surrogate", r#"let obj = { "\uD83D": 123 };"#),
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
        ("length", r#"export function run(s: string): number { return s.length; }"#),
        ("index_access", r#"export function run(s: string): string { return s[0]; }"#),
        ("char_at", r#"export function run(s: string): string { return s.charAt(0); }"#),
        ("char_code_at", r#"export function run(s: string): number { return s.charCodeAt(0); }"#),
        ("code_point_at", r#"export function run(s: string): number { return s.codePointAt(0); }"#),
        ("from_code_point", r#"export function run(n: number): string { return String.fromCodePoint(n); }"#),
        ("from_char_code", r#"export function run(n: number): string { return String.fromCharCode(n); }"#),
        ("slice", r#"export function run(s: string): string { return s.slice(1, 3); }"#),
        ("index_of", r#"export function run(s: string): number { return s.indexOf("x"); }"#),
        ("split", r#"export function run(s: string): any { return s.split(""); }"#),
        ("concat", r#"export function run(a: string, b: string): string { return a + b; }"#),
        ("comparison", r#"export function run(a: string, b: string): boolean { return a < b; }"#),
        ("template", r#"export function run(a: string): string { return `val: ${a}`; }"#),
    ];

    for (name, ts) in cases {
        let ast = parse_typescript(ts, "probe_ops.ts")?;
        let hir = lower_module(&ast, "main", "probe_ops.ts")?;
        println!("=== OP CASE: {name} ===");
        for func in &hir.functions {
            println!("  func {}: params={:?}, return_type={:?}", func.name, func.params, func.return_type);
            for stmt in &func.body {
                println!("    body stmt: {stmt:?}");
            }
        }
    }

    Ok(())
}
