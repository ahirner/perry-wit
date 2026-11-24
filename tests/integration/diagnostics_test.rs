use anyhow::Context as _;
use perry_wit::{
    CompileOptions, ErrorKind, SdkOptions,
    api::{self, Module, Options, Program, Source, Wit},
    codes, generate_sdk_files,
};

fn program_with(name: &str, source: &str, modules: &[(&str, &str)]) -> Program {
    Program {
        source: Source {
            name: Some(name.into()),
            contents: source.as_bytes().to_vec(),
        },
        modules: modules
            .iter()
            .map(|(name, contents)| Module {
                name: (*name).into(),
                contents: contents.as_bytes().to_vec(),
            })
            .collect(),
    }
}

#[test]
fn unannotated_parameters_need_evidence_from_statically_known_calls() -> anyhow::Result<()> {
    let local = "function inc(x) { return x + 1; } export function run(n:number):number { return inc(6) + inc(n); }";
    let typed = local.replace("inc(x)", "inc(x: number)");
    assert_eq!(
        perry_wit::compile_core(local, "local.ts")?,
        perry_wit::compile_core(&typed, "local.ts")?
    );
    let mixed = "function make<T>(x:T) { return {value:x}; } function get(x) { return x.value; } export function run(n:number):number { return get(make(n)) + get(make('wide')).length; }";
    let mut program = crate::core_program::Program::new(mixed);
    assert_eq!(program.number("run", 7.0), 11.0);
    let recursive = "function first(x) { return next(x); } function next(x:number):number { return x ? first(x-1) : 7; } export function run(n:number):number { return first(n); }";
    let mut program = crate::core_program::Program::new(recursive);
    assert_eq!(program.number("run", 4.0), 7.0);
    let callbacks = r#"
        function inc(x) { return x + 1; }
        function size(x) { return x.length; }
        export function run(n: number): number {
            const indexes = [[n], [2]].map(row => row.map(v => inc(v))[0]);
            const words: (string | null)[] = ["abc", null];
            const lengths = words.map(function(word, index, group: (string | null)[]) {
                if (word === null) return 0;
                const width = size(word);
                return width + inc(index + group.length);
            });
            return 100 * indexes[0] + 10 * indexes[1] + lengths[0] + lengths[1];
        }"#;
    let mut program = crate::core_program::Program::new(callbacks);
    assert_eq!(program.number("run", 4.0), 536.0);
    let self_recursive = "function loop(x) { return x ? loop(x-1) : 0; } export function run():number { return loop(3); }";
    let mut program = crate::core_program::Program::new(self_recursive);
    assert_eq!(program.call("run", &[]).0[0].unwrap_f64(), 0.0);
    for source in [
        "export function id(x) { return x; } export function run(): number { return id(1); }",
        "function id(x) { return x; } export function run():number { return [1].map(n => id(n, n))[0]; }",
        "function id(x) { return x; } export const other = {id}; export function run():number { return id(1); }",
        "function id(x) { return x; } export function run():number { return id(1,2) + id(1); }",
    ] {
        let error = perry_wit::compile_core(source, "closed.ts")
            .err()
            .context("expected an unresolved parameter")?;
        assert_eq!(
            error.primary().code(),
            codes::MISSING_TYPE_ANNOTATION,
            "{source}: {error}"
        );
    }
    Ok(())
}

#[test]
fn named_generic_functions_specialize_through_aliases_and_builtin_callbacks() -> anyhow::Result<()>
{
    for body in ["const f=double; return f(3);", "return [3].map(double)[0];"] {
        let source = format!(
            "function double<T>(x:T) {{return x*2;}} export function run():number {{ {body} }}"
        );
        let mut program = crate::core_program::Program::new(&source);
        assert_eq!(program.call("run", &[]).0[0].unwrap_f64(), 6.0);
    }
    let unused = "function id<T>(x:T):T {return x;} function unused():void {const f=id;} export function run(n:number):number {const f=id; return n;}";
    let mut program = crate::core_program::Program::new(unused);
    assert_eq!(program.number("run", 2.0), 2.0);
    let cast = "function double<T>(x:T) { return x*2; } export function run(n:number):number { return (double as (x:number)=>number)(n); }";
    let mut program = crate::core_program::Program::new(cast);
    assert_eq!(program.number("run", 3.0), 6.0);
    Ok(())
}

#[test]
fn function_arguments_check_declared_results_and_locate_escaping_values() -> anyhow::Result<()> {
    for (source, at, needle) in [
        (
            "function make() { return () => 1; } export function run(): boolean { const a = make(); const b = make(); const r = a === b; const x = 1; const y = 2; return r && x === y; }",
            "a === b",
            "capture-free closure literal",
        ),
        (
            "export function run(): boolean { const xs = []; for (let i = 0; i < 2; i++) xs.push(() => 1); return xs[0] === xs[1]; }",
            "xs[0] === xs[1]",
            "capture-free closure literal",
        ),
        (
            "export function run(n: number) { return n ? (() => 1) : 42; }",
            "n ? (() => 1) : 42",
            "joining function values with non-function values",
        ),
        (
            "export function run(): number { const o = { f: () => 1 }; o.f = () => 2; return o.f(); }",
            "o.f = () => 2",
            "reassigning function property 'f' on an object literal",
        ),
        (
            "export function run(): number { new PerformanceObserver(() => {}); return 1; }",
            "new PerformanceObserver",
            "requires the runtime to call a guest function",
        ),
        (
            "export function run(): number { new Promise((resolve) => resolve(Promise.resolve(1))); return 1; }",
            "resolve(Promise.resolve(1))",
            "resolving a Promise with another Promise is not supported yet",
        ),
        (
            "export const api = { async slowDouble(n: number) { return n * 2; } };",
            "slowDouble",
            "async method 'slowDouble' of an exported object literal",
        ),
        (
            "const m = new Map(); const alias = m; alias.set('k', () => 2);",
            "alias.set",
            "storing a function value with Map.set",
        ),
        (
            "export function run(): number { queueMicrotask(() => {}); return 1; }",
            "queueMicrotask(() => {})",
            "queueMicrotask is not supported",
        ),
        (
            "export function run(): number { process.nextTick(() => {}); return 1; }",
            "process.nextTick(() => {})",
            "process.nextTick is not supported",
        ),
        (
            "export function run(): number { process.on('exit', () => {}); return 1; }",
            "process.on('exit', () => {})",
            "passing a callback to runtime 'on' requires the runtime to call a guest function",
        ),
        (
            "export function run(): number { addEventListener('click', () => {}); return 1; }",
            "addEventListener('click', () => {})",
            "passing a callback to runtime 'addEventListener' requires the runtime to call a guest function",
        ),
    ] {
        let err = perry_wit::compile_core(source, "escape.ts").unwrap_err();
        let diagnostic = err.primary();
        assert_eq!(diagnostic.code(), codes::UNSUPPORTED_CONSTRUCT, "{err}");
        assert!(
            diagnostic.message().contains(needle),
            "expected '{needle}' in: {err}"
        );
        assert_eq!(
            diagnostic.span().context("diagnostic span")?.start_byte,
            source.rfind(at).unwrap(),
            "{err}"
        );
    }
    let non_callable = "export function run(): number { const x = 1; return x(); }";
    let err = perry_wit::compile_core(non_callable, "call.ts").unwrap_err();
    assert_eq!(err.primary().code(), codes::TYPE_MISMATCH, "{err}");
    assert_eq!(
        err.primary()
            .span()
            .context("non-callable span")?
            .start_byte,
        non_callable.rfind("x()").unwrap(),
        "{err}"
    );
    let async_mixed = "export function run(): number { const fs = [async () => 1, () => 2]; let n = 0; for (const f of fs) { f(); n++; } return n; }";
    let err = perry_wit::compile_core(async_mixed, "dispatch.ts").unwrap_err();
    assert_eq!(err.primary().code(), codes::UNSUPPORTED_TYPE, "{err}");
    assert_eq!(
        err.primary()
            .span()
            .context("dispatcher union span")?
            .start_byte,
        async_mixed.rfind("f()").unwrap(),
        "{err}"
    );
    let source = "function use(f:(n:number)=>number):number {return 1;} export function run():number {return use(2);}";
    let err = perry_wit::compile_core(source, "wrong.ts").unwrap_err();
    assert_eq!(err.primary().code(), codes::TYPE_MISMATCH, "{err}");
    assert_eq!(
        err.primary()
            .span()
            .context("non-callable argument")?
            .start_byte,
        source.find("use(2)").unwrap(),
        "{err}"
    );
    for target in [
        "function target(x:string):string {return x;}",
        "function target<T>(x:T):T {return x;}",
    ] {
        let source = format!(
            "function apply(f:(n:number)=>string):number {{return 1;}} {target} export function run():number {{ return apply(target); }}"
        );
        let err = perry_wit::compile_core(&source, "type.ts").unwrap_err();
        assert_eq!(err.primary().code(), codes::TYPE_MISMATCH, "{err}");
        assert_eq!(
            err.primary().span().context("argument use")?.start_byte,
            source.rfind("apply(target)").unwrap()
        );
    }
    Ok(())
}

#[test]
fn unresolved_parameter_patterns_keep_the_source_location() -> anyhow::Result<()> {
    for pattern in ["{ w, h }", "[first]"] {
        let source = format!("export function probe(n:number, {pattern}) {{ return n; }}");
        let error = perry_wit::compile_core(&source, "pattern.ts")
            .err()
            .context("expected an unannotated parameter pattern")?;
        let diagnostic = error.primary();
        assert_eq!(diagnostic.code(), codes::MISSING_TYPE_ANNOTATION, "{error}");
        let span = diagnostic.span().context("parameter pattern span")?;
        assert_eq!(source.get(span.start_byte..span.end_byte), Some(pattern));
        assert!(
            diagnostic
                .notes()
                .iter()
                .any(|note| note.message() == "in parameter 2 of 'probe'"),
            "{error}"
        );
        assert!(!error.to_string().contains("__"), "{error}");
    }
    Ok(())
}

#[test]
fn diagnostics_locate_imported_modules_and_multibyte_columns() -> anyhow::Result<()> {
    let helper = "// é🙂\r\nexport function helper(value) { return value; }";
    let mut program = program_with(
        "src/main.ts",
        "import { helper } from './helper'; export function run():number{return helper(1);}",
        &[("src/helper.ts", helper)],
    );
    program.source.contents.extend([b' '; 512]);
    let inferred = api::compile_core(&program).unwrap();
    let annotated = helper.replace("helper(value)", "helper(value: number)");
    program.modules[0].contents = annotated.into_bytes();
    assert_eq!(inferred, api::compile_core(&program).unwrap());
    program.modules[0].contents = helper.as_bytes().to_vec();
    let end = program.source.contents.len();
    program
        .source
        .contents
        .extend_from_slice(b"\nexport { helper };");
    let error = api::compile_core(&program).unwrap_err();
    program.source.contents.truncate(end);
    let diagnostic = error.primary();
    assert_eq!(diagnostic.code(), codes::MISSING_TYPE_ANNOTATION);
    let span = diagnostic.span().expect("declaration span");
    assert_eq!(span.file.to_string_lossy(), "src/helper.ts");
    assert_eq!(span.start_line, 2);
    assert!(helper[span.start_byte..span.end_byte].contains("value"));
    assert!(diagnostic.help().is_some());
    assert!(std::error::Error::source(&error).is_some());

    let helper =
        "// é🙂\r\nexport function helper(value:number):number {\r\n return value ** 1;\r\n}";
    program.modules[0].contents = helper.as_bytes().to_vec();
    let error = api::compile_core(&program).unwrap_err();
    assert_eq!(error.primary().code(), codes::UNSUPPORTED_OPERATOR);
    let span = error.primary().span().unwrap();
    assert_eq!(span.file.to_string_lossy(), "src/helper.ts");
    assert_eq!(&helper[span.start_byte..span.end_byte], "value ** 1");
    assert_eq!((span.start_line, span.start_column), (3, 9));

    for expression in [
        "(value as any)(1)",
        "(value as any).byteLength",
        "(value as any)[0]",
    ] {
        let helper = format!(
            "export function helper(value:number):number {{\r\n 'é🙂'; return {expression};\r\n}}"
        );
        program.modules[0].contents = helper.as_bytes().to_vec();
        let error = api::compile_core(&program).unwrap_err();
        assert_ne!(error.primary().kind(), ErrorKind::Internal, "{error}");
        let span = error.primary().span().unwrap();
        assert_eq!(span.file.to_string_lossy(), "src/helper.ts");
        assert_eq!(&helper[span.start_byte..span.end_byte], expression);
        assert_eq!((span.start_line, span.start_column), (2, 15));
    }

    let destructured = "export const { value: binding } = { value: 7 };";
    let error = api::compile_core(&program_with(
        "src/main.ts",
        "export { binding as run } from './helper';",
        &[("src/helper.ts", destructured)],
    ))
    .err()
    .context("expected a diagnostic for the re-exported constant")?;
    let diagnostic = error.primary();
    assert!(diagnostic.message().contains("Component export 'run'"));
    let span = diagnostic.span().context("destructured declaration span")?;
    assert_eq!(span.file.to_string_lossy(), "src/helper.ts");
    assert_eq!(
        destructured.get(span.start_byte..span.end_byte),
        Some("binding")
    );

    let parse_source = "// é🙂\r\n// cr\r// ls\u{2028}// ps\u{2029}export const missing;";
    let parse_error = api::compile_core(&program_with("parse.ts", parse_source, &[])).unwrap_err();
    assert_eq!(parse_error.primary().code(), codes::PARSE_ERROR);
    assert_eq!(
        perry_wit::compile_core(parse_source, "parse.ts")
            .unwrap_err()
            .primary(),
        parse_error.primary()
    );
    let span = parse_error.primary().span().unwrap();
    assert_eq!(&parse_source[span.start_byte..span.end_byte], "missing");
    assert_eq!((span.start_line, span.start_column), (5, 14));
    Ok(())
}

#[test]
fn sdk_errors_identify_the_failing_wit_or_output_path() {
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
    assert_eq!(wit_err.primary().code(), codes::WIT_PARSE);

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
    assert_eq!(io_err.primary().code(), codes::IO);

    // Multi-file WIT directory with one invalid-UTF-8 file preserves the failing filename in notes.
    let bad_wit_dir = temp.path().join("bad-wit");
    std::fs::create_dir_all(&bad_wit_dir).unwrap();
    std::fs::write(
        bad_wit_dir.join("good.wit"),
        "package test:multi; world app { export run: func() -> f64; }",
    )
    .unwrap();
    std::fs::write(bad_wit_dir.join("corrupt.wit"), [0xffu8, 0xfe]).unwrap();
    let bad_utf8_err = perry_wit::component::wit::resolve_wit(&bad_wit_dir).unwrap_err();
    assert_eq!(bad_utf8_err.primary().code(), codes::IO, "{bad_utf8_err}");
    assert!(
        bad_utf8_err.to_string().contains("corrupt.wit"),
        "{bad_utf8_err}"
    );
}

#[test]
fn generic_specialization_budget_is_per_function() -> anyhow::Result<()> {
    use anyhow::Context;
    use std::fmt::Write;
    let mut source = String::new();
    for id in 0..257 {
        writeln!(
            source,
            "function id{id}<T>(value: T): T {{ return value; }}"
        )?;
    }
    source.push_str("export function run(): number { let total = 0; ");
    for id in 0..257 {
        writeln!(source, "total += id{id}(1);")?;
    }
    source.push_str("return total; }");
    api::compile_core(&program_with("budget.ts", &source, &[]))?;

    // The first recursive call changes the key; the second reuses it.
    api::compile_core(&program_with(
        "finite.ts",
        "function again<T>(value:T, n:number):number { return n<=0 ? 0 : again([0], n-1); } export function run():number {return again(0, 2);}",
        &[],
    ))?;
    let mut finite = String::from(
        "function count<T>(v:T):number {return 1;} export function run():number {let n=0;",
    );
    for id in 0..257 {
        writeln!(finite, "n += count({{field{id}: 1}});")?;
    }
    finite.push_str("return n;}");
    for (source, function, grows) in [
        (finite.as_str(), "count", false),
        (
            "function grow<T>(value:T):number {return grow([value]);} export function run():number {return grow(1);}",
            "grow",
            true,
        ),
        (
            "function grow<T,F>(f:F,x:T):number { return grow(f,[x]); } function inc(n:number):number {return n+1;} export function run():number { return grow(inc,1); }",
            "grow",
            true,
        ),
    ] {
        let error = api::compile_core(&program_with("budget.ts", source, &[]))
            .err()
            .context("expected the per-function specialization budget to be exhausted")?;
        assert_eq!(error.primary().code(), codes::UNSUPPORTED_CONSTRUCT);
        assert!(
            error.to_string().contains(&format!(
                "256 distinct argument lists reached for {function}"
            )),
            "{error}"
        );
        assert_eq!(
            error
                .primary()
                .notes()
                .iter()
                .any(|note| note.message().contains("grows type argument T")),
            grows,
            "{error}"
        );
    }
    Ok(())
}

#[test]
fn imported_helpers_report_the_call_chain_across_compiler_phases() {
    const WORKFLOW: &str = "src/work\x1e\x1fflow.ts";
    let assert_caller_note =
        |error: &perry_wit::CompileError, expected_file: &str, needle: &str| {
            let diag = error.primary();
            let primary = diag.span().expect("primary span in library");
            assert_eq!(primary.file.to_string_lossy(), expected_file, "{diag}");
            assert!(!diag.to_string().contains("__perry_module_"), "{diag}");
            assert!(!diag.to_string().contains("_instance_"), "{diag}");
            assert!(
                diag.notes().iter().any(|note| {
                    note.span()
                        .is_some_and(|span| span.file == std::path::Path::new(WORKFLOW))
                        && note.message().contains(needle)
                }),
                "expected a caller note containing {needle:?} in {WORKFLOW:?}: {diag}"
            );
        };

    for (lib_source, workflow_source, expected_code, needle) in [
        (
            "export function helper(n: number): number {\n  return n ** 2;\n}",
            "import { helper } from './lib';\nexport function run(n: number): number {\n  return helper(n);\n}",
            codes::UNSUPPORTED_OPERATOR,
            "helper",
        ),
        (
            "export function helper(values: number[] | null): number {\n  return values.length;\n}",
            "import { helper } from './lib';\nexport function run(values: number[] | null): number {\n  return helper(values);\n}",
            codes::POSSIBLY_NULL_RECEIVER,
            "helper",
        ),
        (
            "export function helper(input: string, n: number): boolean {\n  if (n > 0) return helper(input, n - 1);\n  return /(/.test(input);\n}",
            "import { helper } from './lib';\nexport function run(input: string): boolean {\n  return helper(input, 1);\n}",
            codes::UNSUPPORTED_REGEX,
            "helper",
        ),
        (
            "export function wrap<T>(value: T): number {\n  return value;\n}",
            "import { wrap } from './lib';\nexport function run(): number {\n  return wrap('bad');\n}",
            codes::TYPE_MISMATCH,
            "while instantiating 'wrap'",
        ),
        (
            "function missing<T>(): T {throw new Error('missing');}\nexport function middle<T>(x:T) {return missing();}",
            "import { middle } from './lib';\nexport function run(): number {return middle(1);}",
            codes::MISSING_TYPE_ANNOTATION,
            "while instantiating 'middle'",
        ),
        (
            "export class Box<T> {\n  constructor(public value: T) {}\n  asNumber(): number {\n    return this.value;\n  }\n}",
            "import { Box } from './lib';\nexport function run(): number {\n  return new Box('bad').asNumber();\n}",
            codes::TYPE_MISMATCH,
            "called from here",
        ),
    ] {
        let err = api::compile_core(&program_with(
            WORKFLOW,
            workflow_source,
            &[("src/lib.ts", lib_source)],
        ))
        .unwrap_err();
        assert_eq!(err.primary().code(), expected_code, "{err}");
        assert_caller_note(&err, "src/lib.ts", needle);
    }

    // Nested chain: workflow.ts -> generic mid<T> in mid.ts -> helper leaf in leaf.ts.
    let chain_err = api::compile_core(&program_with(
        WORKFLOW,
        "import { mid } from './mid';\nexport function run(): number {\n  return mid(1);\n}",
        &[
            (
                "src/mid.ts",
                "import { leaf } from './leaf';\nexport function mid<T>(x: T): number {\n  return leaf(1);\n}",
            ),
            (
                "src/leaf.ts",
                "export function leaf(n: number): number {\n  return n ** 2;\n}",
            ),
        ],
    ))
    .unwrap_err();
    assert_eq!(
        chain_err.primary().code(),
        codes::UNSUPPORTED_OPERATOR,
        "{chain_err}"
    );
    assert_caller_note(&chain_err, "src/leaf.ts", "while instantiating 'mid'");
    assert!(
        chain_err.primary().notes().iter().any(|note| {
            note.span()
                .is_some_and(|span| span.file.to_string_lossy() == "src/mid.ts")
        }),
        "expected intermediate note in src/mid.ts:\n{chain_err}"
    );
}

#[test]
fn generic_property_reads_require_a_live_argument_guard() -> anyhow::Result<()> {
    use anyhow::Context;
    for body in [
        "return size(message);",
        "if (message !== null) { message = null; return size(message); } return 0;",
        "if (message === null) { return 0; } else { message = null; } return size(message);",
        "const reset = (): void => { message = null; }; if (message !== null) { reset(); return size(message); } return 0;",
        "if (message !== undefined) return size(message); return 0;",
        "return [message].map(v => { if (v === null) return 0; v = null; return size(v); })[0];",
        "return ['abc'].map((v, i, group: (string | null)[]) => size(group[0]))[0];",
    ] {
        let source = format!(
            "function size<T>(value:T):number {{return value.length;}} export function run(message:string|null):number {{ {body} }}"
        );
        let error = api::compile_core(&program_with("guard.ts", &source, &[]))
            .err()
            .with_context(|| format!("expected a nullable-property error for {body}"))?;
        assert_eq!(
            error.primary().code(),
            codes::POSSIBLY_NULL_RECEIVER,
            "{body}: {error}"
        );
    }
    Ok(())
}

/// MIR narrowing diagnostics reach the API with the span of the unproven read; the accepted
/// and rejected control flow is covered by `perry-lower`'s tests.
#[test]
fn nullable_property_access_requires_narrowing() {
    let wit = Wit {
        application: b"package example:nullable; world demo { export count: func(values: option<list<u32>>) -> u32; }".as_slice().into(),
        dependencies: vec![],
    };
    let source = "export function count(values: number[] | null): number {\n  if (values !== null) { const count = values.length; } return values.length;\n}";
    let error = api::compile(
        &program_with("main.ts", source, &[]),
        &wit,
        &Options {
            world: Some("demo".into()),
            ..Options::default()
        },
    )
    .expect_err("nullable access must be rejected");
    let diagnostic = error.primary();
    assert_eq!(diagnostic.code(), codes::POSSIBLY_NULL_RECEIVER, "{error}");
    assert_eq!(
        diagnostic.message(),
        "property access receiver is possibly null",
        "{error}"
    );
    assert!(diagnostic.notes().is_empty(), "{error}");
    let span = diagnostic.span().expect("property access span");
    assert_eq!(span.file.to_string_lossy(), "main.ts");
    assert_eq!(span.start_byte, source.rfind("values.length").unwrap());
    assert_eq!(&source[span.start_byte..span.end_byte], "values.length");
}

#[test]
fn type_errors_preserve_source_spans_and_field_names() {
    for (source, code) in [
        (
            "interface Item { value:number|null } export function run():number { const item:Item={};return 0; }",
            codes::MISSING_REQUIRED_FIELD,
        ),
        (
            "export async function mergeDocs():Promise<Map<string,string>> { const base=JSON.parse('{\"y\":\"v\"}'); return { \"x\": base.y }; }",
            codes::TYPE_MISMATCH,
        ),
        (
            "export async function mergeDocs():Promise<Map<string,string>> { const [base, update]=[JSON.parse('{}'), JSON.parse('{}')]; return { ...base, ...update }; }",
            codes::TYPE_MISMATCH,
        ),
    ] {
        let error = api::compile_core(&Program {
            source: source.as_bytes().into(),
            modules: vec![],
        })
        .unwrap_err();
        assert_eq!(error.primary().code(), code, "{error}");
        assert!(error.primary().span().is_some(), "{error}");
        assert!(!error.primary().message().contains("_0"), "{error}");
    }

    for source in [
        "export function run(): string { return atob(); }",
        "export function run(): string { return new URL().href; }",
    ] {
        let error = perry_wit::compile_core(source, "args.ts").unwrap_err();
        assert_eq!(error.primary().kind(), ErrorKind::TypeCheck, "{error}");
        assert_eq!(error.primary().code(), codes::TYPE_MISMATCH, "{error}");
    }

    // Argument index, record field, and shape field details survive on user-facing diagnostics.
    let call_err = perry_wit::compile_core(
        "function check(a: number, b: number): number { return a + b; }\nexport function run(): number { return check(1, 'x' as any as string); }",
        "call.ts",
    )
    .unwrap_err();
    assert_eq!(
        call_err.primary().code(),
        codes::TYPE_MISMATCH,
        "{call_err}"
    );
    assert!(
        call_err.to_string().contains("argument 2 of 'check'"),
        "{call_err}"
    );

    let record_err = perry_wit::compile_core(
        "interface Pair { a: number; b: number }\nexport function run(): number { const p: Pair = { a: 1, b: 'x' }; return p.a; }",
        "record.ts",
    )
    .unwrap_err();
    assert_eq!(
        record_err.primary().code(),
        codes::TYPE_MISMATCH,
        "{record_err}"
    );
    assert!(
        record_err.to_string().contains("record field 'b'"),
        "{record_err}"
    );

    let shape_err = perry_wit::compile_core(
        "interface Bad { count: WeakMap<object, number> }\nexport function run(x: Bad): number { return 0; }",
        "shape.ts",
    )
    .unwrap_err();
    assert_eq!(
        shape_err.primary().code(),
        codes::UNSUPPORTED_TYPE,
        "{shape_err}"
    );
    assert!(
        shape_err.to_string().contains("field 'count'"),
        "{shape_err}"
    );
}

#[test]
fn regex_errors_preserve_the_underlying_cause() {
    let regex_err = perry_wit::compile_core(
        "export function run(input: string): boolean { return /(/.test(input); }",
        "regex.ts",
    )
    .unwrap_err();
    assert_eq!(
        regex_err.primary().code(),
        codes::UNSUPPORTED_REGEX,
        "{regex_err}"
    );
    assert!(
        regex_err
            .primary()
            .notes()
            .iter()
            .any(|note| note.message().contains("unclosed group")),
        "{regex_err}"
    );
}

#[test]
fn namespace_errors_distinguish_missing_members_from_computed_access() {
    let helper = &[(
        "src/helper.ts",
        "export function present(): number { return 1; }",
    )];
    let ns_missing = api::compile_core(&program_with(
        "src/main.ts",
        "import * as helper from './helper'; export function run(): number { return helper.missing(); }",
        helper,
    ))
    .unwrap_err();
    assert_eq!(
        ns_missing.primary().kind(),
        ErrorKind::Module,
        "{ns_missing}"
    );
    assert_eq!(
        ns_missing.primary().code(),
        codes::UNKNOWN_EXPORT,
        "{ns_missing}"
    );
    assert!(
        ns_missing.primary().message().contains("missing"),
        "{ns_missing}"
    );
    assert!(ns_missing.primary().span().is_some(), "{ns_missing}");

    let ns_computed = api::compile_core(&program_with(
        "src/main.ts",
        "import * as helper from './helper'; export function run(k: string): number { return (helper as any)[k](); }",
        helper,
    ))
    .unwrap_err();
    assert_eq!(
        ns_computed.primary().kind(),
        ErrorKind::Unsupported,
        "{ns_computed}"
    );
    assert_eq!(
        ns_computed.primary().code(),
        codes::UNSUPPORTED_CONSTRUCT,
        "{ns_computed}"
    );
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
    const REGEX_CONST: &str = "case.ts:1:8: error[unsupported-regex]: ";
    let cases = [
        (iteration("new Headers()"), "case.ts:1:8: error[unsupported-construct]: synchronous for...of requires a dense typed array or string; this receiver has no supported iterator".into()),
        (iteration("new URLSearchParams()"), "case.ts:1:8: error[unsupported-construct]: synchronous for...of requires a dense typed array or string; URLSearchParams iteration is unsupported".into()),
        ("export function run(): string { return 'a'.replaceAll(/a/, 'b'); }".into(), "case.ts:1:40: error[unsupported-construct]: String.prototype.replaceAll called with a non-global RegExp argument".into()),
        ("class A {} class B extends A {} export function run(): number { return 0; }".into(), "error[unsupported-construct]: class B extends A: class inheritance is not supported yet".into()),
        (regex("/a/g"), "case.ts:1:55: error[unsupported-construct]: Regex search supports only the u and s flags".into()),
        (regex("/\\u{61}/"), format!("{REGEX_CONST}Braced Unicode regex escapes require the u flag")),
        (regex("/(?=a)/"), format!("{REGEX_CONST}Regex lookaround and inline flags are unsupported")),
        (regex("/(a)\\1/"), format!("{REGEX_CONST}Unsupported regex escape: \\1")),
        (regex("/(a)(b)(c)(d)(e)(f)(g)(h)(i)/"), format!("{REGEX_CONST}Regex has 9 capture groups; at most 8 are supported")),
        (regex("/(a{300})/"), format!("{REGEX_CONST}Regex capture program exceeds 256 states")),
        (regex("(input === 'x' ? /a/g : /a/)"), "case.ts:1:55: error[unsupported-construct]: regex flags must be statically known; use a regex literal or a local assigned only literals with the same g flag".into()),
        (regex("/\\uD800/u"), format!("{REGEX_CONST}Unpaired surrogate in regex escape")),
        (regex("new RegExp(input)"), "case.ts:1:55: error[unsupported-construct]: RegExp construction is unsupported; use a bounded regex literal".into()),
        ("export function test(): boolean { return /a/ === /a/; }".into(), "case.ts:1:8: error[unsupported-construct]: regex object identity is outside the literal matching profile".into()),
        ("export function run(x: number): number { return x ** (2 ** x); }".into(), "case.ts:1:55: error[unsupported-operator]: operator Pow is not supported yet".into()),
        ("function* count(): Generator<number> { yield 1; } export function run(x: number): number { return x; }".into(), "case.ts:1:1: error[unsupported-construct]: generator function count is not supported yet".into()),
        ("export function run(x: number): number { const d: WeakMap<object, number> = null as any; return x; }".into(), "case.ts:1:48: error[unsupported-type]: unresolved generic type WeakMap is not supported yet".into()),
        ("export function run(cb: number): number {\n  return (cb as any)(1);\n}".into(), "case.ts:2:10: error[type-mismatch]: expression is not callable".into()),
        ("export function run(s: string): number {\n  return (s as any).byteLength;\n}".into(), "case.ts:2:10: error[unsupported-construct]: property byteLength is not supported on string yet".into()),
        ("export function run(a: bigint, b: bigint): bigint {\n  return a ** b;\n}".into(), "case.ts:2:10: error[unsupported-operator]: operator Pow on bigint and bigint is not supported yet".into()),
        ("export function f(): string { return Temporal.ZonedDateTime.from('2024-01-01T00:00:00[UTC]').toString(); }".into(), "case.ts:1:38: error[unsupported-construct]: Temporal.ZonedDateTime.from is not supported; use Instant.from, Instant.fromEpochMilliseconds, PlainDate.from, PlainDate.compare, PlainDateTime.from or Temporal.Now".into()),
        ("export function f(s: string): string { return Temporal.PlainDate.from(s).add({ hours: 1 }).toString(); }".into(), "case.ts:1:47: error[unsupported-construct]: Temporal add duration field hours is not supported; use years, months, weeks, or days".into()),
        ("export function f(s: string): number { return Temporal.PlainDate.from(s).hour; }".into(), "case.ts:1:47: error[unsupported-construct]: property hour is not supported on Temporal.PlainDate yet".into()),
        ("export function run(): number {\n  return (performance as any).mark();\n}".into(), "case.ts:2:10: error[unsupported-construct]: performance.mark is not supported yet".into()),
        ("enum Color { A }\nexport function run(): number { return 0; }".into(), "case.ts:1:1: error[unsupported-construct]: TypeScript enum declarations are not supported yet; use a union of string or number literals instead".into()),
    ];
    let mismatches: Vec<String> = cases
        .iter()
        .filter_map(|(source, expected)| {
            let actual = match perry_wit::compile_core(source, "case.ts") {
                Ok(_) => "<compiled>".to_owned(),
                Err(error) => {
                    assert_ne!(
                        error.primary().kind(),
                        ErrorKind::Internal,
                        "{source}\n  => {error:#}"
                    );
                    format!("{error:#}")
                }
            };
            (actual != *expected).then(|| format!("{source}\n  => {actual:?}"))
        })
        .collect();
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    perry_wit::compile_core(
        "class Unused { power() { return 2 ** 3; } } export function run() { return 0; }",
        "case.ts",
    )
    .expect("unused methods are not lowered to MIR");
}

#[test]
fn throwing_exports_require_a_declared_channel_but_catches_and_stderr_succeed() -> anyhow::Result<()>
{
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
    let resource_stderr = Wit {
        application: b"package test:stderr; world app { import wasi:cli/stderr@0.2.0; export run: async func() -> f64; }
            package wasi:cli@0.2.0 { interface stderr { resource output-stream; get-stderr: func() -> output-stream; } }"
            .as_slice().into(),
        dependencies: vec![],
    };
    for wit in [&bare_wit, &resource_stderr] {
        let Err(error) = api::compile(&throwing, wit, &Options::default()) else {
            panic!("throwing export compiled without a P3 error channel");
        };
        assert_eq!(error.primary().code(), codes::MISSING_ERROR_CHANNEL);
        assert!(
            error
                .primary()
                .help()
                .is_some_and(|help| help.contains("async"))
        );
    }
    api::compile(&throwing, &stderr_wit, &Options::default()).unwrap();
    for source in [
        "export async function run() {try{throw new Error('handled');}catch(e){return 7;}}",
        "async function value(n:number){return n+1;} export async function run(){return await value(6);}",
        "async function value():Promise<number>{throw 'handled';} export async function run(){try{return await value();}catch{return 7;}}",
        "export async function run(){const holder:{jobs:Promise<number>[]}={jobs:[Promise.resolve(7)]};const job:Promise<number>=holder.jobs[0];return await job;}",
    ] {
        api::compile(&program(source), &bare_wit, &Options::default()).unwrap();
    }
    let error = api::compile(
        &program("async function value():Promise<number>{throw 'unhandled';} export async function run():Promise<number>{const values=await Promise.all([value()]);return values[0];}"),
        &bare_wit,
        &Options::default(),
    ).unwrap_err();
    assert_eq!(error.primary().code(), codes::MISSING_ERROR_CHANNEL);
    Ok(())
}

#[test]
fn guarded_callback_reads_preserve_raw_status_and_only_require_a_wit_channel_for_real_failures()
-> anyhow::Result<()> {
    let optional = Wit {
        application: b"package test:guard; world app { export run: async func(input: option<string>) -> string; }".as_slice().into(),
        dependencies: vec![],
    };
    let compile = |body: &str| {
        let source = format!(
            "function invoke(f) {{ return f(); }} export async function run(input: string | undefined):Promise<string> {{ {body} }}"
        );
        api::compile(
            &program_with("guard.ts", &source, &[]),
            &optional,
            &Options::default(),
        )
    };
    compile("return invoke(() => input ?? 'unset');")?;
    for body in [
        "return invoke(() => input.length.toString());",
        "return invoke(() => { if (input === undefined) throw new Error('missing'); return input; });",
        "if (input !== undefined) { invoke(() => { input = undefined; }); return input.length.toString(); } return 'unset';",
    ] {
        let Err(error) = compile(body) else {
            anyhow::bail!("accepted unhandled failure: {body}");
        };
        assert_eq!(
            error.primary().code(),
            codes::MISSING_ERROR_CHANNEL,
            "{body}: {error}"
        );
    }
    let mut program = crate::core_program::Program::new(
        "function fallback(value:number|undefined):number { return value ?? 7; } export function run(x:number):number { return fallback(x<0 ? undefined : x); }",
    );
    assert_eq!(program.throwing("run", -1.), (7., 0));
    assert_eq!(program.throwing("run", 3.), (3., 0));
    Ok(())
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
export async function run(){
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
            &program_with("src/main.ts", source, &[]),
            &wit,
            &Options::default(),
        )
        .unwrap_err();
        let diag = err.primary();
        assert_eq!(diag.code(), code, "{diag}");
        assert_eq!(diag.span().unwrap().start_line, line, "{diag}");
        assert!(diag.to_string().contains(needle), "{diag}");
    };

    check(
        "// header\nfunction run(id: bigint): Promise<bigint> { return Promise.resolve(id); }",
        codes::MISSING_EXPORT,
        2,
        "Add the 'export' keyword to function 'run'",
    );
    check(
        "// header\nexport async function run(id: number): Promise<bigint> { return 1n; }",
        codes::CONTRACT_MISMATCH,
        2,
        "bigint",
    );
    check(
        "// header\nexport function run(id: bigint): bigint { return id; }",
        codes::ASYNC_MISMATCH,
        2,
        "Add 'async'",
    );
    check(
        "import { ping } from 'test:mismatch/host';\nexport async function run(id: bigint): Promise<bigint> {\n  const x = (ping as any)(1);\n  return id;\n}",
        codes::CONTRACT_MISMATCH,
        3,
        "expects 2 arguments, got 1",
    );
}

#[test]
fn every_kind_has_a_fixture() {
    let temp = tempfile::tempdir().unwrap();
    let wit = Wit {
        application: b"package test:kinds; world app { export run: func() -> f64; }"
            .as_slice()
            .into(),
        dependencies: vec![],
    };
    let program = |source: &str| Program {
        source: source.as_bytes().into(),
        modules: vec![],
    };

    let kinds: std::collections::HashSet<_> = codes::ALL.iter().map(|code| code.kind()).collect();
    for kind in kinds {
        let error = match kind {
            ErrorKind::InvalidInput => api::compile_core(&Program {
                source: b"export function run(): number { return 1; }"
                    .as_slice()
                    .into(),
                modules: vec![Module {
                    name: "../escape.ts".into(),
                    contents: vec![],
                }],
            })
            .unwrap_err(),
            ErrorKind::Io => {
                perry_wit::compile_file(&temp.path().join("missing.ts"), &CompileOptions::default())
                    .unwrap_err()
            }
            ErrorKind::Wit => api::compile(
                &program("export function run(): number { return 1; }"),
                &Wit {
                    application: b"not valid wit".as_slice().into(),
                    dependencies: vec![],
                },
                &Options::default(),
            )
            .unwrap_err(),
            ErrorKind::Syntax => api::compile_core(&program("export function {")).unwrap_err(),
            ErrorKind::Module => {
                api::compile_core(&program("export { run } from './missing';")).unwrap_err()
            }
            ErrorKind::TypeCheck => {
                api::compile_core(&program("export function run(x) { return x; }")).unwrap_err()
            }
            ErrorKind::Unsupported => api::compile_core(&program(
                "export function run(x: number): number { return x ** (2 ** x); }",
            ))
            .unwrap_err(),
            ErrorKind::Contract => api::compile(
                &program("export function other(): number { return 1; }"),
                &wit,
                &Options::default(),
            )
            .unwrap_err(),
            ErrorKind::Internal => perry_wit::CompileError::from(perry_wit::Diagnostic::new(
                codes::INTERNAL,
                "synthetic internal error",
            )),
        };
        assert_eq!(error.primary().kind(), kind, "{error}");
    }
}

#[test]
fn deferred_local_reports_incompatible_native_writes_at_its_declaration() -> anyhow::Result<()> {
    let source = r#"export function run(flag: boolean): boolean {
  const bytes = new Uint8Array([5]);
  let storage;
  if (flag) storage = bytes;
  else storage = { marker: 1 };
  return storage === bytes;
}"#;
    let error = perry_wit::compile_core(source, "storage.ts")
        .err()
        .context("expected unsupported storage")?;
    let diagnostic = error.primary();
    assert_eq!(diagnostic.code(), codes::UNSUPPORTED_TYPE, "{error}");
    assert!(diagnostic.message().contains("local 'storage'"), "{error}");
    let span = diagnostic.span().context("missing binding location")?;
    assert_eq!((span.start_line, span.start_column), (3, 7));
    assert_eq!(source.get(span.start_byte..span.end_byte), Some("storage"));
    Ok(())
}

#[test]
fn inference_engine_diagnostics_strip_internal_names_and_bound_recursion() -> anyhow::Result<()> {
    // Exported object-literal method diagnostics report the source method name, never internal synthetic names.
    for (source, file, expected_name) in [
        (
            "export const obj = { greet(name) { return 'hi ' + name; } };\nexport function run(): string { return obj.greet('a'); }\n",
            "obj_method.js",
            "'greet'",
        ),
        (
            "export const counter = { n: 0, bump(by) { this.n += by; } };\nexport function run(): number { counter.bump(1); return counter.n; }\n",
            "obj_closure_method.js",
            "'bump'",
        ),
    ] {
        let obj_method = perry_wit::compile_core(source, file)
            .err()
            .context("expected missing annotation diagnostic for object method")?;
        assert_eq!(obj_method.primary().code(), codes::MISSING_TYPE_ANNOTATION);
        let msg = obj_method.to_string();
        assert!(msg.contains(expected_name), "{obj_method}");
        assert!(!msg.contains("__obj_method_"), "{obj_method}");
        assert!(!msg.contains("callback_"), "{obj_method}");
    }

    // Polymorphic recursion hits the explicit specialization budget instead of diverging.
    let poly = perry_wit::compile_core(
        "function wrap(x, n) { return n <= 0 ? x : wrap([x], n - 1); }\nexport function run(): number { wrap(1, 3); return 0; }\n",
        "poly.js",
    )
    .err()
    .context("expected specialization limit diagnostic")?;
    assert_eq!(poly.primary().code(), codes::UNSUPPORTED_CONSTRUCT);
    assert!(
        poly.primary()
            .message()
            .contains("generic specialization limit"),
        "{poly}"
    );

    // Self-nesting composites (empty arrays, recursive unions, and loops) widen cleanly and explain why.
    for (source, file, expected_code) in [
        (
            "export function run(): number { const a = []; a.push(a); return a.length; }\n",
            "self_nest_array.js",
            codes::MISSING_TYPE_ANNOTATION,
        ),
        (
            "function wrap(n) { return n === 0 ? 0 : [wrap(n - 1)]; }\nexport function run(): number { return JSON.stringify(wrap(2)).length; }\n",
            "self_nest_union.js",
            codes::MISSING_TYPE_ANNOTATION,
        ),
        (
            "export function run(): number { let x; x = 1; for (let i = 0; i < 3; i++) x = [x]; return JSON.stringify(x).length; }\n",
            "self_nest_loop.js",
            codes::MISSING_TYPE_ANNOTATION,
        ),
    ] {
        let self_nest = perry_wit::compile_core(source, file)
            .err()
            .context("expected widening diagnostic")?;
        assert_eq!(self_nest.primary().code(), expected_code, "{self_nest}");
        assert!(
            self_nest.to_string().contains("nests itself"),
            "expected self-nesting explanation in diagnostic: {self_nest}"
        );
    }
    Ok(())
}
