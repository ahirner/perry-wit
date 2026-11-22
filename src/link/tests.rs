use super::{libraries::Library, parsed::ParsedModule};

mod json;
mod memory;
mod time;

#[test]
fn direct_runtime_imports_bind_to_the_world_without_linking_helpers() -> anyhow::Result<()> {
    use super::{RuntimeImports, canonical::Requirements, link_helpers_with};
    use perry_contract::target::Target;

    let native = "wasi:clocks/system-clock@0.3.0";
    let older = "wasi:clocks/wall-clock@0.2.9";
    let core = wat::parse_str(format!(r#"(module (import "{native}" "now" (func)))"#))?;
    for (target, interfaces, expected) in [
        (Target::P2, vec![older.into()], older),
        (Target::P3, vec![older.into()], older),
        (Target::P3, vec![native.into(), older.into()], native),
    ] {
        let (linked, helpers) = link_helpers_with(
            core.clone(),
            None,
            &Requirements::default(),
            &RuntimeImports {
                target,
                interfaces: &interfaces,
            },
            |_, _| false,
        )?;
        assert_eq!(helpers, 0);
        assert_eq!(ParsedModule::parse(&linked)?.imports[0].0, expected);
    }
    Ok(())
}

#[test]
fn callbacks_invoked_by_another_library_keep_their_dependencies() -> anyhow::Result<()> {
    let bytes = wat::parse_str(
        r#"(module
        (import "env" "memory" (memory 1))
        (import "env" "__table_base" (global $table-base i32))
        (import "env" "__indirect_function_table" (table 1 funcref))
        (import "host" "complete" (func $complete))
        (func $step (call $complete))
        (elem (global.get $table-base) func $step)
        (func (export "register") (result i32) (global.get $table-base)))"#,
    )?;
    let mut library = Library::parse(&bytes)?;
    let empty = std::collections::BTreeSet::new();
    let ordinary = library.reachable(["register"], &empty, &empty)?;
    assert!(
        !ordinary.contains(&0),
        "no local indirect call reaches the host"
    );
    library.external_callbacks = true;
    let callbacks = library.reachable(["register"], &empty, &empty)?;
    assert!(
        callbacks.contains(&0),
        "the externally invoked callback needs its host import"
    );
    assert!(callbacks.contains(&1));
    Ok(())
}

#[test]
fn linked_sections_keep_references_initialization_and_passive_segments() -> anyhow::Result<()> {
    let core = wat::parse_str(
        r#"(module
        (type $value (func (result i32)))
        (import "__perry_helper" "json_measure" (func $measure (param i32 i32) (result i64)))
        (import "__perry_helper" "perry_task_enter" (func $enter (result i32)))
        (import "__perry_helper" "perry_alloc" (func $alloc (param i32) (result i32)))
        (import "__perry_helper" "std_bytes_new" (func $bytes (param f64) (result i32)))
        (import "host" "value" (func $host (type $value)))
        (memory (export "memory") 2)
        (table 1 1 funcref)
        (global (export "callback") funcref (ref.func $host))
        (global $started (mut i32) (i32.const 0))
        (elem (i32.const 0) func $host)
        (elem $functions func $host)
        (data $bytes "K")
        (func $start
            call $enter drop
            i32.const 8 i32.const 0 i32.const 1 memory.init $bytes data.drop $bytes
            i32.const 0 i32.const 0 i32.const 1 table.init $functions elem.drop $functions
            i32.const 1 global.set $started)
        (start $start)
        (export "alloc" (func $alloc))
        (export "bytes" (func $bytes))
        (func (export "run") (result i32)
            i32.const 0 call_indirect (type $value)
            global.get $started i32.add i32.const 8 i32.load8_u i32.add)
        (export "measure" (func $measure)))"#,
    )?;
    let (linked, _) = super::link_helpers_with(
        core,
        None,
        &Default::default(),
        &super::RuntimeImports::RAW,
        |module, name| module == perry_codegen::HELPER_MODULE && name == "perry_alloc",
    )?;
    let engine = wasmtime::Engine::default();
    let module = wasmtime::Module::new(&engine, &linked)?;
    let mut linker = wasmtime::Linker::new(&engine);
    linker.func_wrap("host", "value", || 7i32)?;
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &module)?;
    // The guest imports the allocator directly. The byte constructor calls it from a library.
    let alloc = instance.get_typed_func::<i32, i32>(&mut store, "alloc")?;
    assert_ne!(alloc.call(&mut store, 8)?, 0);
    let bytes = instance.get_typed_func::<f64, i32>(&mut store, "bytes")?;
    let Err(trap) = bytes.call(&mut store, 8.0) else {
        anyhow::bail!("library call bypassed its selected stub");
    };
    assert_eq!(
        trap.downcast_ref::<wasmtime::Trap>(),
        Some(&wasmtime::Trap::UnreachableCodeReached)
    );
    assert_eq!(
        instance
            .get_typed_func::<(), i32>(&mut store, "run")?
            .call(&mut store, ())?,
        83
    );
    let callback = instance
        .get_global(&mut store, "callback")
        .expect("exported callback")
        .get(&mut store);
    let wasmtime::Val::FuncRef(Some(callback)) = callback else {
        anyhow::bail!("missing function reference");
    };
    assert_eq!(callback.typed::<(), i32>(&store)?.call(&mut store, ())?, 7);
    Ok(())
}

#[test]
fn section_readers_reject_malformed_and_unsupported_helpers() -> anyhow::Result<()> {
    for source in [
        "(module (memory 1))",
        "(module (table 1 funcref))",
        "(module (tag))",
        "(module (global (mut i32) (i32.const 0)))",
        "(module (import \"env\" \"memory\" (memory 1)) (data \"passive\"))",
        "(module (import \"env\" \"__indirect_function_table\" (table 1 funcref)) (elem (i32.const 0) func))",
    ] {
        let bytes = wat::parse_str(source)?;
        assert!(Library::parse(&bytes).is_err(), "{source}");
    }
    // A later section must never hide an earlier malformed reader.
    for section in [1, 2, 3, 4, 5, 6, 7, 9, 11, 13] {
        let mut module = wasm_encoder::Module::new();
        module.section(&wasm_encoder::RawSection {
            id: section,
            data: &[0],
        });
        module.section(&wasm_encoder::RawSection {
            id: section,
            data: &[0],
        });
        assert!(
            ParsedModule::parse(&module.finish()).is_err(),
            "section {section}"
        );
    }
    for section in [6, 7, 9, 11] {
        let mut module = wasm_encoder::Module::new();
        module.section(&wasm_encoder::RawSection {
            id: section,
            data: &[1],
        });
        assert!(
            Library::parse(&module.finish()).is_err(),
            "truncated section {section}"
        );
    }
    Ok(())
}

#[test]
fn library_data_skips_zero_runs_longer_than_a_segment_header() {
    let mut contents = vec![0, 0, 1, 2, 0, 0, 3];
    contents.extend([0; 9]);
    contents.extend([4, 0, 0]);
    assert_eq!(
        super::nonzero_runs(&contents).collect::<Vec<_>>(),
        [(2, &[1, 2, 0, 0, 3][..]), (16, &[4][..])]
    );
    assert!(super::nonzero_runs(&[0; 4]).next().is_none());
}
