//! Integration tests for WAFFLE backend string handling, scalar operations, and component round-trip.

#[path = "support/waffle.rs"]
mod waffle_fixture;
use anyhow::Result;
use perry_wit::waffle_backend::WaffleCompileOptions;
use std::alloc::{GlobalAlloc, Layout, System};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use waffle_fixture::compile_typescript_waffle;
use wasmtime::component::{Component, Linker, ResourceTable, Val};
use wasmtime::{Config, Engine, Instance, Module, Store, StoreLimits, StoreLimitsBuilder};
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
    limits: StoreLimits,
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
async fn test_long_invocations_reclaim_dead_text_temporaries() -> Result<()> {
    let source = r#"function transient(input: string): string {
        return input.slice(0, 2).toUpperCase() + ".";
    }
    export function run(input: string, count: number, fail: boolean): number {
        const retained = input.slice(1, 4);
        let result = 0;
        for (let i = 0; i < count; i++) {
            try {
                const temporary = transient(input).split(".").join("-");
                if (fail) throw 7;
                result = temporary.length;
            } catch (e) { result = e as number; }
            finally { const cleanup = input.toLowerCase(); result = result + cleanup.length; }
        }
        return result + retained.length;
    }"#;
    let compiled = compile_typescript_waffle(
        source,
        "temporary_lifetimes.ts",
        &WaffleCompileOptions::default(),
    )?;
    let engine = make_async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let linker = Linker::new(&engine);
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(524_288).build(),
    );
    store.limiter(|limits| limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str, f64, bool), (f64,)>(&mut store, "run")?;
    let input = "aß😀z".repeat(128);
    for count in [1.0, 20_000.0] {
        for fail in [false, true] {
            assert_eq!(
                run.call_async(&mut store, (&input, count, fail)).await?.0,
                if fail { 522.0 } else { 519.0 }
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_collection_retains_interior_views_operands_and_return_payloads() -> Result<()> {
    let prefix = r#"
        function churn(input: string, count: number): string {
            for (let i = 0; i < count; i++) {
                const temporary = input.toLowerCase().split(".").join("-");
            }
            return "!";
        }
    "#;
    let input = "aß🦀.".repeat(128);
    for (body, expected) in [
        (
            "return input.toUpperCase() + churn(input, count);",
            format!("{}!", "ASS🦀.".repeat(128)),
        ),
        (
            "try { return input.toUpperCase().slice(1); } finally { churn(input, count); }",
            format!("SS🦀.{}", "ASS🦀.".repeat(127)),
        ),
        (
            "const retained = input.toUpperCase().split(\".\")[0]; churn(input, count); return retained.slice(1);",
            "SS🦀".into(),
        ),
        (
            "let retained = input.slice(0, 4); for (let i = 0; i < count; i++) { if (i === 1) retained = input.toUpperCase().slice(1, 4); churn(input, 1); } return retained;",
            "SS🦀".into(),
        ),
    ] {
        let source = format!(
            "{prefix} export function run(input: string, count: number): string {{ {body} }}"
        );
        let compiled = compile_typescript_waffle(
            &source,
            "interior_roots.ts",
            &WaffleCompileOptions::default(),
        )?;
        let engine = make_async_engine()?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let linker = Linker::new(&engine);
        let mut store = Store::new(
            &engine,
            StoreLimitsBuilder::new().memory_size(524_288).build(),
        );
        store.limiter(|limits| limits);
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(&str, f64), (String,)>(&mut store, "run")?;
        assert_eq!(
            run.call_async(&mut store, (&input, 2000.0)).await?.0,
            expected,
            "{body}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_regex_search_positions_are_scalar_indices() -> Result<()> {
    run_cases(
        r#"export function run(input: string): number { return input.search(/é😀/u); }"#,
        &[
            (vec![Val::String("x🦀é😀z".into())], Val::Float64(2.0)),
            (vec![Val::String("é😀".into())], Val::Float64(0.0)),
            (vec![Val::String("".into())], Val::Float64(-1.0)),
        ],
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_regex_search_matches_node_with_scalar_positions() -> Result<()> {
    let patterns = [
        (r"a.*z|x", "u"),
        (r"aba", "u"),
        (r"(?:ab|b)+", "u"),
        (r"(?:)", "u"),
        (r"^", "u"),
        (r"$", "u"),
        (r"^$", "u"),
        (r"a?b*?", "u"),
        (r"[]", "u"),
        (r"[^]", "u"),
        (r"[^\s\S]", "u"),
        (r"\d+", "u"),
        (r"\D+", "u"),
        (r"[\w-]+", "u"),
        (r"\s", "u"),
        (r"\S", "u"),
        (r"\uD83D\uDE00", "u"),
        (r".", "u"),
        (r".", "su"),
        (r"é|abc", "u"),
        (r"\/\\", "u"),
        (r"[\b]", "u"),
        (r"\bfoo\b", "u"),
        (r"\Bfoo", "u"),
        (r"a$", "u"),
        (r"[\D_]+", "u"),
        (r"\u{1F600}", "u"),
        (r"\xE9", "u"),
        (r"\0", "u"),
        (r"[()?]", "u"),
    ];
    let inputs = [
        "",
        "🦀é😀",
        "ababa",
        "a😀xfoo z",
        "😀foo_foo foo",
        "\r\n\u{2028}\u{2029}abc",
        "\u{85}\u{FEFF}é",
        "a\n",
        "１２3_",
        "/\\\0\u{8}",
        "e\u{301}é",
    ];
    let branches = patterns
        .iter()
        .enumerate()
        .map(|(index, (pattern, flags))| {
            format!("if (mode === {index}) return input.search(/{pattern}/{flags});")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let source = format!(
        "export function run(input: string, mode: number): number {{ {branches} return -1; }}"
    );
    let scratch = tempfile::tempdir()?;
    let fixture = scratch.path().join("regex.mts");
    std::fs::write(
        &fixture,
        format!(
            "{source}\nconst inputs = {}; const results = []; for (let mode = 0; mode < {}; mode++) for (const input of inputs) {{ const index = run(input, mode); results.push(index < 0 ? -1 : Array.from(input.slice(0, index)).length); }} console.log(JSON.stringify(results));",
            serde_json::to_string(&inputs)?,
            patterns.len()
        ),
    )?;
    let node = Command::new("node").arg(fixture).output()?;
    assert!(
        node.status.success(),
        "{}",
        String::from_utf8_lossy(&node.stderr)
    );
    let expected: Vec<f64> = serde_json::from_slice(&node.stdout)?;
    let cases = patterns
        .iter()
        .enumerate()
        .flat_map(|(mode, _)| {
            inputs
                .iter()
                .map(move |input| vec![Val::String((*input).into()), Val::Float64(mode as f64)])
        })
        .zip(expected.into_iter().map(Val::Float64))
        .collect::<Vec<_>>();
    run_cases(&source, &cases).await?;
    run_cases(
        r#"export function run(input: string): string { const index = input.search(/(?:é|😀)+/u); if (index < 0) return "missing"; return input.slice(index); }"#,
        &[(vec![Val::String("x🦀é😀tail".into())], Val::String("é😀tail".into())), (vec![Val::String("abc".into())], Val::String("missing".into()))],
    ).await
}

#[tokio::test(flavor = "current_thread")]
async fn test_regex_tables_coexist_with_text_helpers_and_suspension() -> Result<()> {
    run_cases(
        r#"import { waitFor } from "perry:clocks";
        export async function run(input: string): Promise<string> {
            const upper = input.toUpperCase();
            await waitFor(1);
            const index = upper.search(/(?:É|😀)+/u);
            if (index < 0) return "missing";
            return upper.slice(index).split("|").join(".").toLowerCase();
        }"#,
        &[(
            vec![Val::String("🦀ßé😀|Z".into())],
            Val::String("é😀.z".into()),
        )],
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_regex_search_has_no_per_search_guest_allocation() -> Result<()> {
    let source = r#"export function run(input: string, count: number): number {
        let result = 0;
        for (let i = 0; i < count; i++) { result = result + input.search(/é😀/u); }
        return result;
    }"#;
    let compiled = compile_typescript_waffle(
        source,
        "regex_allocation.ts",
        &WaffleCompileOptions::default(),
    )?;
    assert!(!compiled.component_wat.as_ref().unwrap().contains("wasi:"));
    let engine = make_async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let linker = Linker::new(&engine);
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(65_536).build(),
    );
    store.limiter(|limits| limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str, f64), (f64,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(
            run.call_async(&mut store, ("🦀é😀", 20_000.0)).await?.0,
            20_000.0
        );
    }
    Ok(())
}

#[test]
fn test_unsupported_regex_forms_are_diagnosed() {
    for (expression, diagnostic) in [
        (
            r"input.search(/a{1000000}/u)",
            "Unsupported or oversized literal regex",
        ),
        (r"input.search(/a/i)", "supports only the u and s flags"),
        (r"input.search(/(?=a)/u)", "Regex lookaround"),
        (r"input.search(/(a)\1/u)", "Unsupported regex escape"),
        (r"input.search(/\uD800/u)", "Unpaired surrogate"),
        (r"input.search(/\u{D800}/u)", "unpaired surrogate"),
        (r"input.search(/\uDC00/u)", "unpaired surrogate"),
        (r#"input.search("a")"#, "requires a literal regex"),
        (
            r"input.search(new RegExp(input))",
            "RegExp construction is unsupported",
        ),
        (
            r#"input.search(new RegExp("a", "u", input))"#,
            "RegExp construction is unsupported",
        ),
        (
            r#"input.search(RegExp("a", "u"))"#,
            "RegExp construction is unsupported",
        ),
        (r"input.search(/[a&&b]/u)", "set operators are unsupported"),
    ] {
        let source =
            format!("export function run(input: string): number {{ return {expression}; }}");
        let error = compile_typescript_waffle(
            &source,
            "unsupported_regex.ts",
            &WaffleCompileOptions::default(),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains(diagnostic),
            "{source}: {error:#}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_shadowed_regexp_is_an_ordinary_guest_function() -> Result<()> {
    run_cases(
        r#"function RegExp(value: string): number { return value.length; }
        export function run(): number { return RegExp("🦀x"); }"#,
        &[(vec![], Val::Float64(2.0))],
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_scalar_iteration_preserves_complete_characters() -> Result<()> {
    run_cases(
        r#"export function run(input: string): string {
            let result = "";
            for (const scalar of input) { result = result + "[" + scalar + "]"; }
            return result;
        }"#,
        &[
            (vec![Val::String("".into())], Val::String("".into())),
            (
                vec![Val::String("Aé😀e\u{301}\0".into())],
                Val::String("[A][é][😀][e][\u{301}][\0]".into()),
            ),
        ],
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_scalar_iteration_and_loop_cleanup_match_node() -> Result<()> {
    let source = r#"export function run(mode: number): string {
        let result = "";
        if (mode === 0) {
            let input = "A😀é";
            for (const scalar of input) {
                input = "changed";
                result = result + scalar.toUpperCase();
            }
        } else if (mode === 1) {
            for (const scalar of "a😀bé") {
                try {
                    if (scalar === "a") continue;
                    if (scalar === "b") break;
                    result = result + scalar;
                } finally { result = result + "!"; }
            }
        } else if (mode === 2) {
            try {
                for (const outer of "ab") {
                    for (const inner of "😀xy") {
                        try {
                            if (inner === "x") break;
                            result = result + outer + inner;
                        } finally { result = result + "i"; }
                    }
                    result = result + "o";
                }
            } finally { result = result + "f"; }
        } else if (mode === 3) {
            try {
                for (const scalar of "ab") {
                    try { throw 7; }
                    finally { result = result + scalar; }
                }
            } catch (e) { result = result + "c"; }
            finally { result = result + "f"; }
        } else if (mode === 4) {
            for (const scalar of "abc") {
                try { break; }
                finally {
                    result = result + scalar;
                    if (scalar === "b") break;
                    continue;
                }
            }
        } else if (mode === 5) {
            for (const scalar of "😀") {
                try { return "wrong"; }
                finally { result = scalar; break; }
            }
        } else if (mode === 6) {
            for (let i = 0; i < 3; i = i + 1) { result = result + "x"; }
        } else if (mode === 7) {
            let i = 0;
            while (i < 5) {
                ++i;
                try {
                    if (i === 1) continue;
                    if (i === 3) break;
                    result = result + "x";
                } finally { result = result + "f"; }
            }
        } else if (mode === 8) {
            for (;;) { result = "once"; break; }
        } else if (mode === 9) {
            try { return "preserved"; }
            finally { for (const scalar of "ab") { break; } }
        } else if (mode === 10) {
            for (const scalar of "ab") {
                try {
                    try { continue; }
                    finally { result = result + scalar; }
                } finally { result = result + "f"; }
            }
        } else {
            let i = 3;
            for (; i-- > 0;) {
                if (i === 1) continue;
                result = result + "x";
            }
            if (i !== -1) return "wrong update ordering";
        }
        return result;
    }"#;
    let expected = [
        "A😀É",
        "!😀!!",
        "a😀iiob😀iiof",
        "acf",
        "ab",
        "😀",
        "xxx",
        "fxff",
        "once",
        "preserved",
        "afbf",
        "xx",
    ];
    let cases = expected
        .iter()
        .enumerate()
        .map(|(mode, result)| {
            (
                vec![Val::Float64(mode as f64)],
                Val::String((*result).into()),
            )
        })
        .collect::<Vec<_>>();
    run_cases(source, &cases).await?;
    let scratch = tempfile::tempdir()?;
    let fixture = scratch.path().join("iteration.mts");
    std::fs::write(
        &fixture,
        format!(
            "{source}\nconsole.log(JSON.stringify(Array.from({{length: {}}}, (_, i) => run(i))));",
            expected.len()
        ),
    )?;
    let node = Command::new("node").arg(fixture).output()?;
    assert!(
        node.status.success(),
        "{}",
        String::from_utf8_lossy(&node.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Vec<String>>(&node.stdout)?,
        expected
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_scalar_iteration_retains_state_across_p3_waits() -> Result<()> {
    run_cases(
        r#"import { waitFor } from "perry:clocks";
        export async function run(input: string): Promise<string> {
            let result = "";
            for (const scalar of input) {
                try {
                    await waitFor(1);
                    if (scalar === "x") continue;
                    result = result + scalar.toUpperCase();
                } finally { result = result + "."; }
            }
            return result;
        }"#,
        &[(
            vec![Val::String("ßx😀".into())],
            Val::String("SS..😀.".into()),
        )],
    )
    .await
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
    for alignment in [0, 3, 6, u32::MAX] {
        assert!(
            realloc
                .call(&mut store, (moved, 32, alignment, 48))
                .is_err()
        );
    }
    assert!(realloc.call(&mut store, (moved, 33, 8, 48)).is_err());
    assert!(realloc.call(&mut store, (moved + 1, 5, 8, 48)).is_err());
    assert!(realloc.call(&mut store, (moved, 32, 8, u32::MAX)).is_err());
    assert_eq!(
        &memory.data(&store)[moved as usize..moved as usize + 5],
        b"saved"
    );
    assert_eq!(realloc.call(&mut store, (moved, 32, 8, 0))?, 0);
    let mut previous = 0;
    let mut previous_size = 0;
    for iteration in 0..2000 {
        let alignment = [1, 2, 4, 8, 64, 256, 4096][iteration % 7];
        let size = [16, 128, 24, 256, 8][iteration % 5];
        let next = realloc.call(&mut store, (previous, previous_size, alignment, size))?;
        assert_eq!(next % alignment, 0);
        if previous != 0 {
            assert_eq!(memory.data(&store)[next as usize], 0xA5);
        }
        memory.write(&mut store, next as usize, &[0xA5])?;
        previous = next;
        previous_size = size;
    }
    assert_eq!(memory.size(&store), 1);
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
    let cases = cases.into_iter().chain([
        (
            vec![Val::String("ABC".into()), Val::Float64(f64::NAN)],
            Val::Float64(65.0),
        ),
        (
            vec![Val::String("ABC".into()), Val::Float64(-0.5)],
            Val::Float64(65.0),
        ),
        (
            vec![Val::String("ABC".into()), Val::Float64(-0.0)],
            Val::Float64(65.0),
        ),
        (
            vec![Val::String("ABC".into()), Val::Float64(1.9)],
            Val::Float64(66.0),
        ),
        (
            vec![Val::String("ABC".into()), Val::Float64(1e20)],
            Val::Float64(f64::NAN),
        ),
        (
            vec![Val::String("ABC".into()), Val::Float64(f64::INFINITY)],
            Val::Float64(f64::NAN),
        ),
        (
            vec![Val::String("ABC".into()), Val::Float64(f64::NEG_INFINITY)],
            Val::Float64(f64::NAN),
        ),
    ]);
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

    // The cap permits one invocation, but cannot hold all 200 invocation arenas.
    let engine = make_async_engine()?;
    let component_bytes = compiled.component.expect("Component emitted");
    let component = Component::new(&engine, &component_bytes)?;
    let linker = make_wasi_linker(&engine)?;
    let mut store = Store::new(
        &engine,
        WasiHostState {
            limits: StoreLimitsBuilder::new().memory_size(524_288).build(),
            ..Default::default()
        },
    );
    store.limiter(|state| &mut state.limits);

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;

    // Verify multiple sequential calls preserve return values without memory corruption
    let res1 = run.call_async(&mut store, ("alpha,beta,gamma",)).await?;
    assert_eq!(res1.0, "ALPHA - BETA - GAMMA");

    let res2 = run.call_async(&mut store, ("one,two",)).await?;
    assert_eq!(res2.0, "ONE - TWO");

    let res3 = run.call_async(&mut store, ("🦀,🌲,🌟",)).await?;
    assert_eq!(res3.0, "🦀 - 🌲 - 🌟");

    let input = format!("{},ß", "é🦀".repeat(2048));
    let expected = format!("{} - SS", "É🦀".repeat(2048));
    for _ in 0..200 {
        let res = run.call_async(&mut store, (&input,)).await?;
        assert_eq!(res.0, expected);
    }

    // Inspect the same workload through the raw canonical ABI, including post-return.
    let core_engine = Engine::default();
    let core_module = Module::new(&core_engine, &compiled.core)?;
    let mut core_store = Store::new(&core_engine, ());
    let core_instance = Instance::new(&mut core_store, &core_module, &[])?;
    let memory = core_instance
        .get_memory(&mut core_store, "memory")
        .expect("linear memory exported");
    let realloc = core_instance
        .get_typed_func::<(u32, u32, u32, u32), u32>(&mut core_store, "cabi_realloc")?;
    let run = core_instance.get_typed_func::<(u32, u32), u32>(&mut core_store, "run")?;
    let post_return = core_instance.get_typed_func::<u32, ()>(&mut core_store, "cabi_post_run")?;
    let mut first_allocation = None;
    let mut peak_pages = None;
    for _ in 0..200 {
        let ptr = realloc.call(&mut core_store, (0, 0, 1, input.len() as u32))?;
        assert_eq!(*first_allocation.get_or_insert(ptr), ptr);
        memory.write(&mut core_store, ptr as usize, input.as_bytes())?;
        let result = run.call(&mut core_store, (ptr, input.len() as u32))?;
        let bytes = memory.data(&core_store);
        let output_ptr =
            u32::from_le_bytes(bytes[result as usize..result as usize + 4].try_into()?) as usize;
        let output_len =
            u32::from_le_bytes(bytes[result as usize + 4..result as usize + 8].try_into()?)
                as usize;
        assert_eq!(
            &bytes[output_ptr..output_ptr + output_len],
            expected.as_bytes()
        );
        assert_ne!(
            &bytes[..4],
            &[0; 4],
            "result must remain allocated until post-return"
        );
        post_return.call(&mut core_store, result)?;
        assert_eq!(&memory.data(&core_store)[..4], &[0; 4]);
        let pages = memory.size(&core_store);
        assert_eq!(
            *peak_pages.get_or_insert(pages),
            pages,
            "pages must plateau after the first allocating call"
        );
    }

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
async fn test_post_return_reclaims_all_export_outcomes() -> Result<()> {
    let input = "é🦀".repeat(2048);
    let expected_string = format!("{input}🦀{input}{input}");
    let engine = make_async_engine()?;
    let linker = make_wasi_linker(&engine)?;
    for (result_type, return_value, expected) in [
        ("void", "", vec![]),
        ("boolean", "true", vec![Val::Bool(true)]),
        ("number", "text.length", vec![Val::Float64(12289.0)]),
        ("string", "text", vec![Val::String(expected_string.clone())]),
        (
            "Result<string, number>",
            "text",
            vec![Val::Result(Ok(Some(Box::new(Val::String(
                expected_string.clone(),
            )))))],
        ),
    ] {
        let can_throw = result_type.starts_with("Result");
        let source = format!(
            r#"
            function recurse(s: string, count: number): string {{
                if (count === 0) return s + "🦀";
                return recurse(s, count - 1) + s;
            }}
            export function run(fail: boolean): {result_type} {{
                let text = recurse("{input}", 2);
                {}
                return {return_value};
            }}"#,
            if can_throw { "if (fail) throw 7;" } else { "" }
        );
        let compiled =
            compile_typescript_waffle(&source, "post_return.ts", &WaffleCompileOptions::default())?;
        let component = Component::new(&engine, compiled.component.unwrap())?;
        let mut store = Store::new(
            &engine,
            WasiHostState {
                limits: StoreLimitsBuilder::new().memory_size(524_288).build(),
                ..Default::default()
            },
        );
        store.limiter(|state| &mut state.limits);
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_func(&mut store, "run").unwrap();
        for iteration in 0..100 {
            let fail = can_throw && iteration % 2 == 0;
            let mut result = vec![Val::Bool(false); expected.len()];
            run.call_async(&mut store, &[Val::Bool(fail)], &mut result)
                .await?;
            if fail {
                assert_eq!(
                    result,
                    [Val::Result(Err(Some(Box::new(Val::Float64(7.0)))))]
                );
            } else {
                assert_eq!(result, expected, "{result_type} at iteration {iteration}");
            }
        }
    }
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

#[test]
fn test_join_rejects_nonarray_receivers_and_extra_arguments() {
    for receiver in [r#""ABC""#, "false", "42", "undefined"] {
        let source = format!(
            "export function run(): string {{ let value: any = {receiver}; return value.join(\"-\"); }}"
        );
        let error = compile_typescript_waffle(
            &source,
            "join_receiver.ts",
            &WaffleCompileOptions::default(),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("string-array receiver"),
            "{error:#}"
        );
    }
    let error = compile_typescript_waffle(
        r#"export function run(s: string): string { return s.split(",").join("-", "extra"); }"#,
        "join_arity.ts",
        &WaffleCompileOptions::default(),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("at most one separator"),
        "{error:#}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_string_result_signature_retains_canonical_allocator() -> Result<()> {
    for signature in [
        "function run(): Result<string, number>",
        "async function run(): Promise<Result<string, number>>",
    ] {
        run_cases(
            &format!("export {signature} {{ throw 1; }}"),
            &[(vec![], Val::Result(Err(Some(Box::new(Val::Float64(1.0))))))],
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_undefined_only_equality_without_string_helpers() -> Result<()> {
    for (operator, expected) in [("===", true), ("==", true), ("!==", false), ("!=", false)] {
        let source = format!(
            "export async function run(): Promise<boolean> {{ let a = await undefined; return a {operator} undefined; }}"
        );
        run_cases(&source, &[(vec![], Val::Bool(expected))]).await?;
        let compiled =
            compile_typescript_waffle(&source, "undefined.ts", &WaffleCompileOptions::default())?;
        for payload in wasmparser::Parser::new(0).parse_all(&compiled.core) {
            if let wasmparser::Payload::FunctionSection(reader) = payload? {
                assert_eq!(
                    reader.count(),
                    2,
                    "Undefined comparisons must not link string helpers"
                );
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_code_points_preserve_number_or_undefined() -> Result<()> {
    let source = r#"export async function run(s: string, index: number): Promise<number> {
        let cp = await s.codePointAt(index);
        let result = 0;
        if (cp === undefined) { result = result + 1; }
        if (cp === cp) { result = result + 2; }
        if (cp === 65) { result = result + 4; }
        if (undefined !== cp) { result = result + 8; }
        if (cp) { result = result + 16; }
        if (cp == s.codePointAt(999)) { result = result + 32; }
        if (cp === s[999]) { result = result + 64; }
        if (cp === 0 / 0) { return 999; }
        if (cp === false) { return 999; }
        if (cp === "") { return 999; }
        return result;
    }"#;
    let cases = [
        ("A🦀B", 0.0, 30.0),
        ("A🦀B", 1.0, 26.0),
        ("A🦀B", 3.0, 99.0),
        ("ABC", -1.0, 99.0),
        ("ABC", f64::INFINITY, 99.0),
        ("", 0.0, 99.0),
        ("\0", 0.0, 10.0),
    ]
    .into_iter()
    .map(|(s, i, n)| {
        (
            vec![Val::String(s.into()), Val::Float64(i)],
            Val::Float64(n),
        )
    })
    .collect::<Vec<_>>();
    run_cases(source, &cases).await
}

#[tokio::test(flavor = "current_thread")]
async fn test_from_code_point_unwinds_guest_handlers() -> Result<()> {
    let source = r#"
        export async function run(cp: number): Promise<string> {
            let result = "start";
            try {
                try { result = await helper(cp); }
                finally { result = result + "!"; }
            } catch (e) { result = result + "caught"; }
            finally { result = result + "done"; }
            return result;
        }
        export function helper(cp: number): string { return String.fromCodePoint(cp); }
    "#;
    let mut cases = vec![(vec![Val::Float64(65.0)], Val::String("A!done".into()))];
    for cp in [
        -1.0,
        -0.5,
        1.5,
        0xD800 as f64,
        0xDFFF as f64,
        0x110000 as f64,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ] {
        cases.push((
            vec![Val::Float64(cp)],
            Val::String("start!caughtdone".into()),
        ));
    }
    run_cases(source, &cases).await?;
    run_cases(r#"export function run(cp: number): string { try { return String.fromCodePoint(cp); } catch (e) { return "caught"; } }"#,
        &[(vec![Val::Float64(-1.0)], Val::String("caught".into()))]).await
}

#[tokio::test(flavor = "current_thread")]
async fn test_join_rejects_size_overflow_before_copying() -> Result<()> {
    let source =
        r#"export function run(s: string, sep: string): string { return s.split(",").join(sep); }"#;
    let error = run_cases(
        source,
        &[(
            vec![
                Val::String(",".repeat(65_536)),
                Val::String("x".repeat(65_536)),
            ],
            Val::String("".into()),
        )],
    )
    .await
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("unreachable"), "{error}");
    assert!(
        !error.contains("out of bounds"),
        "Join must reject overflow before writing: {error}"
    );
    run_cases(
        source,
        &[(
            vec![Val::String(",".repeat(1024)), Val::String("🦀".into())],
            Val::String("🦀".repeat(1024)),
        )],
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_unicode_casing_expansions_and_context() -> Result<()> {
    run_cases(
        r#"export function run(s: string): string { return s.toUpperCase(); }"#,
        &[
            (vec![Val::String("αяﬃ".into())], Val::String("ΑЯFFI".into())),
            (
                vec![Val::String("ΐ".repeat(32_768))],
                Val::String("Ι\u{308}\u{301}".repeat(32_768)),
            ),
        ],
    )
    .await?;
    run_cases(
        r#"export function run(s: string): string { return s.toLowerCase(); }"#,
        &[
            (
                vec![Val::String("ΣЯİ".into())],
                Val::String("σяi\u{307}".into()),
            ),
            (
                vec![Val::String("ΟΣ ΟΣΑ Σ ΑΣ\u{301} ΑΣ\u{301}Α".into())],
                Val::String("ος οσα σ ας\u{301} ασ\u{301}α".into()),
            ),
        ],
    )
    .await?;
    let mapped: String = (0..=0x10FFFF)
        .filter_map(char::from_u32)
        .filter(|&c| {
            c.to_lowercase().ne(std::iter::once(c)) || c.to_uppercase().ne(std::iter::once(c))
        })
        .collect();
    let contexts = "ΑΣ.Α ΑΣ-Α ΑΣ'Α ΑΣ' Α\u{345}Σ \u{345}Σ ǅΣ 𐐀Σ \0Σ İΣ";
    for (method, upper) in [("toUpperCase", true), ("toLowerCase", false)] {
        let cases = [mapped.as_str(), contexts, "აԱևᎠꭰ🦀\0"]
            .into_iter()
            .map(|input| {
                let expected = if upper {
                    input.to_uppercase()
                } else {
                    input.to_lowercase()
                };
                (vec![Val::String(input.into())], Val::String(expected))
            })
            .collect::<Vec<_>>();
        run_cases(
            &format!("export function run(s: string): string {{ return s.{method}(); }}"),
            &cases,
        )
        .await?;
    }
    run_cases(
        r#"export function run(s: string): number { return s.toUpperCase().length; }"#,
        &[(vec![Val::String("αяﬃΐ".into())], Val::Float64(8.0))],
    )
    .await
}
