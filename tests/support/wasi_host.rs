//! The store state component tests run under: WASI P3 and `wasi:http` contexts, one resource
//! table, swappable HTTP hooks and a limiter that records peak linear memory.
use anyhow::Result;
use wasmtime::{
    Engine, ResourceLimiter, Store,
    component::{Component, Instance, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

/// `wasi:http`'s own behavior: every hook keeps its provided implementation.
pub(crate) struct DefaultHooks;
impl WasiHttpHooks for DefaultHooks {}

/// Store data for one component instance. `hooks` lets a test intercept outgoing requests and
/// keep typed access to its fixture through `store.data()`.
pub(crate) struct Host<H = DefaultHooks> {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    pub(crate) table: ResourceTable,
    pub(crate) hooks: H,
    /// The largest linear memory size an instance asked to grow to.
    pub(crate) peak: usize,
    /// Growth beyond this many bytes is refused.
    memory_limit: usize,
}

impl Host {
    pub(crate) fn new(wasi: WasiCtx) -> Self {
        Self {
            wasi,
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            hooks: DefaultHooks,
            peak: 0,
            memory_limit: usize::MAX,
        }
    }
}

impl Default for Host {
    fn default() -> Self {
        Self::new(WasiCtx::builder().build())
    }
}

impl<H> Host<H> {
    /// The same host with `hooks` serving outgoing HTTP requests.
    pub(crate) fn with_hooks<N>(self, hooks: N) -> Host<N> {
        Host {
            wasi: self.wasi,
            http: self.http,
            table: self.table,
            hooks,
            peak: self.peak,
            memory_limit: self.memory_limit,
        }
    }

    /// Refuse linear-memory growth beyond `bytes`.
    pub(crate) fn with_memory_limit(mut self, bytes: usize) -> Self {
        self.memory_limit = bytes;
        self
    }
}

impl<H: Send + 'static> WasiView for Host<H> {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl<H: WasiHttpHooks + Send + 'static> WasiHttpView for Host<H> {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

impl<H: Send> ResourceLimiter for Host<H> {
    fn memory_growing(
        &mut self,
        _: usize,
        desired: usize,
        _: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.peak = self.peak.max(desired);
        Ok(desired <= self.memory_limit)
    }

    fn table_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

/// A linker with WASI P3 and `wasi:http` P3 for [Host].
pub(crate) fn linker<H: WasiHttpHooks + Send + 'static>(
    engine: &Engine,
) -> Result<Linker<Host<H>>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    Ok(linker)
}

/// A store for `host` whose limiter records peak memory and enforces the host's limit.
pub(crate) fn store<H: Send + 'static>(engine: &Engine, host: Host<H>) -> Store<Host<H>> {
    let mut store = Store::new(engine, host);
    store.limiter(|host| host);
    store
}

/// Instantiate `component` with [linker] in a fresh [store] for `host`.
pub(crate) async fn instantiate<H: WasiHttpHooks + Send + 'static>(
    engine: &Engine,
    component: &[u8],
    host: Host<H>,
) -> Result<(Store<Host<H>>, Instance)> {
    let component = Component::new(engine, component)?;
    let mut store = store(engine, host);
    let instance = linker(engine)?
        .instantiate_async(&mut store, &component)
        .await?;
    Ok((store, instance))
}
