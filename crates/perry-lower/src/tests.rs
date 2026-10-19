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
