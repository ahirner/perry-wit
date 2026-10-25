//! Scalar export boundaries: which runtime pieces (memory, realloc, roots) are retained.
use super::{compile, inspect_wasm};
use crate::core_program::Program;
use crate::world::Compiler::Standard;
use anyhow::{Context, Result};
use wasmtime::Val;

#[test]
fn independent_scalar_boundaries_have_no_runtime_roots() -> Result<()> {
    let engine = wasmtime::Engine::default();
    for source in [
        "export function run(a:number,b:number):number { return a+b; }",
        "export function run(a:number,b:number):number { return [a,b].length+a; }",
        "export function run(a:number,b:number):number { let sum=b; for(let i=0;i<a;i++)sum+=1; return sum; }",
        "export function run(a:number,b:number):number { return a+b; } export function unused():string { return String(Math.random()); }",
    ] {
        let compiled = compile(source, "func(a:f64,b:f64)->f64")?;
        let wasm = inspect_wasm(&compiled.core)?;
        assert_eq!(wasm.functions, 1, "export the implementation directly");
        assert_eq!(wasm.memories, 0);
        assert_eq!(wasm.imports, 0);
        assert_eq!(wasm.exports, ["run"]);
        assert!(compiled.core.len() < 2963);
        let (mut store, instance) =
            crate::world::instantiate(&engine, &compiled.component.unwrap().stripped)?;
        let run = instance.get_typed_func::<(f64, f64), (f64,)>(&mut store, "run")?;
        assert_eq!(run.call(&mut store, (5.0, 2.0))?, (7.0,));
    }
    Ok(())
}

#[test]
fn scalar_signatures_do_not_hide_runtime_effects() -> Result<()> {
    let engine = wasmtime::Engine::default();
    for source in [
        "export function run(a:number):number { return String(a).length; }",
        "function check(a:number):number { if(a<0)throw new Error('negative'); return a; } export function run(a:number):number { try { return check(a); } catch { return -1; } }",
        "let count=0; export function run(a:number):number { try { count+=a; return count; } catch { return -1; } }",
    ] {
        let compiled = compile(source, "func(a:f64)->f64")?;
        let wasm = inspect_wasm(&compiled.core)?;
        assert!(
            !wasm.has_start,
            "constant helper initialization must be resolved before instantiation"
        );
        assert!(
            !crate::world::function_names(&compiled.core)?.contains(&"__wasm_apply_data_relocs")
        );
        assert!(wasm.exports.contains(&"memory"), "{source}");
        assert!(wasm.exports.contains(&"cabi_post_run"), "{source}");
        assert!(!wasm.exports.contains(&"cabi_realloc"), "{source}");
        let (mut store, instance) =
            crate::world::instantiate(&engine, &compiled.component.unwrap().stripped)?;
        let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
        assert_eq!(run.call(&mut store, (1.0,))?, (1.0,));
    }
    Ok(())
}

#[test]
fn unused_host_imports_do_not_root_the_allocator() -> Result<()> {
    let compiled = Standard.compile(
        "package test:unused; world test { import label: func() -> string; export run: func(a:f64)->f64; }",
        "test",
        "unused.ts",
        "export function run(a:number):number { return String(a).length; }",
    )?;
    assert!(
        !inspect_wasm(&compiled.core)?
            .exports
            .contains(&"cabi_realloc")
    );
    // Component encoding validates the reachable import and export ABI without realloc.
    assert!(compiled.component.is_some());
    Ok(())
}

#[test]
fn unreachable_functions_do_not_add_code_or_host_requirements() {
    let source = r#"
function recur(n: number): number { if (n <= 0) return 2; return recur(n - 1) + 1; }
async function next(n: number): Promise<number> { return recur(n); }
export async function run(n: number): Promise<number> { return await next(n); }
"#;
    let extra = r#"
function unusedCycle(n: number): number { return unusedOther(n); }
function unusedOther(n: number): number { return unusedCycle(n); }
async function unusedFetch(): Promise<string> { return await (await fetch('https://example.test')).text(); }
function unusedLog(): void { console.log('unreachable'); }
"#;
    let build = |source: &str| perry_wit::compile_core(source, "case.ts").expect("source compiles");
    let core = build(source);
    assert_eq!(core, build(&format!("{source}{extra}")));
    let mut program = Program::from_wasm(core);
    assert_eq!(program.number("run", 5.0), 7.0);
}

#[test]
fn linked_function_names_identify_executing_frames() {
    let mut program = Program::new(
        "export function spin(n: number): string { let text = ''; while (n > 0) { text += String(n); n--; } return text; }",
    );
    program.store.set_fuel(50_000).unwrap();
    let function = program
        .instance
        .get_func(&mut program.store, "spin")
        .unwrap();
    let error = function
        .call(
            &mut program.store,
            &[Val::F64(1_000.0_f64.to_bits())],
            &mut [Val::I32(0)],
        )
        .unwrap_err();
    let trace = error
        .downcast_ref::<wasmtime::WasmBacktrace>()
        .expect("fuel trap has guest frames");
    assert!(
        trace
            .frames()
            .iter()
            .any(|frame| frame.func_name() == Some("spin")),
        "{error:#}"
    );
}

#[test]
fn repeated_leaf_calls_keep_a_bounded_compilation_size() {
    let calls = (0..80)
        .map(|i| format!("sum += choose(n, {i});"))
        .collect::<String>();
    let source = format!(
        r#"
function choose(a: number, b: number): number {{
  if (a < b) return a + b;
  if (a > b) return a - b;
  return a * b;
}}
export function run(n: number): number {{ let sum = 0; {calls} return sum; }}
"#
    );
    let mut program = Program::new(&source);
    let expected = (0..80)
        .map(|i| {
            let b = i as f64;
            if 7.0 < b {
                7.0 + b
            } else if 7.0 > b {
                7.0 - b
            } else {
                7.0 * b
            }
        })
        .sum::<f64>();
    assert_eq!(program.number("run", 7.0), expected);
    assert!(
        program.wasm.len() < 15_000,
        "bounded inlining grew the component unexpectedly"
    );
}

/// Projections through resolved generic shapes and in-bounds character loops cannot throw, so
/// their exports carry no exception status result.
#[test]
fn proven_non_throwing_exports_have_no_status_result() -> Result<()> {
    let mut program = Program::from_wasm(perry_wit::compile_core(
        r#"
interface Item { n:number }
interface Pair<Item,Other=Item> { a:Item; b:Other }
interface Page<T> {items:T[];next:string|null}
interface Base<T> {value:T}
interface Entry<T=string> extends Base<T> {}
type Box<T> = {item:T};
interface Link<T> {value:T;next:Link<T>|null}
function size(page:Page<Entry>) {const items = page.items; const first = items[0]; return first.value.length;}
function unbox(box:Box<number>):number {return box.item;}
export function run():number {
 const value:Pair<string,Item> = {a:'x',b:{n:17}};
 const defaults:Pair<string> = {a:'a',b:'bc'};
 const page:Page<Entry> = {items:[{value:'hello'}],next:null};
 const link:Link<number> = {value:3,next:null};
 return value.b.n + defaults.b.length + size(page) + unbox({item:2}) + link.value;
}

export function charLoop(s: string): number {
 let count = 0;
 for (let i = 0; i < s.length; i++) { if (s[i] === '-') count++; }
 return count;
}
"#,
        "projections.ts",
    )?);
    for export in ["run", "charLoop"] {
        let function = program
            .instance
            .get_func(&mut program.store, export)
            .with_context(|| format!("missing export {export}"))?;
        assert_eq!(function.ty(&program.store).results().len(), 1, "{export}");
    }
    Ok(())
}

#[test]
fn byte_extents_preserve_subview_and_index_semantics() -> Result<()> {
    for (view, start, limit, step, index, expected, checked) in [
        ("bytes", "0", "8", "1", "i", 36.0, false),
        ("bytes", "-1", "2", "1", "i + 1", 6.0, false),
        ("bytes", "-1", "3", "2", "i + 1", 4.0, false),
        ("bytes.subarray(2, 6)", "0", "4", "1", "i", 18.0, false),
        ("bytes.subarray(-6, -2)", "0", "4", "1", "i", 18.0, false),
        (
            "bytes.subarray(1, 7).subarray(1, 5)",
            "0",
            "4",
            "1",
            "i",
            18.0,
            false,
        ),
        ("bytes.subarray(4, 4)", "0", "1", "1", "i", 1000.0, true),
        ("bytes", "0", "4", "1", "i * 2", 16.0, false),
        ("bytes", "0", "5", "1", "i * 2", 1016.0, true),
        ("bytes", "0", "8", "1", "i / 2", 4010.0, true),
        ("bytes", "0", "2", "1", "i + 7", 1008.0, true),
        ("bytes", "0", "2", "1", "i - 1", 1001.0, true),
        ("new Uint8Array(0)", "0", "1", "1", "i", 1000.0, true),
        ("new Uint8Array(3.5)", "0", "4", "1", "i", 1000.0, true),
    ] {
        let source = format!(
            r#"
export function run(): number {{
    const bytes = new Uint8Array(8);
    for (let i = 0; i < 8; i++) bytes[i] = i + 1;
    const view = {view}; const alias = view;
    let sum = 0;
    for (let i = {start}; i < {limit}; i += {step}) {{
        const value = alias[{index}];
        sum += value === undefined ? 1000 : value;
    }}
    return sum;
}}
"#
        );
        let core = perry_wit::compile_core(&source, "extents.ts")?;
        assert_eq!(
            crate::world::function_names(&core)?.contains(&"std_bytes_get"),
            checked,
            "checked byte read: {view}, {start}..{limit}, {index}"
        );
        let mut program = Program::from_wasm(core);
        let (results, _) = program.call("run", &[]);
        assert_eq!(results[0].f64(), Some(expected), "{view}: {limit}: {index}");
        assert!(results.get(1).is_none_or(|status| status.i32() == Some(0)));
    }
    Ok(())
}

#[test]
fn nonfinite_arithmetic_cannot_prove_byte_bounds() -> Result<()> {
    for expression in ["positive-positive", "positive+negative"] {
        let source = format!(
            r#"
export function run(limit:number):number {{
    const bytes=new Uint8Array(2); bytes[0]=7;
    let positive=0; let negative=0;
    for(let i=0;i<limit;i++) {{ positive+=1e308; negative-=1e308; }}
    const index={expression};
    if(index<0)return -1;
    if(index>=2)return -2;
    return bytes[index]===undefined?42:bytes[index];
}}
"#
        );
        let core = perry_wit::compile_core(&source, "nonfinite-bounds.ts")?;
        assert!(crate::world::function_names(&core)?.contains(&"std_bytes_get"));
        let mut program = Program::from_wasm(core);
        assert_eq!(program.number("run", 1.0), 7.0, "{expression}");
        assert_eq!(program.number("run", 2.0), 42.0, "{expression}");
    }
    Ok(())
}
