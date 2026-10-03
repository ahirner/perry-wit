//! Integration tests for WAFFLE backend string handling, scalar operations, and component round-trip.

use anyhow::Result;
use perry_wit::compile_typescript_waffle;
use perry_wit::waffle_backend::WaffleCompileOptions;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

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

    let compiled =
        compile_typescript_waffle(source, "slice.ts", &WaffleCompileOptions::default())?;
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
    let res = run.call_async(&mut store, ("🦀 Rust and TypeScript 🚀",)).await?;
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

    let compiled =
        compile_typescript_waffle(source, "result_roundtrip.ts", &WaffleCompileOptions::default())?;
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
