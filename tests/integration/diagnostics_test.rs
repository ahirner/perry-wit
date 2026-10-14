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
    api::compile(
        &program(
            "export async function run():Promise<number> {try{throw new Error('handled');}catch(e){return 7;}}",
        ),
        &bare_wit,
        &Options::default(),
    )
    .unwrap();
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
