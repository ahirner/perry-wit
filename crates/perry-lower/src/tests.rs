use pliron::{builtin::ops::ModuleOp, context::Context, op::Op, printable::Printable};
use std::collections::HashMap;

/// Lower a source without foreign (WIT) types.
fn lower_without_foreign(ctx: &mut Context, source: &str, file: &str) -> anyhow::Result<ModuleOp> {
    crate::lower_source(ctx, source, file, &HashMap::new())
}

/// Diagnostic messages are tabled in the integration `diagnostics_test`; this pins the spans.
#[test]
fn unsupported_forms_are_rejected_with_located_diagnostics() {
    let ctx = &mut Context::new();
    let err = lower_without_foreign(
        ctx,
        "export function ff(a: number, b: number): number {\n  return a * b;\n}\nexport function fun(x: number, y: number): number {\n  return ff(a, b) + x + y;\n}",
        "case.ts",
    )
    .err()
    .expect("rejection");
    let diag = err
        .downcast_ref::<perry_wit_diagnostics::PerryDiagnostic>()
        .expect("diagnostic");
    assert_eq!(diag.message, "cannot find name 'a'");
    let span = diag.span.as_ref().expect("span on unresolved identifier");
    assert_eq!(span.start_line, 5);
    assert_eq!(span.start_column, 13);

    let call_err = lower_without_foreign(
        ctx,
        "export function run(x: number): string {\n  return x.toFixed(2);\n}",
        "case.ts",
    )
    .err()
    .expect("rejection");
    let call_diag = call_err
        .downcast_ref::<perry_wit_diagnostics::PerryDiagnostic>()
        .expect("diagnostic");
    assert_eq!(
        call_diag.message,
        "method toFixed is not supported on number yet"
    );
    let call_span = call_diag.span.as_ref().expect("span on method call");
    assert_eq!(call_span.start_line, 2);
    assert_eq!(call_span.start_column, 10);
}

#[test]
fn frame_proofs_follow_reference_escape_through_calls() {
    let ctx = &mut Context::new();
    let source = r#"
interface Cell { value: number }
let retained: Cell | null = null;
function read(cell: Cell, n: number): number { if (n < 0) return read(cell, -n); return cell.value; }
function save(cell: Cell, n: number): void { if (n < 0) { save(cell, -n); return; } retained = cell; }
export function local(x: number): number { const cell: Cell = { value: x }; return read(cell, x); }
export function escaped(x: number): number { const cell: Cell = { value: x }; save(cell, x); return x; }
"#;
    let module = lower_without_foreign(ctx, source, "lifetimes.ts").unwrap();
    perry_mir::canonicalize(module.get_operation(), ctx).unwrap();
    let printed = module.disp(ctx).to_string();
    assert!(printed.contains("perry.call read"), "{printed}");
    assert!(printed.contains("perry.call save"), "{printed}");
    assert!(printed.contains("perry.alloc Frame"), "{printed}");
    assert!(printed.contains("perry.alloc Instance"), "{printed}");
}

#[test]
fn generic_collections_keep_concrete_payloads() -> anyhow::Result<()> {
    let ctx = &mut Context::new();
    let source = r#"
function index<T extends {id:string}>(xs:T[]):Map<string,T> {
    const result = new Map<string,T>();
    for (const x of xs) {
        if (result.has(x.id)) throw new Error("duplicate");
        result.set(x.id,x);
    }
    return result;
}
function forward<T extends {id:string}>(xs:T[]):Map<string,T> { return index(xs); }
function unique<T>(xs:T[]) { return new Set<T>(xs); }
function fromMap<T>(values:Map<string,T>):T|undefined { return values.get("a"); }
function first<T>(xs:T[]):T { return xs[0]; }
function defaulted<T>(value:T):T {
    const read = (fallback:T = first([value].map((item:T):T => item))):T => fallback;
    return read();
}
class Catalog {
    cached: string = first(["cached"]);
    static cached: number = first([7]);
    value() { return [1, 2].at(0); }
    defaulted(values: Map<string,{id:string,name:string}> = index([{id:"a",name:"default"}])): string {
        return values.get("a")?.name ?? "";
    }
    find(input:string):string { return index([{id:"a",name:input}]).get("a")?.name ?? ""; }
}
export function catalogValue() { return new Catalog().value(); }
export function run(input:string):string {
    const named = index([{id:"a",name:defaulted(input)}]);
    const counted = forward([{id:"a",count:defaulted(7)}]);
    const empty = index<{id:string,name:string}>([]);
    const names = unique([input]);
    const lookup = (xs:{id:string,name:string}[]):Map<string,{id:string,name:string}> => index(xs);
    return (fromMap(named)?.name ?? "missing") + counted.get("a")?.count
        + empty.size + names.has(input) + new Catalog().find(input)
        + lookup([{id:"a",name:input}]).get("a")?.name + new Catalog().defaulted()
        + (new Catalog().value() ?? 0);
}
"#;
    let module = lower_without_foreign(ctx, source, "generic-collections.ts")?;
    pliron::common_traits::Verify::verify(&*module.get_operation().deref(ctx), ctx)?;
    let printed = module.disp(ctx).to_string();
    assert!(printed.contains("shape_id_name"), "{printed}");
    assert!(printed.contains("shape_count_id"), "{printed}");
    Ok(())
}
