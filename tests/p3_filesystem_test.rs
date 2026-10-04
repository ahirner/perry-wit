use anyhow::Result;
use perry_wit::{compile_typescript_waffle, waffle_backend::WaffleCompileOptions};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

#[path = "p3_filesystem/source.rs"]
mod source;

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
        wat::parse_str(format!(
            "(component {} {})",
            include_str!("fixtures/p3_filesystem_imports.wat"),
            include_str!("fixtures/p3_file_copy.wat")
        ))?,
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

#[tokio::test(flavor = "current_thread")]
async fn compiled_source_scans_a_real_p3_file_across_component_boundaries() -> Result<()> {
    let source = r#"
    declare function readChunk(input: ByteStream): Promise<number>;
    declare function byteAt(index: number): number;
    export async function run(input: ByteStream): Promise<number> {
        let total = 0;
        let count = await readChunk(input);
        while (count > 0) {
            let index = 0;
            while (index < count) {
                total = total + byteAt(index);
                index = index + 1;
            }
            count = await readChunk(input);
        }
        return total;
    }"#;
    check_source_scan(source).await
}

#[tokio::test(flavor = "current_thread")]
async fn compiled_source_reads_a_real_p3_file_into_managed_views() -> Result<()> {
    let source = r#"
    declare function readInto(input: ByteStream, destination: Uint8Array): Promise<number>;
    export async function run(input: ByteStream): Promise<number> {
        const bytes = new Uint8Array(4099);
        bytes[0] = 91;
        bytes[4098] = 17;
        const view = bytes.subarray(2, 4098);
        let total = 0;
        let count = await readInto(input, view);
        while (count > 0) {
            let index = 0;
            while (index < count) {
                total = total + view[index];
                index = index + 1;
            }
            count = await readInto(input, view);
        }
        if (bytes[0] !== 91) { return -1; }
        if (bytes[4098] !== 17) { return -1; }
        return total;
    }"#;
    check_source_scan(source).await
}

async fn check_source_scan(source: &str) -> Result<()> {
    let compiled =
        compile_typescript_waffle(source, "file-scan.ts", &WaffleCompileOptions::default())?;
    let consumer = compiled.component_wat.unwrap();
    let consumer = consumer
        .strip_prefix("(component")
        .unwrap()
        .trim_end()
        .strip_suffix(')')
        .unwrap();
    let wat = format!(
        "(component {} (component $consumer {consumer})
        (instance $consumer (instantiate $consumer)) {})",
        include_str!("fixtures/p3_filesystem_imports.wat"),
        include_str!("fixtures/p3_file_scan.wat")
    );
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, wat::parse_str(wat)?)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let directory = tempfile::tempdir()?;
    let mut store = Store::new(
        &engine,
        Host {
            context: WasiCtxBuilder::new()
                .preopened_dir(directory.path(), "/", FsPerms::ReadWrite)?
                .build(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(65_536).build(),
        },
    );
    store.limiter(|host| &mut host.limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(bool,), ((f64, u32, u32),)>(&mut store, "run")?;
    for size in [0, 1, 8191, 8192, 8193, 4 * 1024 * 1024] {
        let bytes: Vec<u8> = (0..size)
            .map(|index| (index * 37 + index / 251) as u8)
            .collect();
        let sum = bytes.iter().map(|&byte| f64::from(byte)).sum::<f64>();
        std::fs::write(directory.path().join("input.bin"), bytes)?;
        for directory_input in [false, true, false] {
            let expected = if directory_input {
                (0.0, 1, 13)
            } else {
                (sum, 0, 0)
            };
            assert_eq!(
                run.call_async(&mut store, (directory_input,)).await?.0,
                expected
            );
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    Ok(())
}
