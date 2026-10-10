//! Literal regex search and tests have bounded compile-time DFAs and no guest allocation.
use anyhow::{Context, Result, ensure};
use wasmtime::{
    Engine, Store,
    component::{Component, Linker},
};

#[test]
fn literal_search_matches_node_over_bmp_text() -> Result<()> {
    let patterns = [
        "/a+/",
        "/a|é/u",
        "/漢.*/u",
        "/[a-z0-9_]+/",
        "/^a.*z$/s",
        "/\\bword\\b/u",
        "/\\s+\\d{1,3}/u",
        "/[^\\w]+/",
        "/(?:ab|cd){1,3}/",
        "/a*?/",
        "/[]/",
        "/[^]/u",
        "/\\u{6f22}/u",
        "/x$/",
        "/[\\d\\s]+/",
        "/a.b/",
    ];
    let inputs = [
        "",
        "abc",
        "word words",
        "_word_",
        "é漢123",
        " 42",
        "a\nb",
        "a\rz",
        "x\n",
        "x\r\n",
        "a\u{2028}b",
        "\u{feff}42",
        "ababcdaaz",
    ];
    let engine = Engine::default();
    let directory = tempfile::tempdir()?;
    let script = directory.path().join("regex.mts");
    for pattern in patterns {
        let source = format!(
            "export function inspect(input: string): number {{ const expression = {pattern}; return input.search(expression) * 2 + (expression.test(input) ? 1 : 0); }}"
        );
        let wit = "package test:regex; world regex { export inspect: func(input: string) -> f64; }";
        let mut resolve = wit_parser::Resolve::default();
        let package = resolve.push_str("regex.wit", wit)?;
        let world = resolve.select_world(&[package], Some("regex"))?;
        let compiled = perry_wit::compile_for_world(&source, "regex.ts", &resolve, world, true)
            .with_context(|| pattern.to_owned())?;
        let component = Component::new(&engine, compiled.stripped.unwrap())?;
        let mut store = Store::new(&engine, ());
        let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
        let inspect = instance.get_typed_func::<(&str,), (f64,)>(&mut store, "inspect")?;
        std::fs::write(
            &script,
            format!(
                "{source}\nconsole.log(JSON.stringify({}.map(inspect)));",
                serde_json::to_string(&inputs)?
            ),
        )?;
        let node = std::process::Command::new("node")
            .arg("--experimental-strip-types")
            .arg(&script)
            .output()
            .context("Node regex oracle")?;
        ensure!(
            node.status.success(),
            "Node: {}",
            String::from_utf8_lossy(&node.stderr)
        );
        let expected: Vec<f64> = serde_json::from_slice(&node.stdout)?;
        for (input, expected) in inputs.into_iter().zip(expected) {
            assert_eq!(
                inspect.call(&mut store, (input,))?.0,
                expected,
                "pattern={pattern} input={input:?}"
            );
        }
    }
    Ok(())
}

#[test]
fn unsupported_regex_forms_have_diagnostics() {
    for (expression, message) in [
        ("/a/g", "supports only"),
        ("/\\u{61}/", "require the u flag"),
        ("/(?=a)/", "lookaround"),
        ("/(a)\\1/", "Unsupported regex escape"),
        ("/\\uD800/u", "surrogate"),
        ("new RegExp(input)", "RegExp construction is unsupported"),
    ] {
        let source = format!(
            "export function test(input: string): boolean {{ return ({expression}).test(input); }}"
        );
        let error = perry_wit::compile_core(&source, "unsupported-regex.ts").unwrap_err();
        assert!(format!("{error:#}").contains(message), "{error:#}");
    }
}

#[test]
fn literal_regex_identity_is_diagnosed() {
    let error = perry_wit::compile_core(
        "export function test(): boolean { return /a/ === /a/; }",
        "regex-identity.ts",
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("regex object identity"),
        "{error:#}"
    );
}
