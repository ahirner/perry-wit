//! Binary structure and behavior of optimized WIT-constrained programs.
use anyhow::Result;
use wasmparser::{Parser, Payload};

fn compile(source: &str, signature: &str) -> Result<perry_wit::Compiled> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str(
        "optimization.wit",
        &format!("package test:optimization; world test {{ export run: {signature}; }}"),
    )?;
    let world = resolve.select_world(&[package], Some("test"))?;
    perry_wit::compile_for_world(source, "optimization.ts", &resolve, world, true)
}

#[test]
fn independent_scalar_boundaries_have_no_runtime_roots() -> Result<()> {
    for source in [
        "export function run(a:number,b:number):number { return a+b; }",
        "export function run(a:number,b:number):number { let sum=b; for(let i=0;i<a;i++)sum+=1; return sum; }",
        "export function run(a:number,b:number):number { return a+b; } export function unused():string { return String(Math.random()); }",
    ] {
        let compiled = compile(source, "func(a:f64,b:f64)->f64")?;
        let mut functions = 0;
        let mut exports = Vec::new();
        for payload in Parser::new(0).parse_all(&compiled.core) {
            match payload? {
                Payload::FunctionSection(section) => functions = section.count(),
                Payload::MemorySection(section) => assert_eq!(section.count(), 0),
                Payload::ImportSection(section) => assert_eq!(section.count(), 0),
                Payload::ExportSection(section) => {
                    exports = section
                        .into_iter()
                        .map(|e| e.map(|e| e.name.to_owned()))
                        .collect::<std::result::Result<_, _>>()?;
                }
                _ => {}
            }
        }
        assert_eq!(functions, 2, "implementation and wrapper only");
        assert_eq!(exports, ["run"]);
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
        "function check(a:number):number { if(a<0)throw new Error('negative'); return a; } export function run(a:number):number { return check(a); }",
        "let count=0; export function run(a:number):number { count+=a; return count; }",
    ] {
        let compiled = compile(source, "func(a:f64)->f64")?;
        let mut exports = Vec::new();
        for payload in Parser::new(0).parse_all(&compiled.core) {
            if let Payload::ExportSection(section) = payload? {
                exports = section
                    .into_iter()
                    .map(|e| e.map(|e| e.name.to_owned()))
                    .collect::<std::result::Result<_, _>>()?;
            }
        }
        assert!(exports.iter().any(|e| e == "memory"), "{source}");
        assert!(exports.iter().any(|e| e == "cabi_post_run"), "{source}");
        let engine = wasmtime::Engine::default();
        let (mut store, instance) =
            crate::world::instantiate(&engine, &compiled.component.unwrap().stripped)?;
        let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
        assert_eq!(run.call(&mut store, (1.0,))?, (1.0,));
    }
    Ok(())
}
