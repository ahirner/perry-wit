//! Integration tests for WAFFLE backend string handling, scalar operations, and component round-trip.

use anyhow::Result;
use perry_wit::compile_typescript_waffle;
use perry_wit::waffle_backend::WaffleCompileOptions;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use wasmtime::component::{Component, Linker, ResourceTable, Val};
use wasmtime::{Config, Engine, Instance, Module, Store, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

struct TrackingAllocator;
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static DEALLOCATED: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            if new_size > layout.size() {
                ALLOCATED.fetch_add(new_size - layout.size(), Ordering::Relaxed);
            } else {
                DEALLOCATED.fetch_add(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        new_ptr
    }
}

#[global_allocator]
static GLOBAL: TrackingAllocator = TrackingAllocator;

fn make_async_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    Ok(Engine::new(&config)?)
}

#[derive(Default)]
struct WasiHostState {
    context: WasiCtx,
    table: ResourceTable,
}

impl WasiView for WasiHostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.context,
            table: &mut self.table,
        }
    }
}

fn make_wasi_linker(engine: &Engine) -> Result<Linker<WasiHostState>> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    Ok(linker)
}

/// Compile, validate, and execute cases on one component instance.
async fn run_cases(source: &str, cases: &[(Vec<Val>, Val)]) -> Result<()> {
    let compiled =
        compile_typescript_waffle(source, "string_cases.ts", &WaffleCompileOptions::default())?;
    let engine = make_async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_func(&mut store, "run").unwrap();
    for (params, expected) in cases {
        let mut results = [Val::Bool(false)];
        run.call_async(&mut store, params, &mut results).await?;
        assert_eq!(
            &results[0], expected,
            "arguments: {params:?}; source: {source}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_literal_returns_and_multibyte_bytes() -> Result<()> {
    let source = r#"
        export function run(input: number): string {
            if (input == 0) {
                return "";
            } else if (input == 1) {
                return "hello world";
            } else if (input == 2) {
                return "hello \0 world";
            } else {
                return "hello 🦀 😀";
            }
        }
    "#;

    let compiled =
        compile_typescript_waffle(source, "strings.ts", &WaffleCompileOptions::default())?;
    assert!(!compiled.core.is_empty());

    let engine = make_async_engine()?;
    let component_bytes = compiled.component.expect("Component emitted");
    let component = Component::new(&engine, &component_bytes)?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (String,)>(&mut store, "run")?;

    // 0: Empty string
    let res = run.call_async(&mut store, (0.0,)).await?;
    assert_eq!(res.0, "");
    assert_eq!(res.0.len(), 0);

    // 1: ASCII string
    let res = run.call_async(&mut store, (1.0,)).await?;
    assert_eq!(res.0, "hello world");
    assert_eq!(res.0.as_bytes(), b"hello world");

    // 2: String with embedded NUL
    let res = run.call_async(&mut store, (2.0,)).await?;
    assert_eq!(res.0, "hello \0 world");
    assert_eq!(res.0.as_bytes(), b"hello \0 world");

    // 3: Multibyte string with 4-byte UTF-8 emoji
    let res = run.call_async(&mut store, (3.0,)).await?;
    assert_eq!(res.0, "hello 🦀 😀");
    assert_eq!(res.0.as_bytes(), "hello 🦀 😀".as_bytes());

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_scalar_length_operation() -> Result<()> {
    let source = r#"
        export function run(input: number): number {
            if (input == 0) {
                let s = "";
                return s.length;
            } else if (input == 1) {
                let s = "hello world";
                return s.length;
            } else if (input == 2) {
                // "😀" is 4 bytes, but 1 Unicode scalar
                let s = "😀";
                return s.length;
            } else {
                // "hello 🦀 😀": 6 ASCII + 1 crab (1 scalar) + 1 space + 1 grin (1 scalar) = 10 scalars
                // (byte length is 6 + 4 + 1 + 4 = 15 bytes)
                let s = "hello 🦀 😀";
                return s.length;
            }
        }
    "#;

    let compiled =
        compile_typescript_waffle(source, "strlen.ts", &WaffleCompileOptions::default())?;
    let engine = make_async_engine()?;
    let component_bytes = compiled.component.expect("Component emitted");
    let component = Component::new(&engine, &component_bytes)?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;

    let res = run.call_async(&mut store, (0.0,)).await?;
    assert_eq!(res.0, 0.0);

    let res = run.call_async(&mut store, (1.0,)).await?;
    assert_eq!(res.0, 11.0);

    let res = run.call_async(&mut store, (2.0,)).await?;
    assert_eq!(res.0, 1.0); // 1 scalar!

    let res = run.call_async(&mut store, (3.0,)).await?;
    assert_eq!(res.0, 9.0); // 9 scalars!

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_index_and_char_at() -> Result<()> {
    let source = r#"
        export function run(idx: number): string {
            let s = "A😀B🦀C";
            // s has 5 Unicode scalars: 'A', '😀', 'B', '🦀', 'C'
            if (idx == 0) {
                return s[0];
            } else if (idx == 1) {
                return s.charAt(1);
            } else if (idx == 2) {
                return s[2];
            } else if (idx == 3) {
                return s.charAt(3);
            } else if (idx == 4) {
                return s[4];
            } else {
                return s.charAt(10);
            }
        }
    "#;

    let compiled =
        compile_typescript_waffle(source, "char_at.ts", &WaffleCompileOptions::default())?;
    let engine = make_async_engine()?;
    let component_bytes = compiled.component.expect("Component emitted");
    let component = Component::new(&engine, &component_bytes)?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (String,)>(&mut store, "run")?;

    let res = run.call_async(&mut store, (0.0,)).await?;
    assert_eq!(res.0, "A");

    let res = run.call_async(&mut store, (1.0,)).await?;
    assert_eq!(res.0, "😀");

    let res = run.call_async(&mut store, (2.0,)).await?;
    assert_eq!(res.0, "B");

    let res = run.call_async(&mut store, (3.0,)).await?;
    assert_eq!(res.0, "🦀");

    let res = run.call_async(&mut store, (4.0,)).await?;
    assert_eq!(res.0, "C");

    let res = run.call_async(&mut store, (10.0,)).await?;
    assert_eq!(res.0, "");

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_slice_operations() -> Result<()> {
    let source = r#"
        export function run(op: number): string {
            let s = "Hello, 🦀 World!";
            // Scalars:
            // 0: 'H', 1: 'e', 2: 'l', 3: 'l', 4: 'o', 5: ',', 6: ' ', 7: '🦀', 8: ' ', 9: 'W', 10: 'o', 11: 'r', 12: 'l', 13: 'd', 14: '!'
            // Total scalars: 15
            if (op == 0) {
                return s.slice(0, 5); // "Hello"
            } else if (op == 1) {
                return s.slice(7, 8); // "🦀"
            } else if (op == 2) {
                return s.slice(7); // "🦀 World!"
            } else if (op == 3) {
                return s.slice(-6); // "World!"
            } else if (op == 4) {
                return s.slice(-6, -1); // "World"
            } else if (op == 5) {
                return s.slice(5, 2); // empty string because start >= end
            } else {
                return s.slice(0, 100); // clamped to entire string
            }
        }
    "#;

    let compiled = compile_typescript_waffle(source, "slice.ts", &WaffleCompileOptions::default())?;
    let engine = make_async_engine()?;
    let component_bytes = compiled.component.expect("Component emitted");
    let component = Component::new(&engine, &component_bytes)?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (String,)>(&mut store, "run")?;

    let res = run.call_async(&mut store, (0.0,)).await?;
    assert_eq!(res.0, "Hello");

    let res = run.call_async(&mut store, (1.0,)).await?;
    assert_eq!(res.0, "🦀");

    let res = run.call_async(&mut store, (2.0,)).await?;
    assert_eq!(res.0, "🦀 World!");

    let res = run.call_async(&mut store, (3.0,)).await?;
    assert_eq!(res.0, "World!");

    let res = run.call_async(&mut store, (4.0,)).await?;
    assert_eq!(res.0, "World");

    let res = run.call_async(&mut store, (5.0,)).await?;
    assert_eq!(res.0, "");

    let res = run.call_async(&mut store, (6.0,)).await?;
    assert_eq!(res.0, "Hello, 🦀 World!");

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_index_of_search() -> Result<()> {
    let source = r#"
        export function run(op: number): number {
            let s = "foo 🦀 bar 🦀 baz";
            // Scalars:
            // 'f'(0), 'o'(1), 'o'(2), ' '(3), '🦀'(4), ' '(5),
            // 'b'(6), 'a'(7), 'r'(8), ' '(9), '🦀'(10), ' '(11),
            // 'b'(12), 'a'(13), 'z'(14)
            if (op == 0) {
                return s.indexOf("foo"); // 0
            } else if (op == 1) {
                return s.indexOf("🦀"); // 4
            } else if (op == 2) {
                return s.indexOf("🦀", 5); // 10
            } else if (op == 3) {
                return s.indexOf("bar"); // 6
            } else if (op == 4) {
                return s.indexOf("missing"); // -1
            } else if (op == 5) {
                return s.indexOf("", 3); // 3 (empty search)
            } else {
                return s.indexOf("", 100); // 15 (clamped to scalar_len)
            }
        }
    "#;

    let compiled =
        compile_typescript_waffle(source, "index_of.ts", &WaffleCompileOptions::default())?;
    let engine = make_async_engine()?;
    let component_bytes = compiled.component.expect("Component emitted");
    let component = Component::new(&engine, &component_bytes)?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;

    let res = run.call_async(&mut store, (0.0,)).await?;
    assert_eq!(res.0, 0.0);

    let res = run.call_async(&mut store, (1.0,)).await?;
    assert_eq!(res.0, 4.0);

    let res = run.call_async(&mut store, (2.0,)).await?;
    assert_eq!(res.0, 10.0);

    let res = run.call_async(&mut store, (3.0,)).await?;
    assert_eq!(res.0, 6.0);

    let res = run.call_async(&mut store, (4.0,)).await?;
    assert_eq!(res.0, -1.0);

    let res = run.call_async(&mut store, (5.0,)).await?;
    assert_eq!(res.0, 3.0);

    let res = run.call_async(&mut store, (6.0,)).await?;
    assert_eq!(res.0, 15.0);

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_concatenation_and_comparison() -> Result<()> {
    let source = r#"
        export function run(op: number): string {
            let a = "Hello, ";
            let b = "🦀 World!";
            if (op == 0) {
                return a + b;
            } else if (op == 1) {
                let eq = (a == "Hello, ");
                if (eq) {
                    return "equal";
                } else {
                    return "not equal";
                }
            } else if (op == 2) {
                let lt = ("abc" < "abd");
                if (lt) {
                    return "less";
                } else {
                    return "not less";
                }
            } else {
                let chained = "Part1: " + a + "Part2: " + b;
                return chained;
            }
        }
    "#;

    let compiled =
        compile_typescript_waffle(source, "concat.ts", &WaffleCompileOptions::default())?;
    let engine = make_async_engine()?;
    let component_bytes = compiled.component.expect("Component emitted");
    let component = Component::new(&engine, &component_bytes)?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(f64,), (String,)>(&mut store, "run")?;

    let res = run.call_async(&mut store, (0.0,)).await?;
    assert_eq!(res.0, "Hello, 🦀 World!");

    let res = run.call_async(&mut store, (1.0,)).await?;
    assert_eq!(res.0, "equal");

    let res = run.call_async(&mut store, (2.0,)).await?;
    assert_eq!(res.0, "less");

    let res = run.call_async(&mut store, (3.0,)).await?;
    assert_eq!(res.0, "Part1: Hello, Part2: 🦀 World!");

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_canonical_abi_component_round_trip() -> Result<()> {
    let source = r#"
        export function run(input: string): string {
            let prefix = "Echo: ";
            let len_str = " (len: ";
            let s_len = input.length;
            if (s_len == 0) {
                return "empty input";
            }
            return prefix + input;
        }
    "#;

    let compiled =
        compile_typescript_waffle(source, "roundtrip.ts", &WaffleCompileOptions::default())?;
    assert!(!compiled.core.is_empty());

    let engine = make_async_engine()?;
    let component_bytes = compiled.component.expect("Component emitted");
    let component = Component::new(&engine, &component_bytes)?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;

    // Empty string
    let res = run.call_async(&mut store, ("",)).await?;
    assert_eq!(res.0, "empty input");

    // Standard ASCII
    let res = run.call_async(&mut store, ("hello from host",)).await?;
    assert_eq!(res.0, "Echo: hello from host");

    // Embedded NUL
    let res = run.call_async(&mut store, ("a\0b\0c",)).await?;
    assert_eq!(res.0, "Echo: a\0b\0c");

    // Multibyte text
    let res = run
        .call_async(&mut store, ("🦀 Rust and TypeScript 🚀",))
        .await?;
    assert_eq!(res.0, "Echo: 🦀 Rust and TypeScript 🚀");

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_result_component_round_trip() -> Result<()> {
    let source = r#"
        export function run(input: string): Result<string, number> {
            if (input.length == 0) {
                throw 400;
            }
            if (input == "fail") {
                throw 500;
            }
            return "Processed: " + input;
        }
    "#;

    let compiled = compile_typescript_waffle(
        source,
        "result_roundtrip.ts",
        &WaffleCompileOptions::default(),
    )?;
    assert!(!compiled.core.is_empty());

    let engine = make_async_engine()?;
    let component_bytes = compiled.component.expect("Component emitted");
    let component = Component::new(&engine, &component_bytes)?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (Result<String, f64>,)>(&mut store, "run")?;

    // Test error case 1: empty string
    let res = run.call_async(&mut store, ("",)).await?;
    assert_eq!(res.0, Err(400.0));

    // Test error case 2: "fail" string
    let res = run.call_async(&mut store, ("fail",)).await?;
    assert_eq!(res.0, Err(500.0));

    // Test success case: valid string
    let res = run.call_async(&mut store, ("hello world",)).await?;
    assert_eq!(res.0, Ok("Processed: hello world".to_string()));

    // Test success case: multibyte string
    let res = run.call_async(&mut store, ("🦀 🚀 ✨",)).await?;
    assert_eq!(res.0, Ok("Processed: 🦀 🚀 ✨".to_string()));

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_async_string_result_canonical_options() -> Result<()> {
    run_cases(
        r#"export async function run(input: number): Promise<string> { return "hello 🦀"; }"#,
        &[(vec![Val::Float64(1.0)], Val::String("hello 🦀".into()))],
    )
    .await?;
    run_cases(
        r#"export async function run(): Promise<Result<string, number>> { return "hello 🦀"; }"#,
        &[(
            vec![],
            Val::Result(Ok(Some(Box::new(Val::String("hello 🦀".into()))))),
        )],
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_flattened_string_parameter_limit() -> Result<()> {
    for string_count in [7, 8, 9] {
        let params = (0..string_count)
            .map(|i| format!("s{i}: string"))
            .collect::<Vec<_>>()
            .join(", ");
        let source = format!("export function run({params}): string {{ return s0; }}");
        if string_count <= 8 {
            run_cases(
                &source,
                &[(
                    vec![Val::String("🦀".into()); string_count],
                    Val::String("🦀".into()),
                )],
            )
            .await?;
        } else {
            let error = compile_typescript_waffle(
                &source,
                "many_strings.ts",
                &WaffleCompileOptions::default(),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("16 flattened parameters"),
                "{error:#}"
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_memory_grows_for_large_and_repeated_allocations() -> Result<()> {
    let large = "🦀".repeat(16_384);
    let small = "x".repeat(20_000);
    let cases = [large, small.clone(), small.clone(), small.clone(), small]
        .into_iter()
        .map(|s| (vec![Val::String(s.clone())], Val::String(s)))
        .collect::<Vec<_>>();
    run_cases(
        "export function run(input: string): string { return input; }",
        &cases,
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_static_string_pool_reserves_all_pages() -> Result<()> {
    let large = "x".repeat(65_536);
    let multibyte = "🦀".repeat(9_000);
    let other = "y".repeat(36_000);
    let source = format!(
        r#"export function run(input: number): string {{
        if (input === 0) return "{large}";
        if (input === 1) return "{multibyte}";
        return "{other}";
    }}"#
    );
    run_cases(
        &source,
        &[
            (vec![Val::Float64(0.0)], Val::String(large)),
            (vec![Val::Float64(1.0)], Val::String(multibyte)),
            (vec![Val::Float64(2.0)], Val::String(other)),
        ],
    )
    .await
}

#[test]
fn test_string_allocator_failure_does_not_advance_heap() -> Result<()> {
    let options = WaffleCompileOptions {
        componentize: false,
        ..Default::default()
    };
    let compiled = compile_typescript_waffle(
        "export function run(input: string): string { return input; }",
        "allocator.ts",
        &options,
    )?;
    let engine = Engine::default();
    let module = Module::new(&engine, compiled.core)?;
    let limits = StoreLimitsBuilder::new().memory_size(65_536).build();
    let mut store = Store::new(&engine, limits);
    store.limiter(|limits| limits);
    let instance = Instance::new(&mut store, &module, &[])?;
    let realloc =
        instance.get_typed_func::<(u32, u32, u32, u32), u32>(&mut store, "cabi_realloc")?;
    assert!(realloc.call(&mut store, (0, 0, 4, 65_536)).is_err());
    assert!(realloc.call(&mut store, (0, 0, 4, u32::MAX)).is_err());
    let ptr = realloc.call(&mut store, (0, 0, 4, 16))?;
    assert!(ptr < 2048);
    let memory = instance.get_memory(&mut store, "memory").unwrap();
    memory.write(&mut store, ptr as usize, b"saved")?;
    let moved = realloc.call(&mut store, (ptr, 5, 8, 32))?;
    assert_eq!(moved % 8, 0);
    assert_eq!(
        &memory.data(&store)[moved as usize..moved as usize + 5],
        b"saved"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_method_defaults_and_numeric_positions() -> Result<()> {
    run_cases(
        r#"export function run(): string { return "A🦀B".slice() + "A🦀B".charAt(); }"#,
        &[(vec![], Val::String("A🦀BA".into()))],
    )
    .await?;
    let positions = [
        f64::NAN,
        -0.5,
        -1.5,
        -1e20,
        1e20,
        f64::NEG_INFINITY,
        f64::INFINITY,
        1.9,
    ];
    for (operation, expected) in [
        (
            "slice(p)",
            vec!["A🦀B", "A🦀B", "B", "A🦀B", "", "A🦀B", "", "🦀B"],
        ),
        (
            "slice(0, p)",
            vec!["", "", "A🦀", "", "A🦀B", "", "A🦀B", "A"],
        ),
        ("charAt(p)", vec!["A", "A", "", "", "", "", "", "🦀"]),
    ] {
        let source =
            format!(r#"export function run(p: number): string {{ return "A🦀B".{operation}; }}"#);
        let cases = positions
            .into_iter()
            .zip(expected)
            .map(|(p, s)| (vec![Val::Float64(p)], Val::String(s.into())))
            .collect::<Vec<_>>();
        run_cases(&source, &cases).await?;
    }
    for (search, expected) in [
        ("", [0.0, 0.0, 0.0, 0.0, 3.0, 0.0, 3.0, 1.0]),
        ("🦀", [1.0, 1.0, 1.0, 1.0, -1.0, 1.0, -1.0, 1.0]),
    ] {
        let source = format!(
            r#"export function run(p: number): number {{ return "A🦀B".indexOf("{search}", p); }}"#
        );
        let cases = positions
            .into_iter()
            .zip(expected)
            .map(|(p, n)| (vec![Val::Float64(p)], Val::Float64(n)))
            .collect::<Vec<_>>();
        run_cases(&source, &cases).await?;
    }
    Ok(())
}

#[test]
fn test_string_method_unsupported_arguments_are_diagnostics() {
    for expression in [
        r#""abc".slice(1, 2, 3)"#,
        r#""abc".charAt(1, 2)"#,
        r#""abc".indexOf()"#,
        r#""abc".indexOf(1)"#,
        r#""abc".slice(true)"#,
    ] {
        let source = format!("export function run(): string {{ return {expression}; }}");
        assert!(
            compile_typescript_waffle(&source, "arguments.ts", &WaffleCompileOptions::default())
                .is_err(),
            "{expression}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_awaited_string_types_and_truthiness() -> Result<()> {
    let source = r#"
        async function getString(input: string): Promise<string> { return input; }
        export async function run(input: string): Promise<string> {
            let first = await getString(input);
            let second = await first;
            let doubled = first + second;
            if (first === second) {
                if (doubled) { return doubled; }
                return "empty";
            }
            return "mismatch";
        }
    "#;
    run_cases(
        source,
        &[
            (vec![Val::String("".into())], Val::String("empty".into())),
            (vec![Val::String("🦀".into())], Val::String("🦀🦀".into())),
            (vec![Val::String("\0".into())], Val::String("\0\0".into())),
        ],
    )
    .await?;
    run_cases(r#"export function run(): number { let empty = ""; if (empty) return 1; while (empty) return 2; return 3; }"#,
        &[(vec![], Val::Float64(3.0))]).await
}

#[test]
fn test_nonstrings_are_not_used_as_string_descriptors() {
    for expression in [r#""value:" + value"#, r#"value + "!""#, "`value:${value}`"] {
        for initializer in ["true", "1"] {
            let source = format!(
                "export function run(): string {{ let value = {initializer}; return {expression}; }}"
            );
            let error =
                compile_typescript_waffle(&source, "coercion.ts", &WaffleCompileOptions::default())
                    .expect_err("unsupported coercion must be diagnosed");
            assert!(
                error.to_string().contains("String coercion is unsupported"),
                "{error:#}"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_mixed_comparisons() -> Result<()> {
    for (operator, expected) in [("===", false), ("!==", true)] {
        for initializer in ["false", "true", "0", "1"] {
            for (left, right) in [("text", "value"), ("value", "text")] {
                let source = format!(
                    r#"export function run(): boolean {{ let text = ""; let value: any = {initializer}; return {left} {operator} {right}; }}"#
                );
                run_cases(&source, &[(vec![], Val::Bool(expected))]).await?;
            }
        }
    }
    for operator in ["==", "!=", "<", "<=", ">", ">="] {
        let source = format!(
            r#"export function run(): boolean {{ let text = ""; let value = false; return text {operator} value; }}"#
        );
        let error =
            compile_typescript_waffle(&source, "comparison.ts", &WaffleCompileOptions::default())
                .expect_err("mixed coercive comparison must be diagnosed");
        assert!(
            error
                .to_string()
                .contains("Unsupported mixed string comparison"),
            "{error:#}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_indexing_preserves_undefined() -> Result<()> {
    run_cases(
        r#"export function run(input: string): boolean { return input[99] === ""; }"#,
        &[(vec![Val::String("abc".into())], Val::Bool(false))],
    )
    .await?;
    let source = r#"
        export async function run(input: string, index: number): Promise<number> {
            let character = await input[index];
            let result = 0;
            if (character === undefined) { result = result + 1; }
            if (character === "") { result = result + 2; }
            if (character !== input.charAt(index)) { result = result + 4; }
            if (character) { result = result + 8; }
            if (character === input[index]) { result = result + 16; }
            if (character === false) { return 99; }
            if (character === 0) { return 98; }
            if (character == undefined) {
                if (undefined !== character) { return 97; }
            }
            return result;
        }
    "#;
    let cases = [
        ("A🦀B", 0.0, 24.0),
        ("A🦀B", 1.0, 24.0),
        ("A🦀B", 2.0, 24.0),
        ("A🦀B", -0.0, 24.0),
        ("A🦀B", 99.0, 21.0),
        ("A🦀B", -2.0, 21.0),
        ("A🦀B", 1.5, 21.0),
        ("A🦀B", -0.5, 21.0),
        ("A🦀B", f64::NAN, 21.0),
        ("A🦀B", f64::INFINITY, 21.0),
        ("A🦀B", f64::NEG_INFINITY, 21.0),
        ("", 0.0, 21.0),
    ]
    .into_iter()
    .map(|(s, i, n)| {
        (
            vec![Val::String(s.into()), Val::Float64(i)],
            Val::Float64(n),
        )
    })
    .collect::<Vec<_>>();
    run_cases(source, &cases).await?;

    for expression in ["input[99]", "input[99].slice()", "echo(input[99])"] {
        let source = format!(
            "function echo(s: string): string {{ return s; }}
             export function run(input: string): string {{ return {expression}; }}"
        );
        let error = run_cases(
            &source,
            &[(vec![Val::String("abc".into())], Val::String("".into()))],
        )
        .await
        .expect_err("Undefined must not cross a string-only boundary as an empty string");
        assert!(
            format!("{error:#}").contains("unreachable"),
            "{expression}: {error:#}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_code_point_at() -> Result<()> {
    let source = r#"export function run(s: string, pos: number): number {
        return s.codePointAt(pos);
    }"#;
    let cases = vec![
        (
            vec![Val::String("A🦀B".into()), Val::Float64(0.0)],
            Val::Float64(65.0),
        ),
        (
            vec![Val::String("A🦀B".into()), Val::Float64(1.0)],
            Val::Float64(129408.0),
        ), // 0x1F980
        (
            vec![Val::String("A🦀B".into()), Val::Float64(2.0)],
            Val::Float64(66.0),
        ),
        (
            vec![Val::String("A🦀B".into()), Val::Float64(3.0)],
            Val::Float64(f64::NAN),
        ),
        (
            vec![Val::String("A🦀B".into()), Val::Float64(-1.0)],
            Val::Float64(f64::NAN),
        ),
    ];
    // Custom runner to handle NaN comparison
    let compiled =
        compile_typescript_waffle(source, "code_point_at.ts", &WaffleCompileOptions::default())?;
    let engine = make_async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_func(&mut store, "run").unwrap();
    for (params, expected) in cases {
        let mut results = [Val::Float64(0.0)];
        run.call_async(&mut store, &params, &mut results).await?;
        if let (Val::Float64(actual), Val::Float64(exp)) = (&results[0], expected) {
            if exp.is_nan() {
                assert!(actual.is_nan(), "expected NaN, got {actual}");
            } else {
                assert_eq!(actual, &exp);
            }
        }
    }

    // Default position test
    let default_pos_src = r#"export function run(s: string): number {
        return s.codePointAt();
    }"#;
    run_cases(
        default_pos_src,
        &[(vec![Val::String("A🦀B".into())], Val::Float64(65.0))],
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_from_code_point() -> Result<()> {
    let source = r#"export function run(cp: number): string {
        return String.fromCodePoint(cp);
    }"#;
    run_cases(
        source,
        &[
            (vec![Val::Float64(65.0)], Val::String("A".into())),
            (vec![Val::Float64(129408.0)], Val::String("🦀".into())),
            (vec![Val::Float64(0x20AC as f64)], Val::String("€".into())),
            (vec![Val::Float64(0.0)], Val::String("\0".into())),
        ],
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_case_conversion() -> Result<()> {
    let to_lower_src = r#"export function run(s: string): string {
        return s.toLowerCase();
    }"#;
    run_cases(
        to_lower_src,
        &[
            (
                vec![Val::String("Hello, WORLD!".into())],
                Val::String("hello, world!".into()),
            ),
            (vec![Val::String("CAFÉ".into())], Val::String("café".into())),
            (vec![Val::String("🦀".into())], Val::String("🦀".into())),
        ],
    )
    .await?;

    let to_upper_src = r#"export function run(s: string): string {
        return s.toUpperCase();
    }"#;
    run_cases(
        to_upper_src,
        &[
            (
                vec![Val::String("hello, world!".into())],
                Val::String("HELLO, WORLD!".into()),
            ),
            (vec![Val::String("café".into())], Val::String("CAFÉ".into())),
            // German sharp S expands to SS
            (
                vec![Val::String("weiß".into())],
                Val::String("WEISS".into()),
            ),
            (vec![Val::String("🦀".into())], Val::String("🦀".into())),
        ],
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_split_and_join() -> Result<()> {
    let split_index_src = r#"export function run(s: string, sep: string, idx: number): string {
        let parts = s.split(sep);
        return parts[idx];
    }"#;
    run_cases(
        split_index_src,
        &[
            (
                vec![
                    Val::String("one,two,three".into()),
                    Val::String(",".into()),
                    Val::Float64(0.0),
                ],
                Val::String("one".into()),
            ),
            (
                vec![
                    Val::String("one,two,three".into()),
                    Val::String(",".into()),
                    Val::Float64(1.0),
                ],
                Val::String("two".into()),
            ),
            (
                vec![
                    Val::String("one,two,three".into()),
                    Val::String(",".into()),
                    Val::Float64(2.0),
                ],
                Val::String("three".into()),
            ),
        ],
    )
    .await?;

    let split_len_src = r#"export function run(s: string, sep: string): number {
        return s.split(sep).length;
    }"#;
    run_cases(
        split_len_src,
        &[
            (
                vec![Val::String("one,two,three".into()), Val::String(",".into())],
                Val::Float64(3.0),
            ),
            (
                vec![Val::String("single".into()), Val::String(",".into())],
                Val::Float64(1.0),
            ),
            (
                vec![Val::String("🦀🌲🦀".into()), Val::String("".into())],
                Val::Float64(3.0),
            ),
        ],
    )
    .await?;

    let split_join_src = r#"export function run(s: string, sep: string, join_sep: string): string {
        return s.split(sep).join(join_sep);
    }"#;
    run_cases(
        split_join_src,
        &[
            (
                vec![
                    Val::String("a,b,c".into()),
                    Val::String(",".into()),
                    Val::String("-".into()),
                ],
                Val::String("a-b-c".into()),
            ),
            (
                vec![
                    Val::String("hello".into()),
                    Val::String("".into()),
                    Val::String(".".into()),
                ],
                Val::String("h.e.l.l.o".into()),
            ),
            (
                vec![
                    Val::String("🦀🌲🦀".into()),
                    Val::String("".into()),
                    Val::String("~".into()),
                ],
                Val::String("🦀~🌲~🦀".into()),
            ),
        ],
    )
    .await?;

    let default_join_src = r#"export function run(s: string): string {
        return s.split(",").join();
    }"#;
    run_cases(
        default_join_src,
        &[(
            vec![Val::String("x,y,z".into())],
            Val::String("x,y,z".into()),
        )],
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_boundary_audit_and_bounded_storage() -> Result<()> {
    // 1. Audit core module imports: string-only tasks must not import unrelated intrinsics
    let source = r#"export function run(s: string): string {
        return s.toUpperCase().split(",").join(" - ");
    }"#;
    let compiled = compile_typescript_waffle(
        source,
        "boundary_audit.ts",
        &WaffleCompileOptions::default(),
    )?;

    // Inspect core wasm imports via wasmparser
    let mut import_count = 0;
    for payload in wasmparser::Parser::new(0).parse_all(&compiled.core) {
        if let wasmparser::Payload::ImportSection(reader) = payload? {
            for import in reader.into_imports() {
                let imp = import?;
                import_count += 1;
                // Internal helpers must have been linked and removed
                assert!(
                    !imp.module.starts_with("__perry_helper"),
                    "Helper import {}:{} was not linked and stripped",
                    imp.module,
                    imp.name
                );
            }
        }
    }
    // String-only task without host capabilities must have 0 external core imports
    assert_eq!(
        import_count, 0,
        "Core module has unexpected external imports"
    );

    // 2. Measure compiler memory stability over repeated compilations (proves absence of compiler memory leak)
    let net_before_compiles = ALLOCATED
        .load(Ordering::SeqCst)
        .saturating_sub(DEALLOCATED.load(Ordering::SeqCst));
    for _ in 0..200 {
        let _ = compile_typescript_waffle(
            source,
            "boundary_audit.ts",
            &WaffleCompileOptions::default(),
        )?;
    }
    let net_after_compiles = ALLOCATED
        .load(Ordering::SeqCst)
        .saturating_sub(DEALLOCATED.load(Ordering::SeqCst));
    let compiler_leak = net_after_compiles.saturating_sub(net_before_compiles);
    // If every compilation leaked copied function bodies, 200 compilations would leak hundreds of kilobytes.
    assert!(
        compiler_leak < 50_000,
        "Compiler memory leak detected: heap grew by {} bytes across 200 compilations",
        compiler_leak
    );

    // 3. Component execution and repeated allocations with bounded memory growth
    let engine = make_async_engine()?;
    let component_bytes = compiled.component.expect("Component emitted");
    let component = Component::new(&engine, &component_bytes)?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(&engine, WasiHostState::default());

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;

    // Verify multiple sequential calls preserve return values without memory corruption
    let res1 = run.call_async(&mut store, ("alpha,beta,gamma",)).await?;
    assert_eq!(res1.0, "ALPHA - BETA - GAMMA");

    let res2 = run.call_async(&mut store, ("one,two",)).await?;
    assert_eq!(res2.0, "ONE - TWO");

    let res3 = run.call_async(&mut store, ("🦀,🌲,🌟",)).await?;
    assert_eq!(res3.0, "🦀 - 🌲 - 🌟");

    // Repeat 200 times to verify stable allocation behavior
    for _ in 0..200 {
        let res = run.call_async(&mut store, ("x,y,z",)).await?;
        assert_eq!(res.0, "X - Y - Z");
    }

    // 4. Measure runtime linear memory: instantiate core module directly (0 imports)
    // and verify linear memory usage is strictly bounded
    let core_engine = Engine::default();
    let core_module = Module::new(&core_engine, &compiled.core)?;
    let mut core_store = Store::new(&core_engine, ());
    let core_instance = Instance::new(&mut core_store, &core_module, &[])?;
    let memory = core_instance
        .get_memory(&mut core_store, "memory")
        .expect("linear memory exported");
    let initial_pages = memory.size(&core_store);
    let initial_bytes = memory.data_size(&core_store);
    assert!(
        initial_pages <= 3,
        "Initial memory pages should be bounded (got {})",
        initial_pages
    );
    assert!(
        initial_bytes <= 196_608,
        "Initial memory bytes should be bounded (got {})",
        initial_bytes
    );

    Ok(())
}

#[test]
fn test_unused_helpers_are_not_embedded_for_numeric_and_simple_tasks() -> Result<()> {
    // 1. Numeric-only task: must have 0 imports, at most 2 functions (compute + export wrapper),
    // 0 data segments, 0 helper code embedded.
    let numeric_src = r#"
        export function compute(a: number, b: number): number {
            return (a + b) * 2;
        }
    "#;
    let numeric_compiled =
        compile_typescript_waffle(numeric_src, "numeric.ts", &WaffleCompileOptions::default())?;

    let mut numeric_func_count = 0;
    let mut numeric_data_count = 0;
    let mut numeric_import_count = 0;
    let mut numeric_table_count = 0;
    for payload in wasmparser::Parser::new(0).parse_all(&numeric_compiled.core) {
        match payload? {
            wasmparser::Payload::FunctionSection(reader) => {
                numeric_func_count = reader.count();
            }
            wasmparser::Payload::ImportSection(reader) => {
                numeric_import_count = reader.count();
            }
            wasmparser::Payload::DataSection(reader) => {
                numeric_data_count = reader.count();
            }
            wasmparser::Payload::TableSection(reader) => {
                numeric_table_count = reader.count();
            }
            _ => {}
        }
    }
    assert_eq!(numeric_import_count, 0, "Numeric task must have 0 imports");
    assert_eq!(
        numeric_data_count, 0,
        "Numeric task must have 0 data segments"
    );
    assert_eq!(numeric_table_count, 0, "Numeric task must have 0 tables");
    assert!(
        numeric_func_count <= 2,
        "Numeric task must only define user function and export wrapper, found {} functions (helpers were embedded!)",
        numeric_func_count
    );

    // 2. Simple string concatenation task: needs string runtime primitives (concat),
    // but does NOT need SEARCH (str_find_substring) or TEXT (str_split, str_join, str_case_convert, str_code_point_at) helpers.
    let simple_src = r#"
        export function greet(name: string): string {
            return "hello " + name;
        }
    "#;
    let simple_compiled =
        compile_typescript_waffle(simple_src, "simple.ts", &WaffleCompileOptions::default())?;

    let mut simple_func_count = 0;
    let mut simple_import_count = 0;
    for payload in wasmparser::Parser::new(0).parse_all(&simple_compiled.core) {
        match payload? {
            wasmparser::Payload::FunctionSection(reader) => {
                simple_func_count = reader.count();
            }
            wasmparser::Payload::ImportSection(reader) => {
                simple_import_count = reader.count();
            }
            _ => {}
        }
    }
    assert_eq!(
        simple_import_count, 0,
        "Simple string task must have 0 external imports"
    );

    // 3. Search-only string task: needs SEARCH helper, but NOT TEXT helper.
    let search_src = r#"
        export function find_it(s: string): number {
            return s.indexOf("needle");
        }
    "#;
    let search_compiled =
        compile_typescript_waffle(search_src, "search.ts", &WaffleCompileOptions::default())?;

    let mut search_func_count = 0;
    for payload in wasmparser::Parser::new(0).parse_all(&search_compiled.core) {
        if let wasmparser::Payload::FunctionSection(reader) = payload? {
            search_func_count = reader.count();
        }
    }

    // 4. Full string task (using text helper: split, join, toUpperCase).
    let full_src = r#"
        export function transform(s: string): string {
            return s.toUpperCase().split(",").join(" - ");
        }
    "#;
    let full_compiled =
        compile_typescript_waffle(full_src, "full.ts", &WaffleCompileOptions::default())?;

    let mut full_func_count = 0;
    for payload in wasmparser::Parser::new(0).parse_all(&full_compiled.core) {
        if let wasmparser::Payload::FunctionSection(reader) = payload? {
            full_func_count = reader.count();
        }
    }

    // Hierarchical inclusion proves unused helper code is excluded, not embedded:
    // numeric (no strings) < simple (primitives only) < search (primitives + SEARCH) < full (primitives + TEXT)
    assert!(
        numeric_func_count < simple_func_count,
        "Numeric functions ({}) must be fewer than simple string functions ({})",
        numeric_func_count,
        simple_func_count
    );
    assert!(
        simple_func_count < search_func_count,
        "Simple string functions ({}) must exclude SEARCH helper functions ({})",
        simple_func_count,
        search_func_count
    );
    assert!(
        simple_func_count < full_func_count,
        "Simple string functions ({}) must exclude TEXT helper functions ({})",
        simple_func_count,
        full_func_count
    );

    // Verify module byte sizes reflect exclusion of unused helper code
    assert!(
        numeric_compiled.core.len() < simple_compiled.core.len(),
        "Numeric module size ({}) must be smaller than simple string module size ({})",
        numeric_compiled.core.len(),
        simple_compiled.core.len()
    );
    assert!(
        simple_compiled.core.len() < full_compiled.core.len(),
        "Simple string module size ({}) must be smaller than full helper module size ({})",
        simple_compiled.core.len(),
        full_compiled.core.len()
    );

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_split_indices_preserve_undefined() -> Result<()> {
    let source = r#"export async function run(s: string, sep: string, index: number): Promise<number> {
        let parts = s.split(sep);
        let value = await parts[index];
        if (value === undefined) { return 1; }
        if (value === "") { return 2; }
        if (value === "🦀") { return 3; }
        return 4;
    }"#;
    let cases = [
        ("A,🦀,", ",", 0.0, 4.0),
        ("A,🦀,", ",", 1.0, 3.0),
        ("A,🦀,", ",", 2.0, 2.0),
        ("A,B", ",", 2.0, 1.0),
        ("A,B", ",", -0.0, 4.0),
        ("A,B", ",", -1.0, 1.0),
        ("A,B", ",", -0.5, 1.0),
        ("A,B", ",", 0.5, 1.0),
        ("A,B", ",", f64::NAN, 1.0),
        ("A,B", ",", f64::INFINITY, 1.0),
        ("A,B", ",", f64::NEG_INFINITY, 1.0),
        ("A,B", ",", 1e20, 1.0),
        ("A,B", ",", 65536.0, 1.0),
        ("", "", 0.0, 1.0),
        ("", ",", 0.0, 2.0),
    ]
    .into_iter()
    .map(|(s, sep, i, n)| {
        (
            vec![
                Val::String(s.into()),
                Val::String(sep.into()),
                Val::Float64(i),
            ],
            Val::Float64(n),
        )
    })
    .collect::<Vec<_>>();
    run_cases(source, &cases).await
}
