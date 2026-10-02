use perry_wit::sdk::{SdkOptions, generate_sdk_files};

#[test]
fn same_named_types_in_distinct_interfaces_retain_their_shapes() {
    let root = std::env::temp_dir().join(format!("perry-sdk-identities-{}", std::process::id()));
    fs::create_dir_all(root.join("wit")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("wit/world.wit"),
        r#"
        package test:identities;
        interface a { record item { text: string } get: func() -> item; }
        interface b { record item { count: u32 } get: func() -> item; }
        world test { export a; export b; }
    "#,
    )
    .unwrap();
    generate_sdk_files(&SdkOptions {
        wit_dir: root.join("wit"),
        world: Some("test".into()),
        out_dir: root.join(".perry/types"),
        project_root: Some(root.clone()),
    })
    .unwrap();
    fs::write(
        root.join("src/index.ts"),
        r#"
        import type { A, B } from '../.perry/types/world';
        const a: ReturnType<typeof A.get> = { text: "one" };
        const b: ReturnType<typeof B.get> = { count: 2 };
        // @ts-expect-error interface b has a different item type
        const wrongB: ReturnType<typeof B.get> = { text: "two" };
        // @ts-expect-error interface a has a different item type
        const wrongA: ReturnType<typeof A.get> = { count: 1 };
    "#,
    )
    .unwrap();
    let output = Command::new("tsc")
        .current_dir(&root)
        .args(["--noEmit", "--skipLibCheck", "false"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    fs::remove_dir_all(root).unwrap();
}
use std::{fs, process::Command};

#[test]
fn named_composite_aliases_and_nested_types_pass_strict_declaration_checking() {
    let root = std::env::temp_dir().join(format!("perry-sdk-composites-{}", std::process::id()));
    fs::create_dir_all(root.join("wit")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("wit/world.wit"),
        r#"
        package test:composites;
        interface types {
            record item { text: string }
            type names = list<string>;
            type maybe = option<names>;
            type pair = tuple<maybe, item>;
            type outcome = result<pair, string>;
        }
        world test {
            use types.{outcome};
            export run-task: func() -> outcome;
        }
    "#,
    )
    .unwrap();
    generate_sdk_files(&SdkOptions {
        wit_dir: root.join("wit"),
        world: Some("test".into()),
        out_dir: root.join(".perry/types"),
        project_root: Some(root.clone()),
    })
    .unwrap();
    fs::write(root.join("src/index.ts"), r#"
        import type { Outcome } from '../.perry/types/world';
        export function runTask(): Outcome { return { ok: true, value: [["hello"], {text: "item"}] }; }
        // @ts-expect-error the generated nested list must require strings
        const invalid: Outcome = { ok: true, value: [[42], {text: "item"}] };
    "#).unwrap();
    let output = Command::new("tsc")
        .current_dir(&root)
        .args(["--noEmit", "--skipLibCheck", "false"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    fs::remove_dir_all(root).unwrap();
}
