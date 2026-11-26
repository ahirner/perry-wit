use super::*;

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

    let repeated = "{ const fs = [inc]; total += count(fs,fs) + ({size,fs}).size(); }".repeat(257);
    let source = format!(
        "function inc(n) {{return n+1;}} function count(a,b) {{return a.length + b.length;}}
         function size() {{return this.fs.length;}}
         export function run():number {{let total=0; {repeated} return total;}}"
    );
    api::compile_core(&program_with("callable-arrays.ts", &source, &[]))?;

    // The first recursive call changes the key; the second reuses it.
    api::compile_core(&program_with(
        "finite.ts",
        "function again<T>(value:T, n:number):number { return n<=0 ? 0 : again([0], n-1); } export function run():number {return again(0, 2);}",
        &[],
    ))?;
    let mut finite = String::from(
        "function count<T>(v:T):number {return 1;} function inc(n){return n+1;}
         export function run():number {let n=0;",
    );
    for id in 0..255 {
        writeln!(finite, "n += count({{field{id}: 1}});")?;
    }
    // The same record shape has two alias patterns, bringing the instance count to 257.
    finite.push_str("const a=[inc], b=[inc]; return n + count({x:a,y:a}) + count({x:a,y:b});}");
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
/// and rejected control flow is covered by the frontend's tests.

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
