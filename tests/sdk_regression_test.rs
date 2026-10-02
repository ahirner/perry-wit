use perry_wit::sdk::{SdkOptions, generate_sdk_files};
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
