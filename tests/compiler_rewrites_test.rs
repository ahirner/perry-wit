#[expect(
    dead_code,
    reason = "Async lowering is exercised through component tests; this suite inspects its AST rewrite."
)]
#[path = "../src/compiler/async_lowering.rs"]
mod async_lowering;
#[path = "../src/compiler/fetch.rs"]
mod fetch;
#[path = "../src/compiler/rewrites.rs"]
mod rewrites;
mod support;

use perry_parser::swc_ecma_ast::{CallExpr, Callee, Expr, Lit, MemberProp};
use swc_ecma_visit::{Visit, VisitWith};

#[test]
fn promise_constructor_rewrite_respects_lexical_bindings() {
    struct Constructors(usize);
    impl Visit for Constructors {
        fn visit_call_expr(&mut self, call: &CallExpr) {
            if matches!(&call.callee, Callee::Expr(callee)
                if matches!(callee.as_ref(), Expr::Member(member)
                    if matches!(&member.prop, MemberProp::Ident(property) if property.sym == "async_promise_new")))
            {
                self.0 += 1;
            }
            call.visit_children_with(self);
        }
    }
    for (source, expected) in [
        ("new Promise(resolve => resolve(7));", 1),
        (
            "function create(undefined) { return new Promise(resolve => resolve(7)); }",
            1,
        ),
        ("class Promise { constructor(value) {} } new Promise(7);", 0),
        ("function create(Promise) { return new Promise(7); }", 0),
        (
            "{ class Promise { constructor(value) {} } new Promise(7); }",
            0,
        ),
        ("import { Promise } from 'custom'; new Promise(7);", 0),
    ] {
        let mut ast = perry_parser::parse_typescript(source, "constructor.ts").unwrap();
        let original = ast.clone();
        fetch::preserve_calls(&mut ast);
        let mut constructors = Constructors(0);
        ast.visit_with(&mut constructors);
        assert_eq!(constructors.0, expected, "{source}");
        if expected == 0 {
            assert_eq!(ast, original, "{source}");
        }
    }
}

#[test]
fn unlink_rewrite_respects_import_bindings_and_lexical_scopes() {
    struct UnlinkCalls(Vec<String>);
    impl Visit for UnlinkCalls {
        fn visit_call_expr(&mut self, call: &CallExpr) {
            if let Callee::Expr(callee) = &call.callee
                && let Expr::Member(member) = callee.as_ref()
                && let MemberProp::Ident(property) = &member.prop
                && property.sym == "fs_unlink_sync"
            {
                let Expr::Lit(Lit::Str(path)) = call.args[0].expr.as_ref() else {
                    panic!("expected a literal test path");
                };
                self.0.push(path.value.to_string_lossy().into_owned());
            }
            call.visit_children_with(self);
        }
    }

    for (source, expected) in [
        ("import { unlinkSync } from 'fs'; unlinkSync('file');", 1),
        (
            "import { unlinkSync as remove } from 'node:fs'; remove('file');",
            1,
        ),
        (
            "import { unlinkSync as fetch } from 'fs'; fetch('file');",
            1,
        ),
        (
            "import * as disk from 'node:fs'; disk.unlinkSync('file');",
            1,
        ),
        ("import disk from 'fs'; disk.unlinkSync('file');", 1),
        (
            "function unlinkSync(path: string) {} unlinkSync('file');",
            0,
        ),
        (
            "const disk = { unlinkSync(path: string) {} }; disk.unlinkSync('file');",
            0,
        ),
        ("receiver().unlinkSync('file');", 0),
        ("import { unlinkSync } from 'other'; unlinkSync('file');", 0),
        (
            "import { existsSync as unlinkSync } from 'fs'; unlinkSync('file');",
            0,
        ),
        ("import disk from 'other'; disk.unlinkSync('file');", 0),
        (
            "import type { unlinkSync } from 'fs'; unlinkSync('file');",
            0,
        ),
        (
            "import { type unlinkSync } from 'fs'; unlinkSync('file');",
            0,
        ),
        (
            "import { unlinkSync } from 'fs'; function f(unlinkSync: any) { unlinkSync('shadow'); } unlinkSync('file');",
            1,
        ),
        (
            "import * as fs from 'fs'; function f(fs: any) { fs.unlinkSync('shadow'); } fs.unlinkSync('file');",
            1,
        ),
        (
            "import { unlinkSync } from 'fs'; { const unlinkSync = local; unlinkSync('shadow'); } unlinkSync('file');",
            1,
        ),
        (
            "import { unlinkSync } from 'fs'; function f() { unlinkSync('file'); function unlinkSync(path: string) {} }",
            0,
        ),
        (
            "import * as fs from 'fs'; function f() { fs.unlinkSync('file'); var fs = local; }",
            0,
        ),
        (
            "import * as fs from 'fs'; try {} catch (fs) { fs.unlinkSync('shadow'); } fs.unlinkSync('file');",
            1,
        ),
        (
            "import { unlinkSync as remove } from 'fs'; for (const remove of items) { remove('shadow'); } remove('file');",
            1,
        ),
        (
            "import * as fs from 'fs'; function f({ fs }: any) { fs.unlinkSync('file'); }",
            0,
        ),
    ] {
        let mut ast = perry_parser::parse_typescript(source, "bindings.ts").unwrap();
        let original = ast.clone();
        fetch::preserve_calls(&mut ast);
        let mut calls = UnlinkCalls(Vec::new());
        ast.visit_with(&mut calls);
        assert_eq!(calls.0, vec!["file"; expected], "{source}");
        if expected == 0 {
            assert_eq!(
                ast, original,
                "unrelated calls must remain unchanged: {source}"
            );
        }
    }
}

#[test]
fn class_bodies_contribute_runtime_capabilities() {
    for member in [
        "constructor() { OP; }",
        "read() { return OP; }",
        "static read() { return OP; }",
        "get value() { return OP; }",
        "set value(value: any) { OP; }",
        "value = OP;",
        "static value = OP;",
    ] {
        for (operation, marker) in [
            ("Date.now()", "__needs_clocks__"),
            ("fetch('http://example.test')", "__needs_http__"),
        ] {
            let source = format!("class Capability {{ {} }}", member.replace("OP", operation));
            let ast = perry_parser::parse_typescript(&source, "class.ts").unwrap();
            let mut hir = perry_hir::lower_module(&ast, "main", "class.ts").unwrap();
            rewrites::rewrite_program(&mut hir).unwrap();
            assert!(
                hir.init.iter().any(|statement| matches!(statement,
                    perry_hir::ir::Stmt::Expr(perry_hir::ir::Expr::String(value)) if value == marker
                )),
                "missing {marker} in {source}"
            );
        }
    }
}

#[test]
fn static_class_clock_call_selects_clock_dispatch() {
    let output = support::run(
        "class Clock { static read() { return Date.now(); } } console.log(Clock.read() > 0);",
        None,
        None,
    );
    assert_eq!(support::stdout(&output), "true\n");
}

#[test]
fn replacements_rewrite_their_children_including_nested_spreads() {
    let output = support::run(
        r#"
        const a = JSON.parse('{"title":"nested"}');
        console.log(JSON.stringify({ ...a }));
        console.log(JSON.stringify({ ...{ ...a } }));
        if (true) { console.log(JSON.stringify({ ...a })); }
    "#,
        None,
        None,
    );
    let output = support::stdout(&output);
    let objects = serde_json::Deserializer::from_str(&output)
        .into_iter::<serde_json::Value>()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(objects, vec![serde_json::json!({"title": "nested"}); 3]);
}

#[test]
fn compatibility_rewrites_visit_nested_statement_bodies_and_conditions() {
    let source = r#"
        if (JSON.stringify(1)) { JSON.stringify(2); } else { JSON.stringify(3); }
        while (JSON.stringify(4)) { JSON.stringify(5); break; }
        do { JSON.stringify(6); } while (JSON.stringify(7));
        for (let x = JSON.stringify(8); JSON.stringify(9); JSON.stringify(10)) { JSON.stringify(11); }
        outer: { JSON.stringify(12); }
        try { JSON.stringify(13); } catch (e) { JSON.stringify(14); } finally { JSON.stringify(15); }
        switch (JSON.stringify(16)) { case JSON.stringify(17): JSON.stringify(18); break; default: JSON.stringify(19); }
        const f = () => { if (true) { JSON.stringify(20); } };
        function g() { if (true) { return JSON.stringify(21); } }
    "#;
    let ast = perry_parser::parse_typescript(source, "nested.ts").unwrap();
    let mut hir = perry_hir::lower_module(&ast, "main", "nested.ts").unwrap();
    assert!(format!("{hir:?}").contains("JsonStringifyFull"));
    rewrites::rewrite_program(&mut hir).unwrap();
    assert!(!format!("{hir:?}").contains("JsonStringifyFull"));

    let output = support::run(
        r#"
        const a = JSON.parse('{"title":"nested"}');
        if (true) { const b = { ...a }; console.log(JSON.stringify(b)); }
        for (let i = 0; i < 2; i++) { const b = { ...a }; console.log(JSON.stringify(b)); }
    "#,
        None,
        None,
    );
    let output = support::stdout(&output);
    let objects = serde_json::Deserializer::from_str(&output)
        .into_iter::<serde_json::Value>()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(objects, vec![serde_json::json!({"title": "nested"}); 3]);
}
