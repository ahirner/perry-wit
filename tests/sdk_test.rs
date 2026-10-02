//! Integration test suite for the Perry-WIT Developer SDK and TypeScript code generator.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use perry_wit::sdk::{SdkOptions, generate_sdk_files};

#[test]
fn test_generate_sdk_files_for_merge_task() {
    let temp_dir = std::env::temp_dir().join("perry_sdk_test_merge_task");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let options = SdkOptions {
        wit_dir: PathBuf::from("wit"),
        world: Some("merge-task".to_string()),
        out_dir: temp_dir.join(".perry/types"),
        project_root: Some(temp_dir.clone()),
        entry: PathBuf::from("src/index.ts"),
    };

    let result = generate_sdk_files(&options).expect("generate_sdk_files failed");

    let types_file = temp_dir.join(".perry/types/world.d.ts");
    let tsconfig_file = temp_dir.join("tsconfig.json");

    assert_eq!(result.types_path, types_file);
    assert_eq!(result.tsconfig_path, Some(tsconfig_file.clone()));

    assert!(types_file.exists(), "world.d.ts should exist");
    assert!(tsconfig_file.exists(), "tsconfig.json should exist");

    let types_content = fs::read_to_string(&types_file).unwrap();
    assert!(
        types_content.contains("export interface MergeInput"),
        "Missing MergeInput interface: {}",
        types_content
    );
    assert!(
        types_content.contains("export interface MergedDoc"),
        "Missing MergedDoc interface: {}",
        types_content
    );
    assert!(
        types_content.contains("export declare function runTask(input: string): string;"),
        "Missing runTask signature: {}",
        types_content
    );
    assert!(
        types_content.contains("export declare function mergeTask(input: string): string;"),
        "Missing mergeTask signature: {}",
        types_content
    );

    let tsconfig_content = fs::read_to_string(&tsconfig_file).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&tsconfig_content).expect("tsconfig.json must be valid JSON");
    assert_eq!(
        parsed["compilerOptions"]["strict"],
        serde_json::Value::Bool(true)
    );
}

#[test]
fn test_generate_sdk_files_for_template_world() {
    let temp_dir = std::env::temp_dir().join("perry_sdk_test_template");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let options = SdkOptions {
        wit_dir: PathBuf::from("template/wit"),
        world: Some("task".to_string()),
        out_dir: temp_dir.join(".perry/types"),
        project_root: Some(temp_dir.clone()),
        entry: PathBuf::from("src/index.ts"),
    };

    let result = generate_sdk_files(&options).expect("generate_sdk_files for template failed");

    assert!(result.types_path.exists());
    let types_content = fs::read_to_string(&result.types_path).unwrap();
    assert!(
        types_content.contains("export declare function runTask(input: string): string;"),
        "Missing runTask in template declarations: {}",
        types_content
    );
}

#[test]
fn test_typecheck_examples_against_generated_declarations() {
    let temp_dir = std::env::temp_dir().join("perry_sdk_typecheck_suite");
    let _ = fs::remove_dir_all(&temp_dir);
    let src_dir = temp_dir.join("src");
    fs::create_dir_all(&src_dir).unwrap();

    // 1. Generate types for merge-task
    let options = SdkOptions {
        wit_dir: PathBuf::from("wit"),
        world: Some("merge-task".to_string()),
        out_dir: temp_dir.join(".perry/types"),
        project_root: Some(temp_dir.clone()),
        entry: PathBuf::from("src/index.ts"),
    };
    generate_sdk_files(&options).unwrap();

    // 2. Copy examples/merge_task.ts to src/merge_task.ts
    fs::copy("examples/merge_task.ts", src_dir.join("index.ts")).unwrap();

    // 3. Attempt tsc validation
    let direct_tsc = Command::new("tsc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let repo_root = std::env::current_dir().unwrap();
    let output = if direct_tsc {
        Command::new("tsc")
            .current_dir(&temp_dir)
            .arg("--noEmit")
            .output()
    } else {
        Command::new("nix")
            .args([
                "develop",
                repo_root.to_str().unwrap(),
                "--command",
                "tsc",
                "--noEmit",
                "--project",
                temp_dir.join("tsconfig.json").to_str().unwrap(),
            ])
            .output()
    };

    if let Ok(out) = output {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "tsc --noEmit failed.\nStdout: {}\nStderr: {}",
            stdout,
            stderr
        );
    }
}
