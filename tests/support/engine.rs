//! The engine async components run on: component-model async on the callback ABI, without
//! stackful tasks, the additional async builtins or threads.
use std::sync::OnceLock;
use wasmtime::{Config, Engine};

pub(crate) fn async_engine() -> anyhow::Result<Engine> {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    Ok(ENGINE
        .get_or_init(|| {
            let mut config = Config::new();
            config
                .wasm_component_model_async(true)
                .wasm_component_model_async_stackful(false)
                .wasm_component_model_more_async_builtins(false)
                .wasm_component_model_threading(false);
            Engine::new(&config).expect("async component engine")
        })
        .clone())
}
