//! Collector-aware non-moving growth, exercised through the actual linked Wasm allocator.
use crate::link;
use anyhow::Result;
use wasmtime::{Engine, Instance, Memory, Module, Store};

struct Heap {
    store: Store<()>,
    instance: Instance,
    memory: Memory,
}
impl Heap {
    fn new() -> Result<Self> {
        let core = wat::parse_str(
            r#"(module
          (import "__perry_helper" "perry_gc_alloc" (func $alloc (param i32 i32) (result i32)))
          (import "__perry_helper" "perry_buffer_resize" (func $resize (param i32 i32 i32 i32 i32) (result i32)))
          (import "__perry_helper" "perry_roots_push" (func $push (param i32)))
          (import "__perry_helper" "perry_roots_reset" (func $reset (param i32)))
          (import "__perry_helper" "perry_gc_debug" (func $debug (param i32)))
          (import "__perry_helper" "perry_gc_check" (func $collect))
          (import "__perry_helper" "perry_gc_counter" (func $counter (param i32) (result i64)))
          (import "__perry_helper" "perry_gc_pin" (func $pin (param i32) (result i32)))
          (import "__perry_helper" "perry_gc_unpin" (func $unpin (param i32)))
          (memory (export "memory") 1 64)
          (export "alloc" (func $alloc)) (export "resize" (func $resize))
          (export "push" (func $push)) (export "reset" (func $reset))
          (export "debug" (func $debug)) (export "collect" (func $collect))
          (export "counter" (func $counter)) (export "pin" (func $pin)) (export "unpin" (func $unpin)))"#,
        )?;
        let engine = Engine::default();
        let module = Module::new(&engine, link::link_helpers(core)?)?;
        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[])?;
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        let mut heap = Self {
            store,
            instance,
            memory,
        };
        heap.unit("debug", 7)?;
        // Initialize the root stack and directory before the large payload becomes the tail.
        heap.unit("push", 0)?;
        heap.unit("reset", 0)?;
        heap.alloc(32)?;
        Ok(heap)
    }
    fn unit(&mut self, name: &str, value: u32) -> Result<()> {
        Ok(self
            .instance
            .get_typed_func::<u32, ()>(&mut self.store, name)?
            .call(&mut self.store, value)?)
    }
    fn alloc(&mut self, size: u32) -> Result<u32> {
        Ok(self
            .instance
            .get_typed_func::<(u32, u32), u32>(&mut self.store, "alloc")?
            .call(&mut self.store, (size, 0))?)
    }
    fn resize(&mut self, pointer: u32, old: u32, size: u32, initialized: u32) -> Result<u32> {
        Ok(self
            .instance
            .get_typed_func::<(u32, u32, u32, u32, u32), u32>(&mut self.store, "resize")?
            .call(&mut self.store, (pointer, old, size, initialized, 0))?)
    }
    fn counter(&mut self, index: u32) -> Result<u64> {
        Ok(self
            .instance
            .get_typed_func::<u32, u64>(&mut self.store, "counter")?
            .call(&mut self.store, index)?)
    }
    fn collect(&mut self) -> Result<()> {
        Ok(self
            .instance
            .get_typed_func::<(), ()>(&mut self.store, "collect")?
            .call(&mut self.store, ())?)
    }
}

#[test]
fn tail_growth_preserves_bytes_updates_directory_and_reclaims() -> Result<()> {
    for size in [131072, 2 * 1024 * 1024] {
        let mut heap = Heap::new()?;
        let pointer = heap.alloc(8192)?;
        heap.unit("push", pointer)?;
        heap.memory
            .write(&mut heap.store, pointer as usize, b"retained prefix")?;
        let grown = heap.resize(pointer, 8192, size, 8192)?;
        assert_eq!(
            grown, pointer,
            "eligible tail allocation should grow without copying"
        );
        assert_eq!(
            &heap.memory.data(&heap.store)[pointer as usize..pointer as usize + 15],
            b"retained prefix"
        );
        assert!(
            heap.memory.data(&heap.store)
                [pointer as usize + 8192..pointer as usize + size as usize]
                .iter()
                .all(|&b| b == 0)
        );
        heap.unit("reset", 0)?;
        heap.unit("push", pointer + size - 1)?;
        heap.collect()?;
        assert_eq!(heap.counter(2)?, u64::from(size));
        assert_eq!(heap.counter(8)?, 1);
        assert_eq!(heap.counter(10)?, 0);
        heap.unit("reset", 0)?;
        heap.collect()?;
        assert_eq!(heap.counter(2)?, 0);
    }
    Ok(())
}

#[test]
fn fragmentation_and_pins_use_copy_fallback() -> Result<()> {
    for pinned in [false, true] {
        let mut heap = Heap::new()?;
        let pointer = heap.alloc(8192)?;
        heap.memory
            .write(&mut heap.store, pointer as usize, b"kept")?;
        let token = if pinned {
            Some(
                heap.instance
                    .get_typed_func::<u32, u32>(&mut heap.store, "pin")?
                    .call(&mut heap.store, pointer + 3)?,
            )
        } else {
            let barrier = heap.alloc(32768)?;
            heap.unit("push", barrier)?;
            None
        };
        heap.unit("push", pointer)?;
        // The pinned case fits within existing layout slack, so only the pin forbids growth.
        let size = if pinned { 8704 } else { 131072 };
        let grown = heap.resize(pointer, 8192, size, 8192)?;
        assert_ne!(grown, pointer);
        assert_eq!(
            &heap.memory.data(&heap.store)[grown as usize..grown as usize + 4],
            b"kept"
        );
        assert_eq!(
            &heap.memory.data(&heap.store)[pointer as usize..pointer as usize + 4],
            b"kept"
        );
        heap.unit("reset", 0)?;
        heap.unit("push", grown)?;
        heap.collect()?;
        if let Some(token) = token {
            assert_eq!(heap.counter(2)?, u64::from(size + 8192));
            heap.unit("unpin", token)?;
        }
        heap.collect()?;
        assert_eq!(heap.counter(2)?, u64::from(size));
        assert_eq!(heap.counter(8)?, 0);
        assert_eq!(heap.counter(9)?, 1);
        assert_eq!(heap.counter(10)?, 8192);
    }
    Ok(())
}

#[test]
fn denied_growth_keeps_the_original_payload() -> Result<()> {
    let mut heap = Heap::new()?;
    let pointer = heap.alloc(8192)?;
    heap.unit("push", pointer)?;
    heap.memory
        .write(&mut heap.store, pointer as usize, b"original")?;
    assert!(heap.resize(pointer, 8192, 8 * 1024 * 1024, 8192).is_err());
    assert_eq!(
        &heap.memory.data(&heap.store)[pointer as usize..pointer as usize + 8],
        b"original"
    );
    Ok(())
}
