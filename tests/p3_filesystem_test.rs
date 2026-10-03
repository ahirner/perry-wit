use anyhow::Result;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

struct Host {
    context: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.context,
            table: &mut self.table,
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_file_forwarding_preserves_bytes_and_checks_both_completions() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(
        &engine,
        wat::parse_str(include_str!("fixtures/p3_file_copy.wat"))?,
    )?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/", FsPerms::ReadWrite)?
        .build();
    let mut store = Store::new(
        &engine,
        Host {
            context,
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(65_536).build(),
        },
    );
    store.limiter(|host| &mut host.limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(u32,), ((u32, u32, u32, u32),)>(&mut store, "run")?;
    let pages = instance.get_typed_func::<(), (u32,)>(&mut store, "memory-pages")?;
    for size in [0, 1, 8191, 8192, 8193, 4 * 1024 * 1024] {
        let input: Vec<u8> = (0..size)
            .map(|index| (index * 37 + index / 251) as u8)
            .collect();
        std::fs::write(directory.path().join("input.bin"), &input)?;
        for mode in [0, 1, 0, 2, 0] {
            let result = run.call_async(&mut store, (mode,)).await?.0;
            let expected = match mode {
                1 => (1, 13, 0, 0),
                2 => (0, 0, 1, 2),
                _ => (0, 0, 0, 0),
            };
            assert_eq!(result, expected, "size={size}, mode={mode}");
            assert_eq!(
                std::fs::read(directory.path().join("output.bin"))?,
                if mode == 1 { &[][..] } else { &input },
                "size={size}, mode={mode}"
            );
            assert_eq!(pages.call_async(&mut store, ()).await?.0, 1);
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    Ok(())
}
