mod support;

#[test]
fn unsupported_record_parameters_and_results_fail_with_explicit_diagnostics() {
    use perry_wit::compiler::{CompileOptions, compile_typescript};
    for signature in ["func(input: payload) -> string", "func() -> payload"] {
        let scratch = support::Scratch::new();
        let wit_dir = scratch.0.join("wit");
        std::fs::create_dir_all(&wit_dir).unwrap();
        std::fs::write(wit_dir.join("world.wit"), format!("package test:abi; world test {{ {RUNTIME_IMPORTS} record payload {{ title: string, count: u32 }} export run-task: {signature}; }}")).unwrap();
        let error = compile_typescript(
            "export function runTask(input: any): any { return input; }",
            "record.ts",
            &CompileOptions {
                wit_dir,
                world: Some("test".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("export 'run-task'"), "{diagnostic}");
        assert!(
            diagnostic.contains("WIT record 'payload' is unsupported"),
            "{diagnostic}"
        );
    }
}

#[test]
fn string_results_export_the_selected_branch_and_payload() {
    let wit = format!(
        "package test:abi; world test {{ {RUNTIME_IMPORTS} export run-task: func() -> result<string, string>; }}"
    );
    for (expression, expected) in [
        (r#"{ ok: true, value: "done" }"#, r#"ok("done")"#),
        (r#"{ ok: false, error: "failed" }"#, r#"err("failed")"#),
        (r#"{ ok: true, value: "" }"#, r#"ok("")"#),
        (r#"{ ok: false, error: "" }"#, r#"err("")"#),
    ] {
        let output = support::run(
            &format!("export function runTask(): any {{ return {expression}; }}"),
            Some(&wit),
            Some("run-task()"),
        );
        assert_eq!(support::stdout(&output).trim(), expected);
    }
}

const RUNTIME_IMPORTS: &str = r#"
    import wasi:cli/stdout@0.2.6;
    import wasi:cli/stderr@0.2.6;
    import wasi:cli/exit@0.2.6;
    import wasi:http/outgoing-handler@0.2.6;
    import wasi:http/types@0.2.6;
    import wasi:io/poll@0.2.6;
    import wasi:io/streams@0.2.6;
"#;

#[test]
fn void_exports_accept_targets_with_and_without_core_results() {
    let wit = format!(
        "package test:abi; world test {{ {RUNTIME_IMPORTS} export run-task: func(input: string); }}"
    );
    for source in [
        "export function runTask(input: string): void { console.log(input); }",
        "export function runTask(input: string): number { console.log(input); return 7; }",
    ] {
        let output = support::run(source, Some(&wit), Some("run-task(\"void works\")"));
        assert_eq!(support::stdout(&output).trim(), "void works\n()");
    }
}

#[test]
fn scalar_results_have_typed_core_signatures_and_no_buffer_cleanup() {
    for (ty, value) in [
        ("s8", "-128"),
        ("s16", "-32768"),
        ("s32", "7"),
        ("u8", "255"),
        ("u16", "65535"),
        ("u32", "4294967295"),
        ("s64", "-4294967296"),
        ("u64", "4294967296"),
        ("f32", "1.5"),
        ("f64", "-2.25"),
        ("bool", "true"),
        ("bool", "false"),
    ] {
        let wit = format!(
            "package test:abi; world test {{ {RUNTIME_IMPORTS} export run-task: func() -> {ty}; }}"
        );
        let output = support::run(
            &format!("export function runTask(): any {{ return {value}; }}"),
            Some(&wit),
            Some("run-task()"),
        );
        assert_eq!(support::stdout(&output).trim(), value, "{ty}");
    }
}

#[test]
fn scalar_arguments_preserve_numeric_bits_signedness_and_boolean_tags() {
    for (ty, input, expected) in [
        ("s8", "-128", "-128"),
        ("s16", "-32768", "-32768"),
        ("s32", "-2147483648", "-2147483648"),
        ("u8", "255", "255"),
        ("u16", "65535", "65535"),
        ("u32", "4294967295", "4294967295"),
        ("s64", "-4294967296", "-4294967296"),
        ("u64", "4294967296", "4294967296"),
        ("f32", "1.5", "1.5"),
        ("f64", "-2.25", "-2.25"),
        ("bool", "true", "true"),
        ("bool", "false", "false"),
    ] {
        let wit = format!(
            "package test:abi; world test {{ {RUNTIME_IMPORTS} export run-task: func(input: {ty}) -> string; }}"
        );
        let output = support::run(
            "export function runTask(input: any): string { return 'n=' + input; }",
            Some(&wit),
            Some(&format!("run-task({input})")),
        );
        assert_eq!(
            support::stdout(&output).trim(),
            format!("\"n={expected}\""),
            "{ty}"
        );
    }
}
