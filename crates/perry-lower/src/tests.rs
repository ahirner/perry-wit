use pliron::{builtin::ops::ModuleOp, context::Context, op::Op, printable::Printable};
use std::collections::HashMap;

/// Lower a source without foreign (WIT) types.
fn lower_without_foreign(ctx: &mut Context, source: &str, file: &str) -> anyhow::Result<ModuleOp> {
    crate::lower_source(ctx, source, file, &HashMap::new())
}

fn rejection(source: &str, file: &str) -> perry_wit_diagnostics::PerryDiagnostic {
    lower_without_foreign(&mut Context::new(), source, file)
        .err()
        .expect("rejection")
        .downcast()
        .expect("located diagnostic")
}

/// Diagnostic messages are tabled in the integration `diagnostics_test`; this pins the spans.
#[test]
fn unsupported_forms_are_rejected_with_located_diagnostics() {
    for (source, message, location) in [
        (
            "export function ff(a: number, b: number): number {\n  return a * b;\n}\nexport function fun(x: number, y: number): number {\n  return ff(a, b) + x + y;\n}",
            "cannot find name 'a'",
            (5, 13),
        ),
        (
            "export function run(x: number): string {\n  return x.toFixed(2);\n}",
            "method toFixed is not supported on number yet",
            (2, 10),
        ),
    ] {
        let diagnostic = rejection(source, "case.ts");
        assert_eq!(diagnostic.message, message);
        let span = diagnostic.span.expect("source location");
        assert_eq!((span.start_line, span.start_column), location);
    }
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
    const collided = ({x: 1}.x + {x: input}.x) + {x_2: 2}.x_2;
    return (fromMap(named)?.name ?? "missing") + counted.get("a")?.count
        + empty.size + names.has(input) + new Catalog().find(input)
        + lookup([{id:"a",name:input}]).get("a")?.name + new Catalog().defaulted()
        + (new Catalog().value() ?? 0) + collided;
}
"#;
    let module = lower_without_foreign(ctx, source, "generic-collections.ts")?;
    pliron::common_traits::Verify::verify(&*module.get_operation().deref(ctx), ctx)?;
    let printed = module.disp(ctx).to_string();
    for expected in [
        "{ id: perry.string , name: perry.string  }",
        "{ count: builtin.fp64 , id: perry.string  }",
        "{ x: builtin.fp64  }",
        "{ x: perry.string  }",
        "{ x_2: builtin.fp64  }",
    ] {
        assert!(
            printed.contains(expected),
            "missing {expected} in:\n{printed}"
        );
    }
    Ok(())
}

#[test]
fn recursive_class_calls_have_located_diagnostics() {
    for (members, entry, target) in [
        (
            "count(n:number):number { if(n<=0)return 0; return 1+this.count(n-1); }",
            "return new Counter().count(3);",
            "Counter.count",
        ),
        (
            "first(n:number):number { return this.second(n); }
             second(n:number):number { return this.first(n); }",
            "return new Counter().first(3);",
            "Counter.first",
        ),
        (
            "count(n:number):number { return this.count(this.count(n-1)); }",
            "return new Counter().count(3);",
            "Counter.count",
        ),
        (
            "static count(n:number):number { return Counter.count(n-1); }",
            "return Counter.count(3);",
            "Counter.count",
        ),
        (
            "count(n:number=this.count()):number { return n; }",
            "return new Counter().count();",
            "Counter.count",
        ),
        (
            "get count():number { return this.count; }",
            "return new Counter().count;",
            "Counter",
        ),
        (
            "set count(n:number) { this.count=n; }",
            "const c=new Counter(); c.count=3; return 0;",
            "Counter",
        ),
        (
            "constructor() { new Counter(); }",
            "new Counter(); return 0;",
            "Counter",
        ),
        (
            "count:number=1;
             constructor(n:number=new Counter().count) { this.count=n; }",
            "return new Counter().count;",
            "Counter",
        ),
        (
            "count:number=new Counter().count;",
            "return new Counter().count;",
            "field initialization of class Counter",
        ),
        (
            "count:number=new Counter().count; constructor() {}",
            "return new Counter().count;",
            "Counter",
        ),
    ] {
        let source =
            format!("class Counter {{ {members} }}\nexport function run():number {{ {entry} }}");
        let diagnostic = rejection(&source, "classes.ts");
        assert!(
            diagnostic.message.contains("recursive inline") && diagnostic.message.contains(target),
            "{source}\n{diagnostic:?}"
        );
        let span = diagnostic.span.expect("source location");
        assert_eq!(span.file.to_string_lossy(), "classes.ts");
        assert!(span.end_byte > span.start_byte, "{source}\n{span:?}");
    }
}
