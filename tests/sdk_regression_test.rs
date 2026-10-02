use perry_wit::sdk::{SdkOptions, generate_sdk_files};

#[test]
fn generated_contract_checks_the_selected_implementation_module() {
    for custom in [false, true] {
        let root = std::env::temp_dir().join(format!(
            "perry-sdk-contract-{}-{custom}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("wit")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("wit/world.wit"),
            "package test:contract; world test { export run-task: func(input: string) -> string; }",
        )
        .unwrap();
        let entry = if custom {
            "src/custom.ts"
        } else {
            "src/index.ts"
        };
        let result = generate_sdk_files(&SdkOptions {
            wit_dir: root.join("wit"),
            world: Some("test".into()),
            out_dir: root.join(if custom {
                "generated/types"
            } else {
                ".perry/types"
            }),
            project_root: Some(root.clone()),
            entry: entry.into(),
        })
        .unwrap();
        assert!(result.check_path.exists());
        for (source, valid) in [
            (
                "export function runTask(input: string): string { return input; }",
                true,
            ),
            (
                "export function runTask(input: number): boolean { return true; }",
                false,
            ),
            ("export const unrelated = 1;", false),
        ] {
            fs::write(root.join(entry), source).unwrap();
            let output = Command::new("tsc")
                .current_dir(&root)
                .args(["--noEmit", "--skipLibCheck", "true"])
                .output()
                .unwrap();
            assert_eq!(
                output.status.success(),
                valid,
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            if !valid {
                assert!(String::from_utf8_lossy(&output.stdout).contains("runTask"));
            }
        }
        fs::remove_dir_all(root).unwrap();
    }
}

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
        entry: std::path::PathBuf::from("src/index.ts"),
    })
    .unwrap();
    fs::write(
        root.join("src/index.ts"),
        r#"
        import type { A, B } from '../.perry/types/world';
        export function aGet() { return { text: "one" }; }
        export function bGet() { return { count: 2 }; }
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
fn cli_generates_a_checked_sdk_with_default_relative_paths() {
    let root = std::env::temp_dir().join(format!("perry-sdk-cli-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("wit")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("wit/world.wit"),
        "package test:cli; world task { export run-task: func(input: string) -> string; }",
    )
    .unwrap();
    fs::write(
        root.join("src/index.ts"),
        "export function runTask(input: string): string { return input; }",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_perry-wit"))
        .current_dir(&root)
        .arg("gen-types")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.join("tsconfig.json").exists());
    assert!(root.join(".perry/types/implementation-check.ts").exists());
    let output = Command::new("tsc")
        .current_dir(&root)
        .arg("--noEmit")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    fs::remove_dir_all(root).unwrap();
}

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
        entry: std::path::PathBuf::from("src/index.ts"),
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
