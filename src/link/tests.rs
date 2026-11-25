use super::{libraries::Library, parsed::ParsedModule};

mod json;
mod memory;
mod time;

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
    let (linked, _) = super::link_helpers_with(core, None, &Default::default(), |module, name| {
        module == perry_codegen::HELPER_MODULE && name == "perry_alloc"
    })?;
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

#[test]
fn interns_recursive_groups_and_distinguishes_external_references() -> anyhow::Result<()> {
    let wasm = wat::parse_str(
        r#"(module
        (type $a (func (param (ref null $a))))
        (type $b (func (param (ref null $b))))
        (type $c (func (param (ref null $a))))
        (import "host" "one" (func (type $a)))
        (import "host" "two" (func (type $b)))
        (import "host" "three" (func (type $c)))
        (import "__perry_helper" "perry_task_enter" (func (result i32)))
        (memory (export "memory") 2))"#,
    )?;
    let wasm = super::link_helpers(wasm)?;
    wasmparser::Validator::new().validate_all(&wasm)?;
    let parsed = super::ParsedModule::parse(&wasm)?;
    let types: Vec<_> = parsed
        .imports
        .iter()
        .filter_map(|(module, _, ty)| (*module == "host").then_some(*ty))
        .collect();
    assert_eq!(types.len(), 3);
    assert_eq!(types[0], types[1]);
    assert_ne!(types[0], types[2]);
    Ok(())
}

#[test]
fn compacts_imports_calls_blocks_and_indirect_function_types() -> anyhow::Result<()> {
    let core = wat::parse_str(
        r#"(module
            (type $a (func (param i32) (result i32)))
            (type $b (func (param i32) (result i32)))
            (type $pair (func (param i32 i32) (result i32 i32)))
            (type $duplicate-pair (func (param i32 i32) (result i32 i32)))
            (type $unused (func (param i64 i64 i64) (result f64)))
            (import "host" "double" (func $double (type $b)))
            (import "__perry_helper" "perry_task_enter" (func (result i32)))
            (memory (export "memory") 2)
            (table 1 funcref)
            (elem (i32.const 0) $double)
            (func (export "run") (type $a) (param i32) (result i32)
                i32.const 0 i32.const 1
                (block (type $duplicate-pair))
                drop drop
                (call_indirect (type $b) (local.get 0) (i32.const 0))))"#,
    )?;
    let compact = super::link_helpers(core)?;
    let mut unique = std::collections::HashSet::new();
    for payload in wasmparser::Parser::new(0).parse_all(&compact) {
        if let wasmparser::Payload::TypeSection(s) = payload? {
            for ty in s.into_iter_err_on_gc_types() {
                let ty = ty?;
                assert_ne!(ty.params(), &[wasmparser::ValType::I64; 3]);
                assert!(unique.insert(ty), "duplicate linked signature");
            }
        }
    }
    assert!(!unique.is_empty());
    let engine = wasmtime::Engine::default();
    let module = wasmtime::Module::new(&engine, compact)?;
    let mut linker = wasmtime::Linker::new(&engine);
    linker.func_wrap("host", "double", |value: i32| value * 2)?;
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &module)?;
    assert_eq!(
        instance
            .get_typed_func::<i32, i32>(&mut store, "run")?
            .call(&mut store, 21)?,
        42
    );
    Ok(())
}
