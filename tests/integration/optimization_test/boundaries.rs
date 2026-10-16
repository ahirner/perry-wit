//! Scalar export boundaries: which runtime pieces (memory, realloc, roots) are retained.
use super::{compile, inspect_wasm};
use anyhow::Result;

#[test]
fn independent_scalar_boundaries_have_no_runtime_roots() -> Result<()> {
    for source in [
        "export function run(a:number,b:number):number { return a+b; }",
        "export function run(a:number,b:number):number { return [a,b].length+a; }",
        "export function run(a:number,b:number):number { let sum=b; for(let i=0;i<a;i++)sum+=1; return sum; }",
        "export function run(a:number,b:number):number { return a+b; } export function unused():string { return String(Math.random()); }",
    ] {
        let compiled = compile(source, "func(a:f64,b:f64)->f64")?;
        let wasm = inspect_wasm(&compiled.core)?;
        assert_eq!(wasm.functions, 1, "export the implementation directly");
        assert_eq!(wasm.memories, 0);
        assert_eq!(wasm.imports, 0);
        assert_eq!(wasm.exports, ["run"]);
        assert!(compiled.core.len() < 2963);
        let engine = wasmtime::Engine::default();
        let (mut store, instance) =
            crate::world::instantiate(&engine, &compiled.component.unwrap().stripped)?;
        let run = instance.get_typed_func::<(f64, f64), (f64,)>(&mut store, "run")?;
        assert_eq!(run.call(&mut store, (5.0, 2.0))?, (7.0,));
    }
    Ok(())
}

#[test]
fn scalar_signatures_do_not_hide_runtime_effects() -> Result<()> {
    for source in [
        "export function run(a:number):number { return String(a).length; }",
        "function check(a:number):number { if(a<0)throw new Error('negative'); return a; } export function run(a:number):number { try { return check(a); } catch { return -1; } }",
        "let count=0; export function run(a:number):number { try { count+=a; return count; } catch { return -1; } }",
    ] {
        let compiled = compile(source, "func(a:f64)->f64")?;
        let wasm = inspect_wasm(&compiled.core)?;
        assert!(
            !wasm.has_start,
            "constant helper initialization must be resolved before instantiation"
        );
        assert!(!wasm.function_names.contains(&"__wasm_apply_data_relocs"));
        assert!(wasm.exports.contains(&"memory"), "{source}");
        assert!(wasm.exports.contains(&"cabi_post_run"), "{source}");
        assert!(!wasm.exports.contains(&"cabi_realloc"), "{source}");
        let engine = wasmtime::Engine::default();
        let (mut store, instance) =
            crate::world::instantiate(&engine, &compiled.component.unwrap().stripped)?;
        let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
        assert_eq!(run.call(&mut store, (1.0,))?, (1.0,));
    }
    Ok(())
}

#[test]
fn unused_host_imports_do_not_root_the_allocator() -> Result<()> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("unused.wit", "package test:unused; world test { import label: func() -> string; export run: func(a:f64)->f64; }")?;
    let world = resolve.select_world(&[package], Some("test"))?;
    let compiled = perry_wit::compile_for_world(
        "export function run(a:number):number { return String(a).length; }",
        "unused.ts",
        &resolve,
        world,
        true,
    )?;
    assert!(!inspect_wasm(&compiled.core)?.exports.contains(&"cabi_realloc"));
    // Component encoding validates the reachable import and export ABI without realloc.
    assert!(compiled.component.is_some());
    Ok(())
}
