use perry_wit::{
    DiagnosticCode,
    api::{self, Module, Options, Program, Source, Wit},
};

#[test]
fn diagnostics_retain_declarations_and_original_module_coordinates() {
    let helper = "// é🙂\r\nexport function helper(value) { return value; }";
    let program = Program {
        source: Source { name: Some("src/main.ts".into()), contents: b"import { helper } from './helper'; export function run():number{return helper(1);}".to_vec() },
        modules: vec![Module { name: "src/helper.ts".into(), contents: helper.as_bytes().to_vec() }],
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
}

#[test]
fn parse_and_required_field_errors_have_typed_codes() {
    for (source, code) in [
        ("export function {", DiagnosticCode::ParseError),
        (
            "interface Item { value:number|null } export function run():number { const item:Item={};return 0; }",
            DiagnosticCode::MissingRequiredField,
        ),
    ] {
        let error = api::compile_core(&Program {
            source: source.as_bytes().into(),
            modules: vec![],
        })
        .unwrap_err();
        assert_eq!(error.diagnostics[0].code, code, "{error}");
        assert!(error.diagnostics[0].span.is_some(), "{error}");
    }
}

#[test]
fn throwing_exports_require_a_declared_channel_but_catches_do_not() {
    let wit = Wit {
        application: b"package test:diagnostics; world app { export run: async func() -> f64; }"
            .as_slice()
            .into(),
        dependencies: vec![],
    };
    let program = |source: &str| Program {
        source: source.as_bytes().into(),
        modules: vec![],
    };
    let error = api::compile(
        &program("export async function run():Promise<number> {throw new Error('broken');}"),
        &wit,
        &Options::default(),
    )
    .unwrap_err();
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
    for source in [
        "export async function run():Promise<number> {try{throw new Error('handled');}catch(e){return 7;}}",
        "async function value(n:number):Promise<number>{return n+1;} export async function run():Promise<number>{return await value(6);}",
        "async function value():Promise<number>{throw 'handled';} export async function run():Promise<number>{try{return await value();}catch{return 7;}}",
        "export async function run():Promise<number>{const holder:{jobs:Promise<number>[]}={jobs:[Promise.resolve(7)]};const job:Promise<number>=holder.jobs[0];return await job;}",
    ] {
        api::compile(&program(source), &wit, &Options::default()).unwrap();
    }
    let error = api::compile(
        &program("async function value():Promise<number>{throw 'unhandled';} export async function run():Promise<number>{const values=await Promise.all([value()]);return values[0];}"),
        &wit,
        &Options::default(),
    ).unwrap_err();
    assert_eq!(
        error.diagnostics[0].code,
        DiagnosticCode::MissingErrorChannel
    );
}

#[test]
fn unannotated_helpers_infer_returns_and_dependencies() {
    let source = "function words(){return ['a'];} function joined(){return words().concat('b');} function start(){return result();} function result(){return 7;} function loop(){for(let value=start();value>0;value--)return value;return 0;} export function run(){return joined().length+loop();}";
    api::compile_core(&Program {
        source: source.as_bytes().into(),
        modules: vec![],
    })
    .unwrap();
}

#[test]
fn async_exports_accept_declared_stderr_support() {
    let source = "export async function run():Promise<number>{throw new Error('broken');}";
    let program = Program {
        source: source.as_bytes().into(),
        modules: vec![],
    };
    let wit = Wit {application:b"package test:stderr; world app {import wasi:cli/stderr@0.3.0; export run:async func()->f64;}".as_slice().into(), dependencies:vec![]};
    api::compile(&program, &wit, &Options::default()).unwrap();
}

#[test]
fn inference_accounts_for_fallthrough_bare_returns_and_async_helpers() {
    let source = "function maybe(flag:boolean){if(flag)return 7;} function bare(flag:boolean){if(flag)return;return 'value';} async function helper(){return maybe(false);} export async function run():Promise<number>{const value=await helper();return value===undefined && bare(true)===undefined ? 0 : 1;}";
    let mut program = crate::core_program::Program::checked(source, "inferred.ts");
    assert_eq!(program.outcome("run", &[]), (0.0, 0));
}

#[test]
fn recoverable_parser_errors_use_exclusive_original_byte_ranges() {
    let source = "// é🙂\r\nexport const missing;";
    let program = Program {
        source: Source {
            name: Some("parse.ts".into()),
            contents: source.as_bytes().to_vec(),
        },
        modules: vec![],
    };
    let error = api::compile_core(&program).unwrap_err();
    assert_eq!(error.diagnostics[0].code, DiagnosticCode::ParseError);
    let span = error.diagnostics[0].span.as_ref().unwrap();
    assert_eq!(&source[span.start_byte..span.end_byte], "missing");
    assert_eq!((span.start_line, span.start_column), (2, 14));
}
