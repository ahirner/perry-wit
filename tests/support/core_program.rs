//! Raw core modules (`compile_core`, no component) linked with `perry-runtime`, run in
//! wasmtime with fuel metering.
use wasmtime::{Config, Engine, Instance, Module, Store, Val};

/// Runtime memory counters read through `__perry_gc_counter` (`perry-runtime/src/roots.rs`).
/// Only modules compiled with memory checks export them.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Counter {
    Allocations = 0,
    Collections = 1,
    LiveBytes = 2,
    Pins = 5,
    Roots = 6,
    RootPublications = 7,
}

/// One instance of a core module that imports nothing: the runtime is linked in.
pub(crate) struct Program {
    pub(crate) store: Store<()>,
    pub(crate) instance: Instance,
    pub(crate) wasm: Vec<u8>,
}

impl Program {
    /// Compile `source` without memory checks.
    pub(crate) fn new(source: &str) -> Self {
        Self::from_wasm(perry_wit::compile_core(source, "case.ts").expect("source compiles"))
    }

    /// Compile `source` with memory checks, which export the collector controls.
    pub(crate) fn checked(source: &str, file: &str) -> Self {
        Self::from_wasm(
            perry_wit::compiler::compile_core_with_memory_checks(source, file)
                .expect("source compiles"),
        )
    }

    pub(crate) fn from_wasm(wasm: Vec<u8>) -> Self {
        if let Err(error) = wasmparser::Validator::new().validate_all(&wasm) {
            panic!("{error}\n{}", disassemble(&wasm));
        }
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).expect("fuel engine");
        let mut store = Store::new(&engine, ());
        store.set_fuel(u64::MAX).expect("fuel is enabled");
        let module = Module::new(&engine, &wasm).expect("valid module");
        assert_eq!(module.imports().len(), 0, "the runtime is linked in");
        let instance = Instance::new(&mut store, &module, &[]).expect("instantiation");
        Program {
            store,
            instance,
            wasm,
        }
    }

    /// Call an export; returns its results and the fuel the call consumed. A trap panics with
    /// the module's text.
    pub(crate) fn call(&mut self, name: &str, args: &[Val]) -> (Vec<Val>, u64) {
        let function = self
            .instance
            .get_func(&mut self.store, name)
            .unwrap_or_else(|| panic!("missing export {name}"));
        let mut results = vec![Val::I32(0); function.ty(&self.store).results().len()];
        let before = self.store.get_fuel().expect("fuel is enabled");
        if let Err(error) = function.call(&mut self.store, args, &mut results) {
            panic!("{name}: {error:?}\n{}", disassemble(&self.wasm));
        }
        (
            results,
            before - self.store.get_fuel().expect("fuel is enabled"),
        )
    }

    /// The number an export returns for one numeric argument.
    pub(crate) fn number(&mut self, name: &str, x: f64) -> f64 {
        self.call(name, &[Val::F64(x.to_bits())]).0[0].unwrap_f64()
    }

    /// `(value, status)` of an export that may throw, for one numeric argument.
    pub(crate) fn throwing(&mut self, name: &str, x: f64) -> (f64, i32) {
        self.outcome(name, &[Val::F64(x.to_bits())])
    }

    /// `(value, status)` of an export that may throw.
    pub(crate) fn outcome(&mut self, name: &str, args: &[Val]) -> (f64, i32) {
        let (results, _) = self.call(name, args);
        assert_eq!(results.len(), 2, "{name}: expected a status result");
        (results[0].unwrap_f64(), results[1].unwrap_i32())
    }

    /// The current size of the module's linear memory.
    pub(crate) fn memory_bytes(&mut self) -> usize {
        self.instance
            .get_memory(&mut self.store, "memory")
            .expect("modules export their memory")
            .data_size(&self.store)
    }

    /// Force collection at every safe point and poison reclaimed memory, or stop doing so.
    pub(crate) fn stress_collection(&mut self, enabled: bool) {
        self.call("__perry_gc_debug", &[Val::I32(if enabled { 3 } else { 0 })]);
    }

    pub(crate) fn counter(&mut self, counter: Counter) -> i64 {
        self.call("__perry_gc_counter", &[Val::I32(counter as i32)])
            .0[0]
            .unwrap_i64()
    }
}

fn disassemble(wasm: &[u8]) -> String {
    wasmprinter::print_bytes(wasm).unwrap_or_else(|error| format!("<unprintable: {error}>"))
}
