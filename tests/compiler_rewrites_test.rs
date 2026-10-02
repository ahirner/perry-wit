#[path = "../src/compiler/rewrites.rs"]
mod rewrites;
mod support;

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
    rewrites::rewrite_program(&mut hir);
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
