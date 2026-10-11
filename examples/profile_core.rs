//! Allocation counters and fuel for a no-argument numeric core export.
//! Run with `cargo run --example profile_core -- source.ts [export]`.
use anyhow::{Context, Result};
use std::{fs, time::Instant};
use wasmtime::{Config, Engine, Instance, Module, Store};

fn main() -> Result<()> {
    let mut args = std::env::args();
    args.next();
    let path = args.next().context("source.ts required")?;
    let export = args.next().unwrap_or_else(|| "run".into());
    let source = fs::read_to_string(&path)?;
    let start = Instant::now();
    let wasm = perry_wit::compiler::compile_core_with_memory_checks(&source, &path)?;
    let compile_ms = start.elapsed().as_secs_f64() * 1000.0;
    let mut config = Config::new();
    config.consume_fuel(true);
    let engine = Engine::new(&config)?;
    let module = Module::new(&engine, &wasm)?;
    let mut store = Store::new(&engine, ());
    store.set_fuel(u64::MAX)?;
    let instance = Instance::new(&mut store, &module, &[])?;
    let function = instance
        .get_func(&mut store, &export)
        .context("export missing")?;
    let mut results = vec![wasmtime::Val::I32(0); function.ty(&store).results().len()];
    function.call(&mut store, &[], &mut results)?;
    let fuel = u64::MAX - store.get_fuel()?;
    let counter = instance.get_typed_func::<i32, i64>(&mut store, "__perry_gc_counter")?;
    let allocations = counter.call(&mut store, 0)?;
    let collections = counter.call(&mut store, 1)?;
    let live_bytes = counter.call(&mut store, 2)?;
    let memory_bytes = instance
        .get_memory(&mut store, "memory")
        .context("memory missing")?
        .data_size(&store);
    println!(
        "{}",
        serde_json::json!({
            "instrumented":true,"compileMs":compile_ms,"coreBytes":wasm.len(),
            "fuel":fuel,"allocations":allocations,"collections":collections,"liveBytes":live_bytes,
            "linearMemoryBytes":memory_bytes,"output":format!("{results:?}")
        })
    );
    Ok(())
}
