use anyhow::Result;
use perry_json_helper as json;
use std::process::Command;
use wasmtime::{
    Engine, Global, GlobalType, Instance, Linker, Memory, MemoryType, Module, Mutability, Store,
    Val, ValType,
};

#[path = "../src/waffle_backend/libraries.rs"]
mod libraries;
#[path = "../src/waffle_backend/link.rs"]
mod link;

const JSON_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/json.wasm"));
const INPUT: u32 = 262_144;
const GRAPH: u32 = 327_680;
const OUTPUT: u32 = 458_752;

#[test]
fn linked_json_helpers_share_a_measured_layout_with_the_guest_heap() -> Result<()> {
    let mut placement = libraries::HelperMemory::new(59_004);
    placement.place(&libraries::Library::parse(libraries::JSON)?)?;
    let heap = libraries::align_to(placement.stack_top()?, 65_536)?;
    assert!(
        heap > 131_072,
        "this fixture crosses the old fixed helper reservation"
    );
    let core = wat::parse_str(
        r#"(module
        (import "__perry_helper" "json_measure" (func $measure (param i32 i32) (result i64)))
        (import "__perry_helper" "json_populate" (func $populate (param i32 i32 i32 i32) (result i64)))
        (import "__perry_helper" "json_serialized_size" (func $size (param i32 i32 i32) (result i64)))
        (import "__perry_helper" "json_serialize" (func $serialize (param i32 i32 i32 i32 i32) (result i64)))
        (memory (export "memory") 8 8)
        (data (i32.const 59000) "keep")
        (export "json_measure" (func $measure))
        (export "json_populate" (func $populate))
        (export "json_serialized_size" (func $size))
        (export "json_serialize" (func $serialize)))"#,
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
    let helper_data = guest.memory.data(&guest.store)
        [59_004..(placement.stack_top()? - 65_536) as usize]
        .to_vec();
    let deep = format!("{}0{}", "[".repeat(127), "]".repeat(127));
    guest
        .memory
        .write(&mut guest.store, heap as usize, deep.as_bytes())?;
    let size = guest.measure(heap, deep.len() as u32)?;
    assert_eq!(size >> 32, 0);
    let root = guest.populate(heap, deep.len() as u32, GRAPH, size as u32)?;
    assert_eq!(root >> 32, 0);
    assert_eq!(
        guest.serialize(GRAPH, size as u32, root as u32, OUTPUT, deep.len() as u32)?,
        deep.len() as u64
    );
    assert_eq!(
        &guest.memory.data(&guest.store)[59_004..59_004 + helper_data.len()],
        helper_data
    );
    let input = r#"{"b":5e-324,"1":"😀","a":1e21,"b":1e-7}"#;
    guest
        .memory
        .write(&mut guest.store, heap as usize, input.as_bytes())?;
    let size = guest.measure(heap, input.len() as u32)?;
    assert_eq!(size >> 32, 0);
    let root = guest.populate(heap, input.len() as u32, GRAPH, size as u32)?;
    assert_eq!(root >> 32, 0);
    let expected = r#"{"1":"😀","b":1e-7,"a":1e+21}"#;
    assert_eq!(
        guest.serialized_size(GRAPH, size as u32, root as u32)?,
        expected.len() as u64
    );
    assert_eq!(
        guest.serialize(GRAPH, size as u32, root as u32, OUTPUT, 128)?,
        expected.len() as u64
    );
    let mut bytes = vec![0; expected.len()];
    guest
        .memory
        .read(&guest.store, OUTPUT as usize, &mut bytes)?;
    assert_eq!(bytes, expected.as_bytes());
    let mut static_data = [0; 4];
    guest.memory.read(&guest.store, 59000, &mut static_data)?;
    assert_eq!(&static_data, b"keep");
    Ok(())
}

#[test]
fn guest_adapter_round_trips_json_in_disjoint_memory_without_growing() -> Result<()> {
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
    let reference = Command::new("node").args([
        "--input-type=module", "-e",
        "console.log(JSON.stringify(JSON.parse(process.argv[1]).map(text => JSON.stringify(JSON.parse(text)))))",
        &serde_json::to_string(&inputs)?,
    ]).output()?;
    assert!(
        reference.status.success(),
        "{}",
        String::from_utf8_lossy(&reference.stderr)
    );
    let expected: Vec<String> = serde_json::from_slice(&reference.stdout)?;
    for _ in 0..8 {
        for (input, expected) in inputs.iter().zip(&expected) {
            guest
                .memory
                .write(&mut guest.store, INPUT as usize, input.as_bytes())?;
            let length = input.len() as u32;
            let size = guest.measure(INPUT, length)?;
            assert_eq!(size >> 32, 0);
            let root = guest.populate(INPUT, length, GRAPH, size as u32)?;
            assert_eq!(root >> 32, 0);
            let output_size = guest.serialized_size(GRAPH, size as u32, root as u32)?;
            assert_eq!(output_size, expected.len() as u64);
            assert_eq!(
                guest.serialize(GRAPH, size as u32, root as u32, OUTPUT, output_size as u32)?,
                output_size
            );
            let mut output = vec![0; output_size as usize];
            guest
                .memory
                .read(&guest.store, OUTPUT as usize, &mut output)?;
            assert_eq!(std::str::from_utf8(&output)?, expected);
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
        assert_eq!(guest.serialized_size(pointer, length, pointer)?, 6 << 32);
        assert_eq!(
            guest.serialize(pointer, length, pointer, OUTPUT, 128)?,
            6 << 32
        );
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
fn guest_adapter_rejects_invalid_graphs_and_serialization_overlap_atomically() -> Result<()> {
    let mut guest = GuestJson::new()?;
    let input = r#"["valid"]"#;
    guest
        .memory
        .write(&mut guest.store, INPUT as usize, input.as_bytes())?;
    let size = guest.measure(INPUT, input.len() as u32)? as u32;
    let root = guest.populate(INPUT, input.len() as u32, GRAPH, size)? as u32;
    let sentinel = vec![0xA5; 128];
    guest
        .memory
        .write(&mut guest.store, OUTPUT as usize, &sentinel)?;
    assert_eq!(guest.serialize(GRAPH, size, root, OUTPUT, 1)?, 4 << 32);
    for output in [GRAPH, GRAPH + 1, GRAPH - 1] {
        assert_eq!(guest.serialize(GRAPH, size, root, output, 128)?, 6 << 32);
    }
    for (output, length) in [(0, 1), (524_288, 1), (u32::MAX, 2)] {
        assert_eq!(guest.serialize(GRAPH, size, root, output, length)?, 6 << 32);
    }
    for invalid in [0, GRAPH - 1, GRAPH + size, u32::MAX] {
        assert_eq!(guest.serialized_size(GRAPH, size, invalid)?, 5 << 32);
        assert_eq!(guest.serialize(GRAPH, size, invalid, OUTPUT, 128)?, 5 << 32);
    }
    // A cycle, an out-of-range child, and an invalid kind remain confined to this graph.
    for (offset, value) in [(16, root), (16, u32::MAX), (0, u32::MAX)] {
        guest.populate(INPUT, input.len() as u32, GRAPH, size)?;
        guest.memory.write(
            &mut guest.store,
            (root + offset) as usize,
            &value.to_le_bytes(),
        )?;
        let result = guest.serialized_size(GRAPH, size, root)?;
        assert!(matches!(result >> 32, 3 | 5));
        assert_eq!(guest.serialize(GRAPH, size, root, OUTPUT, 128)?, result);
    }
    let mut unchanged = vec![0; 128];
    guest
        .memory
        .read(&guest.store, OUTPUT as usize, &mut unchanged)?;
    assert_eq!(unchanged, sentinel);
    Ok(())
}

#[test]
fn serialization_matches_node_for_unicode_numbers_duplicate_keys_and_property_order() -> Result<()>
{
    let mut inputs: Vec<String> = [
        "null", "true", "false", "-0", "1e400", "-1e400", "1e-400", "1e21", "1e20", "1e-6", "1e-7",
        "2.9802322387695313e-8", "1000000000000000128", "5e-324", "2.2250738585072014e-308",
        r#""🦀\uD83D\uDE00\u0000\b\f\n\r\t\/\\\"""#,
        r#"{"2":"two","10":"ten","1":"one","b":1,"a":2,"b":3,"01":4,"4294967295":5,"4294967294":6,"0":7,"00":8}"#,
        r#"{"\u0061":1,"a":2,"__proto__":{"ok":true},"nested":[null,{},[],false]}"#,
    ].into_iter().map(str::to_owned).collect();
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
    inputs.push(format!("0.{}1", "0".repeat(1000)));
    inputs.push(format!("1{}e-1000", "2".repeat(1000)));
    let reference = Command::new("node").args([
        "--input-type=module", "-e",
        "console.log(JSON.stringify(JSON.parse(process.argv[1]).map(text => JSON.stringify(JSON.parse(text)))))",
        &serde_json::to_string(&inputs)?,
    ]).output()?;
    assert!(
        reference.status.success(),
        "{}",
        String::from_utf8_lossy(&reference.stderr)
    );
    let expected: Vec<String> = serde_json::from_slice(&reference.stdout)?;
    for (input, expected) in inputs.iter().zip(expected) {
        let mut graph = vec![0; json::measure(input).unwrap()];
        let root = json::populate(input, &mut graph, 4096).unwrap();
        let length = json::measure_serialized(&graph, 4096, root).unwrap();
        let mut result = vec![0; length];
        assert_eq!(
            json::serialize(&graph, 4096, root, &mut result).unwrap(),
            length
        );
        assert_eq!(std::str::from_utf8(&result)?, expected, "{input}");
        assert_eq!(
            json::serialize(&graph, 4096, root, &mut result[..length - 1]),
            Err(json::Error::Capacity)
        );
    }
    Ok(())
}

#[test]
fn serialization_checks_all_graph_ranges_and_kinds() {
    let mut graph = vec![0; json::measure(r#""valid""#).unwrap()];
    let root = json::populate(r#""valid""#, &mut graph, 4096).unwrap();
    for invalid in [0, 4095, u32::MAX] {
        assert_eq!(
            json::measure_serialized(&graph, 4096, invalid),
            Err(json::Error::InvalidGraph)
        );
    }
    graph[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        json::measure_serialized(&graph, 4096, root),
        Err(json::Error::InvalidGraph)
    );
    graph[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        json::measure_serialized(&graph, 4096, root),
        Err(json::Error::InvalidGraph)
    );
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

#[test]
fn parser_populates_shared_storage_with_exact_types_and_unicode() -> Result<()> {
    fn normalize_numbers(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Number(number) => {
                *value = serde_json::json!(number.as_f64().unwrap())
            }
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
        let instance = linker.instantiate(&mut store, &module)?;
        Ok(Self {
            store,
            instance,
            memory,
        })
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

    fn serialized_size(&mut self, pointer: u32, length: u32, root: u32) -> Result<u64> {
        Ok(self
            .instance
            .get_typed_func::<(u32, u32, u32), u64>(&mut self.store, "json_serialized_size")?
            .call(&mut self.store, (pointer, length, root))?)
    }

    fn serialize(
        &mut self,
        pointer: u32,
        length: u32,
        root: u32,
        output: u32,
        capacity: u32,
    ) -> Result<u64> {
        Ok(self
            .instance
            .get_typed_func::<(u32, u32, u32, u32, u32), u64>(&mut self.store, "json_serialize")?
            .call(&mut self.store, (pointer, length, root, output, capacity))?)
    }
}
