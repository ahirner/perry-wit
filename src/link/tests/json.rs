use anyhow::Result;
use perry_json_helper as json;
use wasmtime::{
    Engine, Global, GlobalType, Instance, Linker, Memory, MemoryType, Module, Mutability, Store,
    Val, ValType,
};

use crate::link::{self, libraries};

const JSON_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/json.wasm"));
const INPUT: u32 = 262_144;
const GRAPH: u32 = 327_680;

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

/// The graph the host build of the helper populates at `GRAPH`.
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

#[test]
fn parsed_numbers_round_like_core() {
    let mut inputs: Vec<String> = [
        "-0",
        "1e400",
        "-1e400",
        "1e-400",
        "1e21",
        "1e20",
        "1e-6",
        "1e-7",
        "2.9802322387695313e-8",
        "1000000000000000128",
        "5e-324",
        "2.2250738585072014e-308",
        "2.4703282292062328e-324",
        "1.7976931348623158e308",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let mut bits = 0xABCD_ABCD_ABCD_ABCDu64;
    for _ in 0..256 {
        bits = bits
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let number = f64::from_bits(bits);
        if number.is_finite() {
            inputs.push(number.to_string());
            inputs.push(format!("{number:e}"));
        }
    }
    inputs.push(format!("0.{}1", "0".repeat(1000)));
    inputs.push(format!("1{}e-1000", "2".repeat(1000)));
    for input in inputs {
        let mut graph = vec![0; json::measure(&input).unwrap()];
        let root = json::populate(&input, &mut graph, GRAPH).unwrap();
        let at = (root - GRAPH) as usize + 8;
        let parsed = f64::from_le_bytes(graph[at..at + 8].try_into().unwrap());
        let expected: f64 = input.parse().unwrap();
        assert_eq!(parsed.to_bits(), expected.to_bits(), "{input}");
    }
}

#[test]
fn parser_validates_complete_json_and_rejects_unpaired_escapes() {
    for input in [
        "",
        " ",
        "null false",
        "nul",
        "True",
        "01",
        "-",
        "1.",
        "1e",
        "[1,]",
        "{\"x\":}",
        "{x:1}",
        "\"a\n\"",
        r#""\x20""#,
        r#""\uD800""#,
        r#""\uDC00""#,
        r#"{"\uD800":1}"#,
    ] {
        assert!(json::measure(input).is_err(), "{input:?}");
    }
    assert!(matches!(
        json::measure(r#""\uD800""#),
        Err(json::Error::UnpairedSurrogate(1))
    ));
    let deep = format!("{}0{}", "[".repeat(128), "]".repeat(128));
    assert!(matches!(json::measure(&deep), Err(json::Error::Depth(_))));
}

fn normalize_numbers(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Number(number) => *value = serde_json::json!(number.as_f64().unwrap()),
        serde_json::Value::Array(values) => values.iter_mut().for_each(normalize_numbers),
        serde_json::Value::Object(values) => values.values_mut().for_each(normalize_numbers),
        _ => {}
    }
}
fn word(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}
fn value(bytes: &[u8], at: usize, base: usize) -> serde_json::Value {
    match word(bytes, at) {
        1 => serde_json::Value::Null,
        2 => serde_json::Value::Bool(word(bytes, at + 8) != 0),
        3 => serde_json::json!(f64::from_le_bytes(
            bytes[at + 8..at + 16].try_into().unwrap()
        )),
        4 => {
            let start = word(bytes, at + 8) - base;
            let end = start + word(bytes, at + 12);
            let text = std::str::from_utf8(&bytes[start..end]).unwrap();
            assert_eq!(word(bytes, at + 16), text.chars().count());
            serde_json::Value::String(text.into())
        }
        kind @ (5 | 6) => {
            let mut child = word(bytes, at + 16);
            let mut values = Vec::new();
            let mut object = serde_json::Map::new();
            for _ in 0..word(bytes, at + 20) {
                assert!(child >= base);
                let at = child - base;
                let item = value(bytes, at, base);
                if kind == 5 {
                    values.push(item);
                } else {
                    let key = value(bytes, word(bytes, at + 24) - base, base)
                        .as_str()
                        .unwrap()
                        .to_owned();
                    object.insert(key, item);
                }
                child = word(bytes, at + 4);
            }
            assert_eq!(child, 0);
            if kind == 5 {
                serde_json::Value::Array(values)
            } else {
                serde_json::Value::Object(object)
            }
        }
        kind => panic!("Invalid node kind {kind}"),
    }
}

#[test]
fn parser_populates_shared_storage_with_exact_types_and_unicode() -> Result<()> {
    for input in [
        "null",
        "true",
        "false",
        "0",
        "-0",
        "42.5",
        "1.2345678901234567e-200",
        r#""🦀\uD83D\uDE00\u0000\b\f\n\r\t\/\\\"""#,
        r#"[{},[],null,true,42.5,"ß"]"#,
        r#"{"a":{"b":["é",1,2]},"a":false,"__proto__":null}"#,
    ] {
        let size = json::measure(input).unwrap();
        let mut bytes = vec![0xFF; size];
        let root = json::populate(input, &mut bytes, 1024).unwrap();
        let mut expected: serde_json::Value = serde_json::from_str(input)?;
        normalize_numbers(&mut expected);
        assert_eq!(
            value(&bytes, root as usize - 1024, 1024),
            expected,
            "{input}"
        );
        assert_eq!(
            json::populate(input, &mut bytes[..size - 1], 1024),
            Err(json::Error::Capacity)
        );
    }
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
