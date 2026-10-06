//! Exercise adapter exits with deterministic native completion statuses.
#[path = "support/core_probe.rs"]
mod core_probe;

use anyhow::{Context, Result};
use perry_wit::waffle_backend::{WaffleCompileOptions, compile_typescript_for_world};
use waffle::{
    Export, ExportKind, Func, FuncDecl, FunctionBody, Module, Operator, Terminator, Type,
};
use wasmtime::{Engine, Linker, Store};

#[test]
fn cancelled_exports_invalidate_saved_borrows_and_drop_handles() -> Result<()> {
    let mut module = compile(
        "import type {Secret} from 'test:cleanup/secrets'; import {reveal} from 'test:cleanup/secrets';
         let saved:Secret|null=null;
         export function run(value:Secret):number {saved=value;throw 42;}
         export function stale():number {const value=saved;if(value===null)return 0;return reveal(value);}",
        "package test:cleanup; interface secrets {resource secret;reveal:func(value:borrow<secret>)->u32;}
         world boundary {import secrets;use secrets.{secret};export run:async func(value:borrow<secret>)->u32;export stale:func()->u32;}",
    )?;
    constant_return(&mut module, "operations.cancelled", 1)?;
    export(&mut module, "run.export")?;
    export(&mut module, "stale")?;
    let engine = Engine::default();
    let core = wasmtime::Module::new(&engine, module.to_wasm_bytes()?)?;
    let mut linker = Linker::<u32>::new(&engine);
    linker.func_wrap(
        "test:cleanup/secrets",
        "[resource-drop]secret",
        |mut caller: wasmtime::Caller<'_, u32>, raw: u32| {
            assert_eq!(raw, 42);
            *caller.data_mut() += 1;
        },
    )?;
    linker.func_wrap("test:cleanup/secrets", "reveal", |_: u32| -> u32 {
        panic!("expired borrow reached host")
    })?;
    let mut store = Store::new(&engine, 0);
    linker.define_unknown_imports_as_traps(&core)?;
    let instance = linker.instantiate(&mut store, &core)?;
    let run = instance.get_typed_func::<u32, u32>(&mut store, "probe-run.export")?;
    let stale = instance.get_typed_func::<(), (i32, f64)>(&mut store, "probe-stale")?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .context("missing memory")?;
    let active_roots: [u8; 4] = memory.data(&store)[44..48].try_into()?;
    assert_eq!(run.call(&mut store, 42)?, 0);
    assert_eq!(
        &memory.data(&store)[44..48],
        &active_roots,
        "cancelled export retained scratch frames"
    );
    assert!(stale.call(&mut store, ()).is_err());
    assert_eq!(*store.data(), 1);
    Ok(())
}

#[test]
fn cancelled_imports_drop_temporary_exported_resource_handles() -> Result<()> {
    for status in [2, 3] {
        let mut module = compile(
            "import type {Secret} from 'test:cleanup/secrets'; import {inspect} from 'test:cleanup/host';
             export async function secretsRun(value:Secret):Promise<void> {await inspect(value);}",
            "package test:cleanup;
             interface secrets {resource secret;run:async func(value:borrow<secret>);}
             interface host {use secrets.{secret};inspect:async func(value:borrow<secret>);}
             world boundary {import host;export secrets;}",
        )?;
        constant_return(&mut module, "operations.await", status)?;
        let imported = function(&module, "inspect.import")?;
        let callee = function(&module, "secretsRun")?;
        let signature = module.funcs[callee].sig();
        let mut body = FunctionBody::new(&module, signature);
        let entry = body.entry;
        let object = body.blocks[entry].params[0].1;
        // Catch the import's failure so repeated calls can expose leaked handles.
        body.add_op(
            entry,
            Operator::Call {
                function_index: imported,
            },
            &[object],
            &[Type::I32, Type::F64],
        );
        let payload = body.add_op(entry, Operator::F64Const { value: 0 }, &[], &[Type::F64]);
        let completed = body.add_op(entry, Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        body.set_terminator(
            entry,
            Terminator::Return {
                values: vec![completed, payload],
            },
        );
        module.funcs[callee] = FuncDecl::Body(signature, "secretsRun".into(), body);
        export(&mut module, "test:cleanup/secrets#run.export")?;
        let engine = Engine::default();
        let core = wasmtime::Module::new(&engine, module.to_wasm_bytes()?)?;
        let mut linker = Linker::<(u32, u32)>::new(&engine);
        linker.func_wrap(
            "[export]test:cleanup/secrets",
            "[resource-new]secret",
            |mut caller: wasmtime::Caller<'_, (u32, u32)>, raw: u32| -> u32 {
                assert_eq!(raw, 42);
                caller.data_mut().0 += 1;
                caller.data().0
            },
        )?;
        linker.func_wrap(
            "[export]test:cleanup/secrets",
            "[resource-drop]secret",
            |mut caller: wasmtime::Caller<'_, (u32, u32)>, handle: u32| {
                assert_eq!(handle, caller.data().0);
                caller.data_mut().1 += 1;
            },
        )?;
        linker.func_wrap(
            "test:cleanup/host",
            "[async-lower]inspect",
            |_: u32| -> u32 { 2 },
        )?;
        linker.define_unknown_imports_as_traps(&core)?;
        let mut store = Store::new(&engine, (0, 0));
        let instance = linker.instantiate(&mut store, &core)?;
        let run = instance
            .get_typed_func::<u32, ()>(&mut store, "probe-test:cleanup/secrets#run.export")?;
        for round in 1..=20 {
            run.call(&mut store, 42)?;
            assert_eq!(*store.data(), (round, round), "status={status}");
        }
    }
    Ok(())
}

fn compile(source: &str, wit: &str) -> Result<Module<'static>> {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("cleanup.wit", wit)?;
    let world = resolve.select_world(&[package], Some("boundary"))?;
    // WIT elaboration imports the exported resource interface as a dependency.
    // This core-only fixture removes that alias to exercise temporary handles
    // independently of the compiler's duplicate-resource restriction.
    let definition = &mut resolve.worlds[world];
    definition
        .imports
        .retain(|key, _| !definition.exports.contains_key(key));
    let compiled = compile_typescript_for_world(
        source,
        "cleanup.ts",
        &WaffleCompileOptions {
            componentize: false,
            ..Default::default()
        },
        resolve,
        world,
    )?;
    core_probe::named_core(&compiled)
}

fn function(module: &Module<'_>, name: &str) -> Result<Func> {
    module
        .funcs
        .entries()
        .find_map(|(id, function)| match function {
            FuncDecl::Body(_, actual, _) if actual == name => Some(id),
            _ => None,
        })
        .with_context(|| format!("missing function {name}"))
}

fn constant_return(module: &mut Module<'_>, name: &str, value: u32) -> Result<()> {
    let function = function(module, name)?;
    let signature = module.funcs[function].sig();
    let mut body = FunctionBody::new(module, signature);
    let values = module.signatures[signature]
        .returns
        .iter()
        .map(|ty| {
            assert_eq!(*ty, Type::I32);
            body.add_op(body.entry, Operator::I32Const { value }, &[], &[*ty])
        })
        .collect();
    body.set_terminator(body.entry, Terminator::Return { values });
    module.funcs[function] = FuncDecl::Body(signature, name.into(), body);
    Ok(())
}

fn export(module: &mut Module<'_>, name: &str) -> Result<()> {
    module.exports.push(Export {
        name: format!("probe-{name}"),
        kind: ExportKind::Func(function(module, name)?),
    });
    Ok(())
}
