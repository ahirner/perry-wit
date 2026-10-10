//! Node differential coverage for the declared HTTP(S) URL surface.
use anyhow::{Context, Result, ensure};
use wasmtime::{
    Engine, Store,
    component::{Component, Linker},
};

#[test]
fn http_urls_match_node_for_construction_mutation_and_serialization() -> Result<()> {
    let source = r#"
export function inspect(input: string, base: string): string {
    const url = new URL(input, base);
    const params = url.searchParams;
    const before = url.href;
    const protocol = url.protocol;
    const username = url.username;
    const password = url.password;
    const hash = url.hash;
    url.pathname = "/next/../é?x#y";
    params.append("a b", "é🙂 ~!'");
    return JSON.stringify([before, protocol, username, password, hash, url.pathname, url.toString(), encodeURIComponent("é🙂 !~*'()/?:")]);
}
"#;
    let wit = "package test:url; world urls { export inspect: func(input: string, base: string) -> string; }";
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("url.wit", wit)?;
    let world = resolve.select_world(&[package], Some("urls"))?;
    let compiled = perry_wit::compile_for_world(source, "url.ts", &resolve, world, true)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let inspect = instance.get_typed_func::<(&str, &str), (String,)>(&mut store, "inspect")?;
    let directory = tempfile::tempdir()?;
    let script = directory.path().join("url.mts");
    std::fs::write(
        &script,
        format!("{source}\nconsole.log(inspect(process.argv[2], process.argv[3]));"),
    )?;
    for (input, base) in [
        (
            "https://Example.com:443/a/../b?x=~+%20&&bad=%FF#frag ment",
            "http://base.test/",
        ),
        (
            "../a?q=one%20two#é",
            "https://u:p@base.test/path/file?old=1#old",
        ),
        ("https://a@b:secret@example.test/a", "https://base.test/"),
        ("//[2001:0db8::1]:443/a", "https://base.test/"),
        ("#next", "http://base.test/path?q=x#old"),
        ("https://:pass@example.test/?#", "https://base.test/"),
    ] {
        let node = std::process::Command::new("node")
            .arg("--experimental-strip-types")
            .arg(&script)
            .args([input, base])
            .output()
            .context("Node URL oracle")?;
        ensure!(
            node.status.success(),
            "Node failed: {}",
            String::from_utf8_lossy(&node.stderr)
        );
        let expected = String::from_utf8(node.stdout)?.trim_end().to_owned();
        let (actual,) = inspect.call(&mut store, (input, base))?;
        assert_eq!(actual, expected, "new URL({input:?}, {base:?})");
    }
    Ok(())
}
