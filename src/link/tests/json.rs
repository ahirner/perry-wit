use anyhow::Result;
use perry_json as json;
use wasmtime::{
    Engine, Global, GlobalType, Instance, Linker, Memory, MemoryType, Module, Mutability, Store,
    Val, ValType,
};

use crate::link::{self, libraries};

const JSON_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime/json.wasm"));
const INPUT: u32 = 262_144;
const GRAPH: u32 = 327_680;

#[test]
fn descriptor_readers_link_only_their_input_mechanism() -> Result<()> {
    use libraries::{Library, LibraryId};
    use std::collections::BTreeSet;

    for (entry, reader) in [
        ("json_descriptor_copy", "copy"),
        ("json_descriptor_decode_cursor", "cursor"),
        ("json_descriptor_decode_graph", "graph"),
    ] {
        let library = Library::parse(LibraryId::for_entry(entry)?.bytes())?;
        let reachable = library.reachable([entry], &BTreeSet::new(), &BTreeSet::new())?;
        let imports: BTreeSet<_> = reachable
            .iter()
            .filter_map(|&index| {
                library
                    .module
                    .imports
                    .get(index as usize)
                    .map(|&(_, name, _)| name)
            })
            .collect();
        assert_eq!(
            imports.iter().any(|name| name.starts_with("json_cursor_")),
            reader == "cursor"
        );
        for name in [
            "std_json_require_number",
            "std_json_require_object",
            "std_json_member",
            "std_json_first",
            "std_json_next",
            "std_json_keys",
        ] {
            assert_eq!(
                imports.contains(name),
                reader == "graph",
                "{reader}: {name}"
            );
        }
        if reader == "copy" {
            let mut pending = Vec::from_iter(imports);
            let mut seen = BTreeSet::new();
            while let Some(entry) = pending.pop() {
                if !seen.insert(entry) {
                    continue;
                }
                let id = LibraryId::for_entry(entry)?;
                assert_ne!(
                    id,
                    LibraryId::Json,
                    "native copy links parser through {entry}"
                );
                let library = Library::parse(id.bytes())?;
                for index in library.reachable([entry], &BTreeSet::new(), &BTreeSet::new())? {
                    if let Some(&(_, name, _)) = library.module.imports.get(index as usize) {
                        pending.push(name);
                    }
                }
            }
        }
    }
    Ok(())
}

#[test]
fn linked_json_helpers_share_a_measured_layout_with_the_guest_heap() -> Result<()> {
    let mut placement = libraries::HelperMemory::new(59_004);
    placement.place(&libraries::Library::parse(libraries::JSON)?, None)?;
    let heap = libraries::align_to(placement.stack_top()?, 65_536)?;
    let core = wat::parse_str(
        r#"(module
        (import "__perry_helper" "json_measure" (func $measure (param i32 i32) (result i64)))
        (import "__perry_helper" "json_populate" (func $populate (param i32 i32 i32 i32) (result i64)))
        (memory (export "memory") 8 8)
        (data (i32.const 59000) "keep")
        (export "json_measure" (func $measure))
        (export "json_populate" (func $populate)))"#,
    )?;
    let linked = link::link_helpers(core)?;
    let engine = Engine::default();
    let module = Module::new(&engine, &linked)?;
    assert_eq!(module.imports().count(), 0);
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;
    let memory = instance.get_memory(&mut store, "memory").unwrap();
    let mut guest = GuestJson {
        store,
        instance,
        memory,
    };
    let helper_data =
        guest.memory.data(&guest.store)[59_004..placement.data_end() as usize].to_vec();
    for input in [
        format!("{}0{}", "[".repeat(127), "]".repeat(127)),
        r#"{"b":5e-324,"1":"😀","a":1e21,"b":1e-7}"#.to_string(),
    ] {
        guest
            .memory
            .write(&mut guest.store, heap as usize, input.as_bytes())?;
        let size = guest.measure(heap, input.len() as u32)?;
        assert_eq!(size >> 32, 0);
        let root = guest.populate(heap, input.len() as u32, GRAPH, size as u32)?;
        assert_eq!(root >> 32, 0);
        assert_eq!(guest.graph(size), host_graph(&input), "{input}");
    }
    assert_eq!(
        &guest.memory.data(&guest.store)[59_004..59_004 + helper_data.len()],
        helper_data
    );
    let mut static_data = [0; 4];
    guest.memory.read(&guest.store, 59000, &mut static_data)?;
    assert_eq!(&static_data, b"keep");
    Ok(())
}

/// The graph the host JSON codec populates at `GRAPH`.
fn host_graph(input: &str) -> Vec<u8> {
    let mut graph = vec![0; json::measure(input).unwrap()];
    json::populate(input, &mut graph, GRAPH).unwrap();
    graph
}

#[test]
fn guest_adapter_populates_like_the_host_in_disjoint_memory_without_growing() -> Result<()> {
    let mut guest = GuestJson::new()?;
    let mut inputs = vec![
        r#"{"2":"two","1":"one","b":1,"a":2,"b":3,"__proto__":[null,true,false]}"#.to_string(),
        r#""🦀\uD83D\uDE00\u0000\b\f\n\r\t\/\\\"""#.to_string(),
        "[1e400,-1e400,1e-400,-0,1e21,1e20,1e-6,1e-7,5e-324]".to_string(),
        format!("\"{}\"", "🦀é".repeat(8_000)),
        format!("{}0{}", "[".repeat(127), "]".repeat(127)),
    ];
    let mut bits = 0xABCD_ABCD_ABCD_ABCDu64;
    for _ in 0..256 {
        bits = bits
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let number = f64::from_bits(bits);
        if number.is_finite() {
            inputs.push(number.to_string());
        }
    }
    for _ in 0..8 {
        for input in &inputs {
            guest
                .memory
                .write(&mut guest.store, INPUT as usize, input.as_bytes())?;
            let length = input.len() as u32;
            let size = guest.measure(INPUT, length)?;
            assert_eq!(size >> 32, 0);
            let root = guest.populate(INPUT, length, GRAPH, size as u32)?;
            assert_eq!(root >> 32, 0);
            assert_eq!(guest.graph(size), host_graph(input), "{input}");
            let root = guest
                .instance
                .get_typed_func::<(u32, u32, u32, u32), u64>(
                    &mut guest.store,
                    "json_populate_measured",
                )?
                .call(&mut guest.store, (INPUT, length, GRAPH, size as u32))?;
            assert_eq!(root >> 32, 0);
            assert_eq!(guest.graph(size), host_graph(input), "measured {input}");
        }
    }
    assert_eq!(guest.memory.size(&guest.store), 8);
    Ok(())
}

#[test]
fn guest_adapter_rejects_ranges_overlap_utf8_and_invalid_json_before_writing() -> Result<()> {
    let mut guest = GuestJson::new()?;
    guest
        .memory
        .write(&mut guest.store, INPUT as usize, b"[true]")?;
    let sentinel = vec![0xA5; 128];
    guest
        .memory
        .write(&mut guest.store, GRAPH as usize, &sentinel)?;
    for (pointer, length) in [(0, 1), (524_288, 1), (u32::MAX, 2), (INPUT, 0x8000_0000)] {
        assert_eq!(guest.measure(pointer, length)?, 6 << 32);
        assert_eq!(guest.populate(pointer, length, GRAPH, 128)?, 6 << 32);
    }
    for (pointer, length) in [(0, 0), (524_288, 0)] {
        assert_eq!(guest.measure(pointer, length)? >> 32, 1);
    }
    for output in [INPUT, INPUT + 1, INPUT - 1] {
        assert_eq!(guest.populate(INPUT, 6, output, 128)?, 6 << 32);
    }
    for (output, length) in [(0, 1), (524_288, 1), (u32::MAX, 2)] {
        assert_eq!(guest.populate(INPUT, 6, output, length)?, 6 << 32);
    }
    assert_eq!(guest.populate(INPUT, 6, GRAPH, 1)?, 4 << 32);
    let deep = format!("{}0{}", "[".repeat(128), "]".repeat(128));
    for (input, error) in [
        (b"[1,]".as_slice(), 1),
        (b"\"\\uD800\"".as_slice(), 2),
        (b"\"\\uDC00\"".as_slice(), 2),
        (b"{\"\\uD800\":1}".as_slice(), 2),
        (deep.as_bytes(), 3),
        (b"\"\xFF\"".as_slice(), 7),
    ] {
        guest
            .memory
            .write(&mut guest.store, INPUT as usize, input)?;
        assert_eq!(guest.measure(INPUT, input.len() as u32)? >> 32, error);
        assert_eq!(
            guest.populate(INPUT, input.len() as u32, GRAPH, 128)? >> 32,
            error
        );
    }
    let mut unchanged = vec![0; 128];
    guest
        .memory
        .read(&guest.store, GRAPH as usize, &mut unchanged)?;
    assert_eq!(unchanged, sentinel);
    Ok(())
}

struct GuestJson {
    store: Store<()>,
    instance: Instance,
    memory: Memory,
}

impl GuestJson {
    fn new() -> Result<Self> {
        let engine = Engine::default();
        let module = Module::new(&engine, JSON_WASM)?;
        let mut store = Store::new(&engine, ());
        let memory = Memory::new(&mut store, MemoryType::new(8, Some(8)))?;
        let mut linker = Linker::new(&engine);
        linker.define(&store, "env", "memory", memory)?;
        for (name, value, mutable) in [
            ("__stack_pointer", 131_072, Mutability::Var),
            ("__memory_base", 1024, Mutability::Const),
            ("__table_base", 0, Mutability::Const),
        ] {
            let global = Global::new(
                &mut store,
                GlobalType::new(ValType::I32, mutable),
                Val::I32(value),
            )?;
            linker.define(&store, "env", name, global)?;
        }
        for import in module
            .imports()
            .filter(|import| import.module() == "__perry_helper")
        {
            if let wasmtime::ExternType::Func(ty) = import.ty() {
                linker.func_new(import.module(), import.name(), ty, |_, _, _| {
                    Err(wasmtime::Error::msg(
                        "raw codec called a typed-construction runtime import",
                    ))
                })?;
            }
        }
        let instance = linker.instantiate(&mut store, &module)?;
        Ok(Self {
            store,
            instance,
            memory,
        })
    }

    /// The `size` bytes of graph storage at `GRAPH`.
    fn graph(&self, size: u64) -> Vec<u8> {
        let start = GRAPH as usize;
        self.memory.data(&self.store)[start..start + size as u32 as usize].to_vec()
    }

    fn measure(&mut self, pointer: u32, length: u32) -> Result<u64> {
        Ok(self
            .instance
            .get_typed_func::<(u32, u32), u64>(&mut self.store, "json_measure")?
            .call(&mut self.store, (pointer, length))?)
    }

    fn populate(&mut self, pointer: u32, length: u32, output: u32, capacity: u32) -> Result<u64> {
        Ok(self
            .instance
            .get_typed_func::<(u32, u32, u32, u32), u64>(&mut self.store, "json_populate")?
            .call(&mut self.store, (pointer, length, output, capacity))?)
    }
}
