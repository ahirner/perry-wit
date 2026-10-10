use anyhow::Result;
use wasmtime::{Engine, Instance, Module, Store};

#[test]
fn static_modules_resolve_aliases_namespaces_and_reexports() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("left.ts"),
        "export function value(x: number): number { return x + 1; }",
    )?;
    std::fs::write(
        directory.path().join("right.ts"),
        "export function value(x: number): number { return x * 2; }",
    )?;
    let source = "import { value as left } from './left'; import * as right from './right.js'; export function run(x: number): number { return right.value(left(x)); }";
    let entry = directory.path().join("main.ts");
    let core = perry_wit::compile_core(source, &entry.to_string_lossy())?;
    let engine = Engine::default();
    let module = Module::new(&engine, core)?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;
    assert_eq!(
        instance
            .get_typed_func::<f64, f64>(&mut store, "run")?
            .call(&mut store, 5.0)?,
        12.0
    );

    let source = "export { value as run } from './left.js';";
    let core = perry_wit::compile_core(source, &entry.to_string_lossy())?;
    let module = Module::new(&engine, core)?;
    let instance = Instance::new(&mut store, &module, &[])?;
    assert_eq!(
        instance
            .get_typed_func::<f64, f64>(&mut store, "run")?
            .call(&mut store, 5.0)?,
        6.0
    );
    Ok(())
}

#[test]
fn typed_module_state_survives_exports_and_collection() -> Result<()> {
    let source = r#"
interface Cell { value: number; next: Cell | null }
let count = 7;
let saved: Cell = { value: 3, next: null };
saved.next = saved;
function build(value: number, next: Cell): Cell { if (value < 0) throw 0; return { value, next }; }
export function run(n: number): number {
    count++;
    for (let i = 0; i < n; i++) { const temporary = build(i, saved); saved.value = temporary.value; }
    return count + saved.next.value;
}
"#;
    let core = perry_wit::compile_core(source, "state.ts")?;
    let engine = Engine::default();
    let module = Module::new(&engine, core)?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;
    let run = instance.get_typed_func::<f64, (f64, i32)>(&mut store, "run")?;
    assert_eq!(run.call(&mut store, 200_000.0)?, (200_007.0, 0));
    assert_eq!(run.call(&mut store, 0.0)?, (200_008.0, 0));
    Ok(())
}
