//! UTC Date behavior, including alias-visible mutation and calendar rollover.
use anyhow::{Context, Result, ensure};
use wasmtime::{
    Engine, Store,
    component::{Component, Linker},
};

#[test]
fn utc_dates_match_node_through_parsing_mutation_and_invalid_values() -> Result<()> {
    let source = r#"
export function inspect(input: string, delta: number): string {
    const date = new Date(input);
    const alias = date;
    const copy = new Date(date);
    const parsed = Date.parse(input);
    const original = date.getTime();
    const year = date.getUTCFullYear();
    const month = date.getUTCMonth();
    const day = date.getUTCDate();
    const weekday = date.getUTCDay();
    const hour = date.getUTCHours();
    const minute = date.getUTCMinutes();
    const second = date.getUTCSeconds();
    const ms = date.getUTCMilliseconds();
    const changed = date.setUTCDate(day + delta);
    const seen = alias.getTime();
    date.setUTCFullYear(2024, 1, 29);
    date.setUTCHours(25, -2, 70, 1001);
    const iso = alias.toISOString();
    const changedMonth = date.setUTCMonth(-1, 32);
    date.setUTCMinutes(63, -4, -1);
    date.setUTCSeconds(61, 1001);
    date.setUTCMilliseconds(-1);
    const final = date.toISOString();
    const utc = Date.UTC(99, -1, 32, 25, 61, 61, 1001);
    const constructed = new Date(99, -1, 32, 25, 61, 61, 1001).getTime();
    date.setTime(-0.4);
    const zero = 1 / date.getTime();
    date.setTime(NaN);
    const invalid = date.setUTCMonth(2);
    const invalidJson = date.toJSON();
    const recovered = date.setUTCFullYear(0);
    const embedded = JSON.stringify([new Date(0), new Date(NaN)]);
    return JSON.stringify({ parsed, original, year, month, day, weekday, hour, minute, second, ms, changed, seen, copied: copy.getTime(), iso, changedMonth, final, utc, constructed, zero, invalid, invalidJson, recovered, embedded });
}
"#;
    let wit = "package test:dates; world dates { export inspect: func(input: string, delta: f64) -> string; }";
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("date.wit", wit)?;
    let world = resolve.select_world(&[package], Some("dates"))?;
    let compiled = perry_wit::compile_for_world(source, "date.ts", &resolve, world, true)?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let inspect = instance.get_typed_func::<(&str, f64), (String,)>(&mut store, "inspect")?;
    let directory = tempfile::tempdir()?;
    let script = directory.path().join("date.mts");
    std::fs::write(
        &script,
        format!("{source}\nconsole.log(inspect(process.argv[2], Number(process.argv[3])));"),
    )?;
    for (input, delta) in [
        ("1970-01-01T00:00:00.000Z", -1.0),
        ("1969-12-31T23:59:59.999Z", 32.0),
        ("2024-02-29T12:34:56.789+02:30", 365.0),
        ("2023-02-29", -31.0),
        ("2000-02-29T24:00Z", 0.0),
        ("0000-01-01", -366.0),
        ("-000001-12-31T23:59:59.999Z", 1.0),
        ("+275760-09-13T00:00:00.000Z", 0.0),
        ("-271821-04-20T00:00:00.000Z", 0.0),
        ("invalid", 10.0),
    ] {
        let node = std::process::Command::new("node")
            .env("TZ", "UTC")
            .arg("--experimental-strip-types")
            .arg(&script)
            .arg(input)
            .arg(delta.to_string())
            .output()
            .context("Node UTC date oracle")?;
        ensure!(
            node.status.success(),
            "Node: {}",
            String::from_utf8_lossy(&node.stderr)
        );
        let expected = String::from_utf8(node.stdout)?.trim_end().to_owned();
        let (actual,) = inspect.call(&mut store, (input, delta))?;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&actual)?,
            serde_json::from_str::<serde_json::Value>(&expected)?,
            "input={input} delta={delta}"
        );
    }
    Ok(())
}
