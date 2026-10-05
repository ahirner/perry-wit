use anyhow::Result;
use wasmtime::{Engine, Instance, Memory, Module, Store};

#[path = "../src/waffle_backend/libraries.rs"]
mod libraries;
#[path = "../src/waffle_backend/link.rs"]
mod link;

const INPUT: u32 = 262_144;
const VALUE: u32 = 327_680;
const OUTPUT: u32 = 458_752;
const MEMORY_END: u32 = 524_288;

#[test]
fn linked_time_and_json_share_memory_and_keep_exact_immutable_values() -> Result<()> {
    let mut guest = GuestTime::new()?;
    let parse = guest
        .instance
        .get_typed_func::<(u32, u32, u32), u64>(&mut guest.store, "time_instant_parse")?;
    let format = guest
        .instance
        .get_typed_func::<(u32, u32, u32), u64>(&mut guest.store, "time_instant_format")?;
    let milliseconds = guest
        .instance
        .get_typed_func::<u32, f64>(&mut guest.store, "time_instant_ms")?;
    let json = guest
        .instance
        .get_typed_func::<(u32, u32), u64>(&mut guest.store, "json_measure")?;
    let reserved = guest.memory.data(&guest.store)[..INPUT as usize].to_vec();
    for _ in 0..256 {
        for (source, expected, nanos, millis) in [
            (
                "1970-01-01T00:00:00.000000001Z",
                "1970-01-01T00:00:00.000000001Z",
                1i128,
                0.0,
            ),
            (
                "1970-01-01T00:00:00+00:00:00.000000001",
                "1969-12-31T23:59:59.999999999Z",
                -1,
                -1.0,
            ),
            (
                "+275760-09-13T00:00:00Z",
                "+275760-09-13T00:00:00Z",
                8_640_000_000_000_000_000_000,
                8_640_000_000_000_000.0,
            ),
            (
                "-271821-04-20T00:00:00Z",
                "-271821-04-20T00:00:00Z",
                -8_640_000_000_000_000_000_000,
                -8_640_000_000_000_000.0,
            ),
        ] {
            guest
                .memory
                .write(&mut guest.store, INPUT as usize, source.as_bytes())?;
            assert_eq!(
                parse.call(&mut guest.store, (INPUT, source.len() as u32, VALUE))?,
                16
            );
            assert_eq!(guest.bytes(VALUE, 16), nanos.to_le_bytes());
            assert_eq!(milliseconds.call(&mut guest.store, VALUE)?, millis);
            assert_eq!(
                format.call(&mut guest.store, (VALUE, OUTPUT, 40))?,
                expected.len() as u64
            );
            assert_eq!(guest.bytes(OUTPUT, expected.len()), expected.as_bytes());
            assert_eq!(guest.bytes(VALUE, 16), nanos.to_le_bytes());
        }
        guest
            .memory
            .write(&mut guest.store, INPUT as usize, b"[1,true]")?;
        assert_eq!(json.call(&mut guest.store, (INPUT, 8))? >> 32, 0);
    }
    // Helper stack contents may change; static data and its preceding guest data may not.
    let mut placement = libraries::HelperMemory::new(59_004);
    placement.place(&libraries::Library::parse(libraries::JSON)?)?;
    placement.place(&libraries::Library::parse(libraries::TIME)?)?;
    let data_end = (placement.stack_top()? - 65_536) as usize;
    assert_eq!(
        &guest.memory.data(&guest.store)[..data_end],
        &reserved[..data_end]
    );
    assert_eq!(guest.memory.size(&guest.store), 8);
    Ok(())
}

#[test]
fn plain_calendar_days_and_fields_preserve_nanoseconds_without_a_zone() -> Result<()> {
    let mut guest = GuestTime::new()?;
    let parse = guest
        .instance
        .get_typed_func::<(u32, u32, u32), u64>(&mut guest.store, "time_plain_parse")?;
    let add = guest
        .instance
        .get_typed_func::<(u32, f64, u32), u64>(&mut guest.store, "time_plain_add_days")?;
    let part = guest
        .instance
        .get_typed_func::<(u32, u32), f64>(&mut guest.store, "time_plain_part")?;
    let format = guest
        .instance
        .get_typed_func::<(u32, u32, u32), u64>(&mut guest.store, "time_plain_format")?;
    let source = b"2024-02-28T23:58:57.123456789+03:00";
    guest
        .memory
        .write(&mut guest.store, INPUT as usize, source)?;
    assert_eq!(
        parse.call(&mut guest.store, (INPUT, source.len() as u32, VALUE))?,
        16
    );
    let original = guest.bytes(VALUE, 16).to_vec();
    assert_eq!(add.call(&mut guest.store, (VALUE, 2.0, VALUE + 16))?, 16);
    for (index, expected) in [2024.0, 3.0, 1.0, 23.0, 58.0, 57.0, 123.0, 456.0, 789.0, 5.0]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            part.call(&mut guest.store, (VALUE + 16, index as u32))?,
            expected
        );
    }
    let expected = b"2024-03-01T23:58:57.123456789";
    assert_eq!(
        format.call(&mut guest.store, (VALUE + 16, OUTPUT, 40))?,
        expected.len() as u64
    );
    assert_eq!(guest.bytes(OUTPUT, expected.len()), expected);
    assert_eq!(guest.bytes(VALUE, 16), original);
    for days in [f64::NAN, f64::INFINITY, 0.5, 200_000_003.0, -200_000_003.0] {
        let unchanged = guest.bytes(VALUE + 16, 16).to_vec();
        assert_eq!(
            add.call(&mut guest.store, (VALUE, days, VALUE + 16))?,
            2 << 32
        );
        assert_eq!(guest.bytes(VALUE + 16, 16), unchanged);
    }
    assert_eq!(part.call(&mut guest.store, (VALUE, 9))?, 3.0);
    assert!(part.call(&mut guest.store, (VALUE, 10))?.is_nan());
    Ok(())
}

#[test]
fn parsers_reject_bad_ranges_overlap_and_invalid_text_without_writes() -> Result<()> {
    let mut guest = GuestTime::new()?;
    let sentinel = [0xA5; 16];
    for name in ["time_instant_parse", "time_utc_parse", "time_plain_parse"] {
        let parse = guest
            .instance
            .get_typed_func::<(u32, u32, u32), u64>(&mut guest.store, name)?;
        guest
            .memory
            .write(&mut guest.store, VALUE as usize, &sentinel)?;
        for (pointer, length) in [(0, 1), (MEMORY_END, 1), (u32::MAX, 2), (INPUT, 0x8000_0000)] {
            assert_eq!(
                parse.call(&mut guest.store, (pointer, length, VALUE))?,
                6 << 32
            );
            assert_eq!(guest.bytes(VALUE, 16), sentinel);
        }
        for invalid in [
            b"\xff".as_slice(),
            b"2023-02-29T12:00:00Z",
            b"2024-01-01T24:00:00",
            b"",
            b"2024-01-01T00:00:00.1234567890Z",
        ] {
            guest
                .memory
                .write(&mut guest.store, INPUT as usize, invalid)?;
            assert_ne!(
                parse.call(&mut guest.store, (INPUT, invalid.len() as u32, VALUE))? >> 32,
                0
            );
            assert_eq!(guest.bytes(VALUE, 16), sentinel);
        }
        let source = b"2024-01-01T00:00:00Z";
        guest
            .memory
            .write(&mut guest.store, INPUT as usize, source)?;
        for output in [0, MEMORY_END - 15, u32::MAX, INPUT - 15, INPUT, INPUT + 18] {
            assert_eq!(
                parse.call(&mut guest.store, (INPUT, source.len() as u32, output))?,
                6 << 32
            );
            assert_eq!(guest.bytes(INPUT, source.len()), source);
        }
    }
    let strict = guest
        .instance
        .get_typed_func::<(u32, u32, u32), u64>(&mut guest.store, "time_utc_parse")?;
    for invalid in [
        "2024-01-01T00:00:00+00:00",
        "2024-01-01t00:00:00z",
        "2024-01-01T00:00:60Z",
    ] {
        guest
            .memory
            .write(&mut guest.store, INPUT as usize, invalid.as_bytes())?;
        assert_ne!(
            strict.call(&mut guest.store, (INPUT, invalid.len() as u32, VALUE))? >> 32,
            0
        );
        assert_eq!(guest.bytes(VALUE, 16), sentinel);
    }
    let valid = b"2024-01-01T00:00:00.123456789Z";
    guest
        .memory
        .write(&mut guest.store, INPUT as usize, valid)?;
    assert_eq!(
        strict.call(&mut guest.store, (INPUT, valid.len() as u32, VALUE))?,
        16
    );
    Ok(())
}

#[test]
fn accessors_and_formatters_validate_storage_and_capacity_before_writing() -> Result<()> {
    let mut guest = GuestTime::new()?;
    let from_ms = guest
        .instance
        .get_typed_func::<(f64, u32), u64>(&mut guest.store, "time_instant_from_ms")?;
    let milliseconds = guest
        .instance
        .get_typed_func::<u32, f64>(&mut guest.store, "time_instant_ms")?;
    let part = guest
        .instance
        .get_typed_func::<(u32, u32), f64>(&mut guest.store, "time_plain_part")?;
    let sentinel = [0xA5; 40];
    guest
        .memory
        .write(&mut guest.store, OUTPUT as usize, &sentinel)?;
    for name in ["time_instant_format", "time_plain_format"] {
        let format = guest
            .instance
            .get_typed_func::<(u32, u32, u32), u64>(&mut guest.store, name)?;
        guest
            .memory
            .write(&mut guest.store, VALUE as usize, &[0; 16])?;
        for (pointer, output, capacity) in [
            (0, OUTPUT, 40),
            (MEMORY_END - 15, OUTPUT, 40),
            (VALUE, 0, 40),
            (VALUE, MEMORY_END - 39, 40),
            (VALUE, OUTPUT, u32::MAX),
            (VALUE, VALUE, 40),
        ] {
            assert_eq!(
                format.call(&mut guest.store, (pointer, output, capacity))?,
                6 << 32
            );
            assert_eq!(guest.bytes(OUTPUT, 40), sentinel);
        }
        assert_eq!(format.call(&mut guest.store, (VALUE, OUTPUT, 18))?, 4 << 32);
        assert_eq!(format.call(&mut guest.store, (VALUE, 0, 0))?, 4 << 32);
        assert_eq!(guest.bytes(OUTPUT, 40), sentinel);
        guest
            .memory
            .write(&mut guest.store, VALUE as usize, &[0x7F; 16])?;
        assert_eq!(format.call(&mut guest.store, (VALUE, OUTPUT, 40))?, 2 << 32);
        assert_eq!(guest.bytes(OUTPUT, 40), sentinel);
    }
    for pointer in [0, MEMORY_END - 15, u32::MAX, VALUE] {
        assert!(milliseconds.call(&mut guest.store, pointer)?.is_nan());
        assert!(part.call(&mut guest.store, (pointer, 0))?.is_nan());
    }
    for value in [
        f64::NAN,
        f64::INFINITY,
        -f64::INFINITY,
        0.5,
        8_640_000_000_000_001.0,
    ] {
        assert_eq!(from_ms.call(&mut guest.store, (value, OUTPUT))?, 2 << 32);
        assert_eq!(guest.bytes(OUTPUT, 40), sentinel);
    }
    assert_eq!(from_ms.call(&mut guest.store, (-1.0, VALUE))?, 16);
    assert_eq!(milliseconds.call(&mut guest.store, VALUE)?, -1.0);
    assert_eq!(guest.bytes(VALUE, 16), (-1_000_000i128).to_le_bytes());
    assert_eq!(guest.memory.size(&guest.store), 8);
    Ok(())
}

struct GuestTime {
    store: Store<()>,
    instance: Instance,
    memory: Memory,
}

impl GuestTime {
    fn new() -> Result<Self> {
        let core = wat::parse_str(
            r#"(module
            (import "__perry_helper" "time_instant_parse" (func $instant_parse (param i32 i32 i32) (result i64)))
            (import "__perry_helper" "time_utc_parse" (func $utc_parse (param i32 i32 i32) (result i64)))
            (import "__perry_helper" "time_instant_from_ms" (func $from_ms (param f64 i32) (result i64)))
            (import "__perry_helper" "time_instant_ms" (func $ms (param i32) (result f64)))
            (import "__perry_helper" "time_instant_format" (func $instant_format (param i32 i32 i32) (result i64)))
            (import "__perry_helper" "time_plain_parse" (func $plain_parse (param i32 i32 i32) (result i64)))
            (import "__perry_helper" "time_plain_add_days" (func $add (param i32 f64 i32) (result i64)))
            (import "__perry_helper" "time_plain_part" (func $part (param i32 i32) (result f64)))
            (import "__perry_helper" "time_plain_format" (func $plain_format (param i32 i32 i32) (result i64)))
            (import "__perry_helper" "json_measure" (func $json (param i32 i32) (result i64)))
            (memory (export "memory") 8 8)
            (data (i32.const 59000) "keep")
            (export "time_instant_parse" (func $instant_parse))
            (export "time_utc_parse" (func $utc_parse))
            (export "time_instant_from_ms" (func $from_ms))
            (export "time_instant_ms" (func $ms))
            (export "time_instant_format" (func $instant_format))
            (export "time_plain_parse" (func $plain_parse))
            (export "time_plain_add_days" (func $add))
            (export "time_plain_part" (func $part))
            (export "time_plain_format" (func $plain_format))
            (export "json_measure" (func $json)))"#,
        )?;
        let linked = link::link_helpers(&core)?;
        let engine = Engine::default();
        let module = Module::new(&engine, &linked)?;
        assert_eq!(module.imports().count(), 0);
        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[])?;
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        Ok(Self {
            store,
            instance,
            memory,
        })
    }

    fn bytes(&self, pointer: u32, length: usize) -> &[u8] {
        &self.memory.data(&self.store)[pointer as usize..pointer as usize + length]
    }
}
