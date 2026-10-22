use perry_wit::{
    DiagnosticCode, SdkOptions,
    api::{self, Module, Options, Program, Source, Wit},
    generate_sdk_files,
};

#[test]
fn diagnostics_locate_imported_modules_and_multibyte_columns() {
    let helper = "// é🙂\r\nexport function helper(value) { return value; }";
    let program = Program {
        source: Source {
            name: Some("src/main.ts".into()),
            contents: b"import { helper } from './helper'; export function run():number{return helper(1);}".to_vec(),
        },
        modules: vec![Module {
            name: "src/helper.ts".into(),
            contents: helper.as_bytes().to_vec(),
        }],
    };
    let error = api::compile_core(&program).unwrap_err();
    let diagnostic = &error.diagnostics[0];
    assert_eq!(diagnostic.code, DiagnosticCode::MissingTypeAnnotation);
    let span = diagnostic.span.as_ref().expect("declaration span");
    assert_eq!(span.file.to_string_lossy(), "src/helper.ts");
    assert_eq!(span.start_line, 2);
    assert!(helper[span.start_byte..span.end_byte].contains("value"));
    assert!(diagnostic.help.is_some());
    assert!(std::error::Error::source(&error).is_some());

    let helper =
        "// é🙂\r\nexport function helper(value:number):number {\r\n return value << 1;\r\n}";
    let mut program = program;
    program.modules[0].contents = helper.as_bytes().to_vec();
    let error = api::compile_core(&program).unwrap_err();
    assert_eq!(
        error.diagnostics[0].code,
        DiagnosticCode::UnsupportedOperator
    );
    let span = error.diagnostics[0].span.as_ref().unwrap();
    assert_eq!(span.file.to_string_lossy(), "src/helper.ts");
    assert_eq!(&helper[span.start_byte..span.end_byte], "value << 1");
    assert_eq!((span.start_line, span.start_column), (3, 9));

    let parse_source = "// é🙂\r\n// cr\r// ls\u{2028}// ps\u{2029}export const missing;";
    let parse_error = api::compile_core(&Program {
        source: Source {
            name: Some("parse.ts".into()),
            contents: parse_source.as_bytes().to_vec(),
        },
        modules: vec![],
    })
    .unwrap_err();
    assert_eq!(parse_error.diagnostics[0].code, DiagnosticCode::ParseError);
    assert_eq!(
        perry_wit::compile_core(parse_source, "parse.ts")
            .unwrap_err()
            .diagnostics,
        parse_error.diagnostics
    );
    let span = parse_error.diagnostics[0].span.as_ref().unwrap();
    assert_eq!(&parse_source[span.start_byte..span.end_byte], "missing");
    assert_eq!((span.start_line, span.start_column), (5, 14));

    let temp = tempfile::tempdir().unwrap();
    let wit_dir = temp.path().join("wit");
    std::fs::create_dir_all(&wit_dir).unwrap();
    std::fs::write(wit_dir.join("world.wit"), "not valid wit").unwrap();
    let wit_err = generate_sdk_files(&SdkOptions {
        wit_dir: wit_dir.clone(),
        out_dir: temp.path().join("out"),
        ..SdkOptions::default()
    })
    .unwrap_err();
    assert_eq!(wit_err.diagnostics[0].code, DiagnosticCode::WitResolution);

    std::fs::write(
        wit_dir.join("world.wit"),
        "package test:sdk; world app { export run: func() -> f64; }",
    )
    .unwrap();
    let blocker = temp.path().join("blocker");
    std::fs::write(&blocker, "file").unwrap();
    let io_err = generate_sdk_files(&SdkOptions {
        wit_dir,
        out_dir: blocker.join("types"),
        ..SdkOptions::default()
    })
    .unwrap_err();
    assert_eq!(io_err.diagnostics[0].code, DiagnosticCode::Io);
}

#[test]
fn parse_and_required_field_errors_have_typed_codes() {
    for (source, code) in [
        ("export function {", DiagnosticCode::ParseError),
        (
            "interface Item { value:number|null } export function run():number { const item:Item={};return 0; }",
            DiagnosticCode::MissingRequiredField,
        ),
        (
            "export async function mergeDocs():Promise<Map<string,string>> { const base=JSON.parse('{\"y\":\"v\"}'); return { \"x\": base.y }; }",
            DiagnosticCode::UnsupportedType,
        ),
        (
            "export async function mergeDocs():Promise<Map<string,string>> { const [base, update]=[JSON.parse('{}'), JSON.parse('{}')]; return { ...base, ...update }; }",
            DiagnosticCode::UnsupportedType,
        ),
    ] {
        let error = api::compile_core(&Program {
            source: source.as_bytes().into(),
            modules: vec![],
        })
        .unwrap_err();
        assert_eq!(error.diagnostics[0].code, code, "{error}");
        assert!(error.diagnostics[0].span.is_some(), "{error}");
        assert!(!error.diagnostics[0].message.contains("_0"), "{error}");
    }
}

/// Unsupported source forms are rejected with their exact message.
#[test]
fn unsupported_forms_have_exact_diagnostics() {
    let regex = |expression: &str| {
        format!(
            "export function test(input: string): boolean {{ return ({expression}).test(input); }}"
        )
    };
    let iteration = |expression: &str| {
        format!("export function run():number {{for(const value of {expression}){{}} return 0;}}")
    };
    const IN_RUN: &str = "\n  lowering function run";
    const IN_F: &str = "\n  lowering function f";
    const IN_TEST: &str = "\n  lowering function test";
    const REGEX_CONST: &str =
        "CodegenFailure: lowering function test to wasm\n  lowering perry.regex_const\n  ";
    let cases = [
        (iteration("new Headers()"), format!("case.ts:1:8: UnsupportedExpression: synchronous for...of requires a dense typed array or string; this receiver has no supported iterator{IN_RUN}")),
        (iteration("new URLSearchParams()"), format!("case.ts:1:8: UnsupportedExpression: synchronous for...of requires a dense typed array or string; URLSearchParams iteration is unsupported{IN_RUN}")),
        ("export function run(): string { return 'a'.replaceAll(/a/, 'b'); }".into(), format!("case.ts:1:40: UnsupportedExpression: String.prototype.replaceAll called with a non-global RegExp argument{IN_RUN}")),
        ("class A {} class B extends A {} export function run(): number { return 0; }".into(), "UnsupportedExpression: class B extends A: class inheritance is not supported yet".into()),
        (regex("/a/g"), format!("case.ts:1:55: UnsupportedExpression: Regex search supports only the u and s flags{IN_TEST}")),
        (regex("/\\u{61}/"), format!("{REGEX_CONST}Braced Unicode regex escapes require the u flag")),
        (regex("/(?=a)/"), format!("{REGEX_CONST}Regex lookaround and inline flags are unsupported")),
        (regex("/(a)\\1/"), format!("{REGEX_CONST}Unsupported regex escape: \\1")),
        (regex("/(a)(b)(c)(d)(e)(f)(g)(h)(i)/"), format!("{REGEX_CONST}Regex has 9 capture groups; at most 8 are supported")),
        (regex("/(a{300})/"), format!("{REGEX_CONST}Regex capture program exceeds 256 states")),
        (regex("(input === 'x' ? /a/g : /a/)"), format!("case.ts:1:55: UnsupportedExpression: regex flags must be statically known; use a regex literal or a local assigned only literals with the same g flag{IN_TEST}")),
        (regex("/\\uD800/u"), format!("{REGEX_CONST}Unpaired surrogate in regex escape")),
        (regex("new RegExp(input)"), format!("case.ts:1:55: UnsupportedExpression: RegExp construction is unsupported; use a bounded regex literal{IN_TEST}")),
        ("export function test(): boolean { return /a/ === /a/; }".into(), format!("case.ts:1:8: UnsupportedExpression: regex object identity is outside the literal matching profile{IN_TEST}")),
        ("export function run(x: number): number { return x ** (2 ** x); }".into(), format!("case.ts:1:55: UnsupportedOperator: MIR backend does not support operator Pow yet{IN_RUN}")),
        ("function* count(): Generator<number> { yield 1; } export function run(x: number): number { return x; }".into(), "case.ts:1:1: UnsupportedExpression: generator function count is not supported yet".into()),
        ("export function run(x: number): number { const d: WeakMap<object, number> = null as any; return x; }".into(), format!("case.ts:1:48: UnsupportedExpression: MIR backend does not support unresolved generic type WeakMap{IN_RUN}")),
        ("export function run(cb: number): number {\n  return (cb as any)(1);\n}".into(), format!("case.ts:2:10: UnsupportedExpression: indirect calls through local or parameter bindings are not supported yet; call a declared module function directly{IN_RUN}")),
        ("export function run(s: string): number {\n  return (s as any).byteLength;\n}".into(), format!("case.ts:2:10: UnsupportedExpression: property byteLength is not supported on string yet{IN_RUN}")),
        ("export function run(a: bigint, b: bigint): bigint {\n  return a ** b;\n}".into(), format!("case.ts:2:10: UnsupportedExpression: operator Pow on bigint and bigint is not supported yet{IN_RUN}")),
        ("export function f(): string { return Temporal.ZonedDateTime.from('2024-01-01T00:00:00[UTC]').toString(); }".into(), format!("case.ts:1:38: UnsupportedExpression: Temporal.ZonedDateTime.from is not supported; use Instant.from, Instant.fromEpochMilliseconds, PlainDate.from, PlainDate.compare, PlainDateTime.from or Temporal.Now{IN_F}")),
        ("export function f(s: string): string { return Temporal.PlainDate.from(s).add({ hours: 1 }).toString(); }".into(), format!("case.ts:1:47: UnsupportedExpression: Temporal add duration field hours is not supported; use years, months, weeks, or days{IN_F}")),
        ("export function f(s: string): number { return Temporal.PlainDate.from(s).hour; }".into(), format!("case.ts:1:47: UnsupportedExpression: property hour is not supported on Temporal.PlainDate yet{IN_F}")),
    ];
    let mismatches: Vec<String> = cases
        .iter()
        .filter_map(|(source, expected)| {
            let actual = match perry_wit::compile_core(source, "case.ts") {
                Ok(_) => "<compiled>".to_owned(),
                Err(error) => format!("{error:#}"),
            };
            (actual != *expected).then(|| format!("{source}\n  => {actual:?}"))
        })
        .collect();
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    perry_wit::compile_core(
        "class Unused { power():number { return 2 ** 3; } } export function run():number { return 0; }",
        "case.ts",
    ).expect("unused methods are not lowered to MIR");
}

#[test]
fn throwing_exports_require_a_declared_channel_but_catches_and_stderr_succeed() {
    let bare_wit = Wit {
        application: b"package test:diagnostics; world app { export run: async func() -> f64; }"
            .as_slice()
            .into(),
        dependencies: vec![],
    };
    let stderr_wit = Wit {
        application: b"package test:stderr; world app {import wasi:cli/stderr@0.3.0; export run:async func()->f64;}"
            .as_slice()
            .into(),
        dependencies: vec![],
    };
    let program = |source: &str| Program {
        source: source.as_bytes().into(),
        modules: vec![],
    };
    let throwing =
        program("export async function run():Promise<number> {throw new Error('broken');}");
    let error = api::compile(&throwing, &bare_wit, &Options::default()).unwrap_err();
    assert_eq!(
        error.diagnostics[0].code,
        DiagnosticCode::MissingErrorChannel
    );
    assert!(
        error.diagnostics[0]
            .help
            .as_ref()
            .unwrap()
            .contains("async")
    );
    api::compile(&throwing, &stderr_wit, &Options::default()).unwrap();
    for source in [
        "export async function run():Promise<number> {try{throw new Error('handled');}catch(e){return 7;}}",
        "async function value(n:number):Promise<number>{return n+1;} export async function run():Promise<number>{return await value(6);}",
        "async function value():Promise<number>{throw 'handled';} export async function run():Promise<number>{try{return await value();}catch{return 7;}}",
        "export async function run():Promise<number>{const holder:{jobs:Promise<number>[]}={jobs:[Promise.resolve(7)]};const job:Promise<number>=holder.jobs[0];return await job;}",
    ] {
        api::compile(&program(source), &bare_wit, &Options::default()).unwrap();
    }
    let error = api::compile(
        &program("async function value():Promise<number>{throw 'unhandled';} export async function run():Promise<number>{const values=await Promise.all([value()]);return values[0];}"),
        &bare_wit,
        &Options::default(),
    ).unwrap_err();
    assert_eq!(
        error.diagnostics[0].code,
        DiagnosticCode::MissingErrorChannel
    );
}

#[test]
fn inference_accounts_for_fallthrough_bare_returns_dependencies_and_async_helpers() {
    let source = r#"
function words(){return ['a'];}
function joined(){return words().concat('b');}
function start(){return result();}
function result(){return 7;}
function loop(){for(let value=start();value>0;value--)return value;return 0;}
function maybe(flag:boolean){if(flag)return 7;}
function bare(flag:boolean){if(flag)return;return 'value';}
async function helper(){return maybe(false);}
export async function run():Promise<number>{
    const value=await helper();
    return value===undefined && bare(true)===undefined && joined().length+loop()===9 ? 0 : 1;
}
"#;
    let mut program = crate::core_program::Program::checked(source, "inferred.ts");
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}

#[test]
fn export_and_import_mismatches_report_spans_and_actionable_help() {
    let wit = Wit {
        application: b"package test:mismatch;
interface host {
    ping: func(a: u32, b: u32) -> u32;
}
world app {
    import host;
    export run: async func(id: u64) -> u64;
}"
        .as_slice()
        .into(),
        dependencies: vec![],
    };
    let check = |source: &str, code, line, needle: &str| {
        let err = api::compile(
            &Program {
                source: Source {
                    name: Some("src/main.ts".into()),
                    contents: source.as_bytes().to_vec(),
                },
                modules: vec![],
            },
            &wit,
            &Options::default(),
        )
        .unwrap_err();
        let diag = &err.diagnostics[0];
        assert_eq!(diag.code, code, "{diag}");
        assert_eq!(diag.span.as_ref().unwrap().start_line, line, "{diag}");
        assert!(diag.to_string().contains(needle), "{diag}");
    };

    check(
        "// header\nfunction run(id: bigint): Promise<bigint> { return Promise.resolve(id); }",
        DiagnosticCode::ExportMismatch,
        2,
        "Add the 'export' keyword to function 'run'",
    );
    check(
        "// header\nexport async function run(id: number): Promise<bigint> { return 1n; }",
        DiagnosticCode::ExportMismatch,
        2,
        "bigint",
    );
    check(
        "// header\nexport function run(id: bigint): bigint { return id; }",
        DiagnosticCode::ExportMismatch,
        2,
        "Add 'async'",
    );
    check(
        "import { ping } from 'test:mismatch/host';\nexport async function run(id: bigint): Promise<bigint> {\n  const x = (ping as any)(1);\n  return id;\n}",
        DiagnosticCode::ImportSignatureMismatch,
        3,
        "expects 2 arguments, got 1",
    );
}
