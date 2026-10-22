//! The Developer SDK: generated declarations type-check implementations under `tsc`, and the
//! CLI writes declarations, configuration and components without clobbering authored files.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use perry_wit::sdk::{SdkOptions, generate_sdk_files};

/// Run `tsc --noEmit` with `args` in `directory`; returns success and the diagnostics.
fn tsc_check(directory: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new("tsc")
        .current_dir(directory)
        .arg("--noEmit")
        .args(args)
        .output()
        .expect("tsc on PATH (run SDK checks through nix develop)");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// Write `wit/world.wit` (unless `wit` is `None`, which selects the repository's `wit/`) and
/// generate a checked SDK for `world` and `entry` into `.perry/types`.
fn generate(root: &Path, wit: Option<&str>, world: &str, entry: &str) {
    let wit_dir = match wit {
        Some(wit) => {
            fs::create_dir_all(root.join("wit")).unwrap();
            fs::write(root.join("wit/world.wit"), wit).unwrap();
            root.join("wit")
        }
        None => PathBuf::from("wit"),
    };
    generate_sdk_files(&SdkOptions {
        wit_dir,
        world: Some(world.into()),
        out_dir: root.join(".perry/types"),
        project_root: Some(root.into()),
        entry: entry.into(),
        initialize_tsconfig: true,
    })
    .unwrap();
}

/// WIT text (`None` selects the repository's `wit/`), world, entry, and implementations paired
/// with whether they type-check.
type ContractCase = (
    Option<&'static str>,
    &'static str,
    &'static str,
    Vec<(String, bool)>,
);

/// Each generated contract accepts exactly the implementations whose signatures it describes.
#[test]
fn generated_contracts_accept_only_matching_implementations() {
    const OUTCOME_WIT: &str = "package test:outcome; world task {
        record failure {name:string,message:string}
        type outcome = result<string,failure>;
        export run:async func(fail:bool)->outcome;
    }";
    const OUTCOME: &str = "import type {Outcome} from '../.perry/types/world';";
    let lookup = |body: &str| {
        format!(
            "import {{load}} from 'test:async-sdk/lookup'; export async function run(key:string):Promise<string> {{{body}}}"
        )
    };
    let cases: [ContractCase; 6] = [
        (
            None,
            "command",
            "src/index.ts",
            [
                ("console.log('script');", true),
                ("import {readFile} from 'node:fs/promises'; console.log(await readFile('file','utf8'));", true),
                ("import {spawn} from 'node:child_process'; spawn('program');", false),
                ("process.env.VALUE = 'changed';", false),
                ("await 1; console.log('async script');", true),
                ("console.log('setup'); export function helper():number {return 2;}", true),
                ("export async function runRun(): Promise<{ok:true}|{ok:false}> { return {ok:true}; }", true),
                ("export function runRun(): {ok:true}|{ok:false} { return {ok:false}; }", true),
                ("export function runRun(): string { return 'wrong'; }", false),
                ("export function runRun(input: number): {ok:true} { return {ok:true}; }", false),
            ]
            .map(|(source, valid)| (source.to_owned(), valid))
            .to_vec(),
        ),
        (
            Some("package test:guest-async; world task { export run-task: func(input: string) -> string; }"),
            "task",
            "src/index.ts",
            [
                ("export async function runTask(input: string): Promise<string> { return await input; }", true),
                ("export function runTask(input: string): string { return input; }", true),
                ("export async function runTask(input: string): Promise<number> { return 42; }", false),
                ("export async function runTask(input: number): Promise<string> { return 'wrong'; }", false),
            ]
            .map(|(source, valid)| (source.to_owned(), valid))
            .to_vec(),
        ),
        (
            Some(OUTCOME_WIT),
            "task",
            "src/index.ts",
            vec![
                ("export function run(fail:boolean):string {if(fail)throw new Error('failed'); return 'ok';}".into(), true),
                ("export async function run(fail:boolean):Promise<string> {if(fail)throw new Error('failed'); return 'ok';}".into(), true),
                (format!("{OUTCOME} export function run(fail:boolean):Outcome {{return fail?{{ok:false,error:{{name:'Error',message:'failed'}}}}:{{ok:true,value:'ok'}};}}"), true),
                (format!("{OUTCOME} export async function run(fail:boolean):Promise<Outcome> {{return fail?{{ok:false,error:{{name:'Error',message:'failed'}}}}:{{ok:true,value:'ok'}};}}"), true),
                ("export function run(fail:boolean):number {return 42;}".into(), false),
                ("export async function run(fail:boolean):Promise<number> {return 42;}".into(), false),
                ("export function run(fail:number):string {return 'ok';}".into(), false),
                ("export function run(fail:boolean):{ok:true,value:string}|{ok:false,error:number} {return {ok:false,error:42};}".into(), false),
                (format!("{OUTCOME} export function run(fail:boolean):string|Outcome {{return fail?{{ok:false,error:{{name:'Error',message:'failed'}}}}:'ok';}}"), false),
            ],
        ),
        (
            Some("package test:async-sdk; interface lookup {load:async func(key:string)->string;} world boundary {import lookup; export run:async func(key:string)->string;}"),
            "boundary",
            "component.ts",
            vec![
                (lookup("return (await load(key)).toUpperCase();"), true),
                (lookup("return load(key).toUpperCase();"), false),
                (lookup("return (await load(key)).length;"), false),
            ],
        ),
        (
            Some("package test:identities; interface a { record item { text: string } get: func() -> item; } interface b { record item { count: u32 } get: func() -> item; } world test { export a; export b; }"),
            "test",
            "src/index.ts",
            vec![(
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
    "#
                .into(),
                true,
            )],
        ),
        (
            Some("package test:composites; interface types { record item { text: string } type names = list<string>; type maybe = option<names>; type pair = tuple<maybe, item>; type outcome = result<pair, string>; } world test { use types.{outcome}; export run-task: func() -> outcome; }"),
            "test",
            "src/index.ts",
            vec![(
                r#"
        import type { Outcome } from '../.perry/types/world';
        export function runTask(): Outcome { return { ok: true, value: [["hello"], {text: "item"}] }; }
        // @ts-expect-error the generated nested list must require strings
        const invalid: Outcome = { ok: true, value: [[42], {text: "item"}] };
    "#
                .into(),
                true,
            )],
        ),
    ];
    for (wit, world, entry, sources) in cases {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src")).unwrap();
        generate(root, wit, world, entry);
        for (source, valid) in sources {
            fs::write(root.join(entry), &source).unwrap();
            let (success, diagnostics) = tsc_check(root, &["--skipLibCheck", "false"]);
            assert_eq!(success, valid, "{world}: {source}\n{diagnostics}");
        }
    }
}

#[test]
fn generated_contract_checks_the_selected_implementation_module() {
    for custom in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
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
            project_root: Some(root.into()),
            entry: entry.into(),
            initialize_tsconfig: true,
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
            let (success, diagnostics) = tsc_check(root, &["--skipLibCheck", "true"]);
            assert_eq!(success, valid, "{diagnostics}");
            if !valid {
                assert!(diagnostics.contains("runTask"), "{diagnostics}");
            }
        }
    }
}

#[test]
fn examples_type_check_against_generated_declarations() {
    for (example, world) in [("merge_task", "merge-task"), ("merge_docs", "merge-docs")] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src")).unwrap();
        let result = generate_sdk_files(&SdkOptions {
            wit_dir: PathBuf::from(format!("examples/{example}")),
            world: Some(world.to_string()),
            out_dir: root.join(".perry/types"),
            project_root: Some(root.into()),
            entry: PathBuf::from("src/index.ts"),
            initialize_tsconfig: true,
        })
        .unwrap();
        assert_eq!(result.types_path, root.join(".perry/types/world.d.ts"));
        assert_eq!(result.tsconfig_path, Some(root.join("tsconfig.json")));
        let tsconfig: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(root.join("tsconfig.json")).unwrap()).unwrap();
        assert_eq!(tsconfig["compilerOptions"]["strict"], true);
        fs::copy(
            format!("examples/{example}/index.ts"),
            root.join("src/index.ts"),
        )
        .unwrap();
        fs::write(
            root.join("src/temporal.ts"),
            r#"
function formatTemporal(epoch: number): string {
    const instant: Temporal.Instant = Temporal.Instant.fromEpochMilliseconds(epoch);
    const date: Temporal.PlainDate = Temporal.PlainDate.from('2024-02-28').add({ days: 1 });
    const time: Temporal.PlainDateTime = Temporal.PlainDateTime.from('2024-02-29T12:30');
    return instant.toJSON() + date.toJSON() + time.toJSON();
}
"#,
        )
        .unwrap();
        let (success, diagnostics) = tsc_check(root, &[]);
        assert!(success, "{example}: {diagnostics}");
    }
}

#[test]
fn disposable_sdk_checks_the_entry_without_rewriting_authored_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::create_dir(root.join("wit")).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("wit/world.wit"),
        "package test:disposable; world task {export run:func()->string;}",
    )
    .unwrap();
    fs::write(
        root.join("src/index.ts"),
        "export function run():string{return 'hello';}",
    )
    .unwrap();
    let options = SdkOptions {
        wit_dir: root.join("wit"),
        out_dir: root.join(".perry/types"),
        project_root: Some(root.into()),
        initialize_tsconfig: false,
        ..Default::default()
    };
    generate_sdk_files(&options).unwrap();
    assert!(!root.join("tsconfig.json").exists());
    let config = "{\"compilerOptions\":{\"strict\":false},\"files\":[\"src/unrelated.ts\"]}";
    fs::write(root.join("tsconfig.json"), config).unwrap();
    fs::write(root.join("src/unrelated.ts"), "export {};\n").unwrap();
    for valid in [true, false] {
        generate_sdk_files(&options).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("tsconfig.json")).unwrap(),
            config
        );
        fs::write(
            root.join("src/index.ts"),
            if valid {
                "export function run():string{return 'hello';}"
            } else {
                "export function unrelated():string{return 'hello';}"
            },
        )
        .unwrap();
        let (success, diagnostics) = tsc_check(root, &["-p", ".perry/types"]);
        assert_eq!(success, valid, "{diagnostics}");
    }
}

/// Ambiguous worlds and colliding implementation names fail before any file is written.
#[test]
fn cli_and_sdk_reject_invalid_worlds_before_writing_files() {
    for (wit, entry, source, message) in [
        (
            "package test:ambiguous; world first {export run:func();} world second {export run:func();}",
            "src/index.ts",
            "export function run():void{} ",
            "world",
        ),
        (
            "package test:collision; interface api {read:func();} world task {export api; export api-read:func(); export test-collision-api-read:func();}",
            "entry.ts",
            "export function apiRead():void{} export function testCollisionApiRead():void{}",
            "both require TypeScript implementation",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("wit")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("wit/world.wit"), wit).unwrap();
        fs::write(root.join(entry), source).unwrap();
        for arguments in [
            vec!["gen-types", "--no-tsconfig"],
            vec![entry, "-o", "out.wasm"],
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_perry-wit"))
                .current_dir(root)
                .args(arguments)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success());
            assert!(stderr.contains(message), "{stderr}");
            for written in [".perry", "out.wasm", "tsconfig.json"] {
                assert!(!root.join(written).exists(), "{written}");
            }
        }
    }
}

#[test]
fn cli_generates_a_checked_sdk_with_default_relative_paths() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
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
        .current_dir(root)
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
    let (success, diagnostics) = tsc_check(root, &[]);
    assert!(success, "{diagnostics}");
}

#[test]
fn profiling_names_are_optional_in_cli_components() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::create_dir(root.join("wit")).unwrap();
    fs::write(
        root.join("wit/world.wit"),
        "package test:profile; world task {export run:func(input:string)->string;}",
    )
    .unwrap();
    fs::write(
        root.join("entry.ts"),
        "export function run(input:string):string{return input + '!';}",
    )
    .unwrap();
    for keep_names in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_perry-wit"));
        command
            .current_dir(root)
            .args(["entry.ts", "-o", "out.wasm"]);
        if keep_names {
            command.arg("--keep-names");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let wasm = fs::read(root.join("out.wasm")).unwrap();
        let functions = crate::world::function_names(&wasm).unwrap();
        if keep_names {
            assert!(functions.contains(&"run"), "{functions:?}");
            assert!(functions.contains(&"perry_string_concat"), "{functions:?}");
        } else {
            assert!(functions.is_empty());
        }
    }
}

#[test]
fn cli_help_and_diagnostics_are_aligned_and_deduplicated() {
    let bin = env!("CARGO_BIN_EXE_perry-wit");
    for args in [&["--help"][..], &["gen-types", "--help"][..]] {
        let out = Command::new(bin).args(args).output().unwrap();
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("\nOptions:\n"), "{stdout}");
        assert!(
            !stdout.lines().any(|line| line.starts_with("        ")),
            "{stdout}"
        );
    }
    for (args, flag) in [
        (&["--world"][..], "--world"),
        (&["--out="][..], "--out"),
        (&["gen-types", "--wit"][..], "--wit"),
        (&["gen-types", "--entry="][..], "--entry"),
    ] {
        let out = Command::new(bin).args(args).output().unwrap();
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains(&format!("Option '{flag}' requires a value")),
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let run_gen = || {
        Command::new(bin)
            .current_dir(dir.path())
            .arg("gen-types")
            .output()
            .unwrap()
    };
    let missing_wit = String::from_utf8_lossy(&run_gen().stderr).into_owned();
    assert!(
        missing_wit.contains("WIT directory 'wit' does not exist"),
        "{missing_wit}"
    );
    assert_eq!(
        missing_wit.matches("WitResolution:").count(),
        1,
        "{missing_wit}"
    );

    fs::create_dir(dir.path().join("wit")).unwrap();
    fs::write(
        dir.path().join("wit/world.wit"),
        "world app { export run: func(); }",
    )
    .unwrap();
    let missing_pkg = String::from_utf8_lossy(&run_gen().stderr).into_owned();
    assert_eq!(
        missing_pkg
            .matches("no `package` header was found in any WIT file for this package")
            .count(),
        1,
        "{missing_pkg}"
    );
    assert_eq!(
        missing_pkg.matches("WitResolution:").count(),
        1,
        "{missing_pkg}"
    );
    assert!(
        missing_pkg.contains("help: Add a package header"),
        "{missing_pkg}"
    );
}
