//! The engine async components run on: component-model async on the callback ABI, without
//! stackful tasks, the additional async builtins or threads.
use wasmtime::{Config, Engine};

pub(crate) fn async_engine() -> anyhow::Result<Engine> {
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_async_stackful(false)
        .wasm_component_model_more_async_builtins(false)
        .wasm_component_model_threading(false);
    Ok(Engine::new(&config)?)
}
