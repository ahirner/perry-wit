//! Integration test suite for the Perry-WIT Developer SDK and TypeScript code generator.

use std::fs;
use std::path::PathBuf;

use perry_wit::sdk::{SdkOptions, generate_sdk_files};

#[test]
fn command_contract_accepts_scripts_and_validates_explicit_run_exports() {
    let temp_dir = std::env::temp_dir().join(format!("perry-sdk-command-{}", std::process::id()));
    fs::create_dir_all(temp_dir.join("src")).unwrap();
    generate_sdk_files(&SdkOptions {
        wit_dir: PathBuf::from("wit"),
        world: Some("command".into()),
        out_dir: temp_dir.join(".perry/types"),
        project_root: Some(temp_dir.clone()),
        entry: PathBuf::from("src/index.ts"),
        initialize_tsconfig: true,
    })
    .unwrap();
    for (source, succeeds) in [
        ("console.log('script');", true),
        (
            "import {readFile} from 'node:fs/promises'; console.log(await readFile('file','utf8'));",
            true,
        ),
        (
            "import {spawn} from 'node:child_process'; spawn('program');",
            false,
        ),
        ("process.env.VALUE = 'changed';", false),
        ("await 1; console.log('async script');", true),
        (
            "console.log('setup'); export function helper():number {return 2;}",
            true,
        ),
        (
            "export async function runRun(): Promise<{ok:true}|{ok:false}> { return {ok:true}; }",
            true,
        ),
        (
            "export function runRun(): {ok:true}|{ok:false} { return {ok:false}; }",
            true,
        ),
        (
            "export function runRun(): string { return 'wrong'; }",
            false,
        ),
        (
            "export function runRun(input: number): {ok:true} { return {ok:true}; }",
            false,
        ),
    ] {
        fs::write(temp_dir.join("src/index.ts"), source).unwrap();
        let output = std::process::Command::new("tsc")
            .arg("--noEmit")
            .current_dir(&temp_dir)
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            succeeds,
            "{source}\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    fs::remove_dir_all(temp_dir).unwrap();
}

#[test]
fn generated_contract_accepts_resolved_async_results_and_rejects_wrong_types() {
    let temp_dir = std::env::temp_dir().join(format!("perry-sdk-async-{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(temp_dir.join("src")).unwrap();
    fs::create_dir_all(temp_dir.join("wit")).unwrap();
    fs::write(
        temp_dir.join("wit/test.wit"),
        "package test:guest-async; world task { export run-task: func(input: string) -> string; }",
    )
    .unwrap();
    generate_sdk_files(&SdkOptions {
        wit_dir: temp_dir.join("wit"),
        world: Some("task".into()),
        out_dir: temp_dir.join(".perry/types"),
        project_root: Some(temp_dir.clone()),
        entry: PathBuf::from("src/index.ts"),
        initialize_tsconfig: true,
    })
    .unwrap();
    for (source, succeeds) in [
        (
            "export async function runTask(input: string): Promise<string> { return await input; }",
            true,
        ),
        (
            "export function runTask(input: string): string { return input; }",
            true,
        ),
        (
            "export async function runTask(input: string): Promise<number> { return 42; }",
            false,
        ),
        (
            "export async function runTask(input: number): Promise<string> { return 'wrong'; }",
            false,
        ),
    ] {
        fs::write(temp_dir.join("src/index.ts"), source).unwrap();
        let output = std::process::Command::new("tsc")
            .arg("--noEmit")
            .current_dir(&temp_dir)
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            succeeds,
            "{source}\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    fs::remove_dir_all(temp_dir).unwrap();
}

#[test]
fn result_contract_accepts_completion_or_explicit_result_implementations() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    fs::create_dir(root.join("wit")).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("wit/world.wit"),
        "package test:outcome; world task {
        record failure {name:string,message:string}
        type outcome = result<string,failure>;
        export run:async func(fail:bool)->outcome;
    }",
    )
    .unwrap();
    generate_sdk_files(&SdkOptions {
        wit_dir: root.join("wit"),
        world: Some("task".into()),
        out_dir: root.join(".perry/types"),
        project_root: Some(root.into()),
        entry: PathBuf::from("src/index.ts"),
        initialize_tsconfig: true,
    })
    .unwrap();
    for (source, succeeds) in [
        (
            "export function run(fail:boolean):string {if(fail)throw new Error('failed'); return 'ok';}",
            true,
        ),
        (
            "export async function run(fail:boolean):Promise<string> {if(fail)throw new Error('failed'); return 'ok';}",
            true,
        ),
        (
            "import type {Outcome} from '../.perry/types/world'; export function run(fail:boolean):Outcome {return fail?{ok:false,error:{name:'Error',message:'failed'}}:{ok:true,value:'ok'};}",
            true,
        ),
        (
            "import type {Outcome} from '../.perry/types/world'; export async function run(fail:boolean):Promise<Outcome> {return fail?{ok:false,error:{name:'Error',message:'failed'}}:{ok:true,value:'ok'};}",
            true,
        ),
        (
            "export function run(fail:boolean):number {return 42;}",
            false,
        ),
        (
            "export async function run(fail:boolean):Promise<number> {return 42;}",
            false,
        ),
        (
            "export function run(fail:number):string {return 'ok';}",
            false,
        ),
        (
            "export function run(fail:boolean):{ok:true,value:string}|{ok:false,error:number} {return {ok:false,error:42};}",
            false,
        ),
        (
            "import type {Outcome} from '../.perry/types/world'; export function run(fail:boolean):string|Outcome {return fail?{ok:false,error:{name:'Error',message:'failed'}}:'ok';}",
            false,
        ),
    ] {
        fs::write(root.join("src/index.ts"), source).unwrap();
        let output = std::process::Command::new("tsc")
            .arg("--noEmit")
            .current_dir(root)
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            succeeds,
            "{source}\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

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
        initialize_tsconfig: true,
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
        initialize_tsconfig: true,
    };
    generate_sdk_files(&options).unwrap();

    // 2. Copy examples/merge_task.ts to src/merge_task.ts
    fs::copy("examples/merge_task.ts", src_dir.join("index.ts")).unwrap();

    let output = std::process::Command::new("tsc")
        .current_dir(&temp_dir)
        .arg("--noEmit")
        .output()
        .expect("Run SDK checks through nix develop");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
