mod support;

use std::process::Command;
use support::{run, stdout};

#[test]
fn guest_function_values_and_captures_match_node() {
    let source = r#"
        const identity = value => value;
        console.log(identity("identity"));
        console.log(typeof identity);
        function named(a, b, c, d) { return [a, b, c, d].join("|"); }
        const alias = named;
        console.log(alias(1, 2, 3, 4));
        console.log(alias("missing"));
        console.log(alias(1, 2, 3, 4, 5));
        function create(prefix) {
            const object = {value: "before"};
            const callback = suffix => prefix + object.value + suffix;
            object.value = "after";
            return callback;
        }
        const captured = create("capture:");
        console.log(captured("!"));
        let evaluations = "";
        function argument(label) { evaluations += label; return label; }
        console.log(alias(argument("a"), argument("b"), argument("c"), argument("d")));
        console.log(evaluations);
        function nested(callback, value) { return callback(value); }
        console.log(nested(identity, "nested"));
        evaluations = "";
        function factory() { evaluations += "callee;"; return identity; }
        console.log(factory()(argument("argument;")));
        console.log(evaluations);
        const noResult = value => { console.log(value); };
        console.log(noResult("void"));
        function namedVoid(value) { console.log(value); }
        const voidAlias = namedVoid;
        console.log(voidAlias("named-void"));
        const counter = {count: "2"};
        let receivers = 0;
        function receiver() { receivers++; return counter; }
        console.log([receiver().count++, ++receiver().count, receiver().count--, --receiver().count].join("|"));
        console.log(counter.count + ":" + receivers);
        console.log([5000 % 256, -5 % 3, 5 % -3, "5" % 3, 1 % 0, Infinity % 3].join("|"));
    "#;
    let expected = Command::new("node")
        .args(["--eval", source])
        .output()
        .unwrap();
    let actual = run(source, None, None);
    assert!(
        actual.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(stdout(&actual), stdout(&expected));
}

#[test]
fn callback_exceptions_unwind_to_guest_catches() {
    let source = r#"
        const fail = value => { throw value; };
        function invoke(callback) { return callback("failed"); }
        try { invoke(fail); console.log("unreachable"); }
        catch (error) { console.log(error); }
        const notCallable: any = 42;
        try { notCallable("ignored"); console.log("unreachable"); }
        catch (error) { console.log(error); }
        const identity = value => value;
        console.log(identity("recovered"));
    "#;
    assert_eq!(
        stdout(&run(source, None, None)),
        "failed\nTypeError: Value is not a callable guest function\nrecovered\n"
    );
}

#[test]
fn unsupported_closure_forms_report_compiler_diagnostics() {
    for (source, message) in [
        (
            "function create() { let count = 0; return () => ++count; } create();",
            "shared mutable captures",
        ),
        (
            "const callback = (value = 1) => value; callback();",
            "rest, default, or arguments parameters",
        ),
        (
            "const callback = (...values) => values.length; callback();",
            "rest, default, or arguments parameters",
        ),
        (
            "const callback = async value => value; callback(1);",
            "Async and generator",
        ),
        (
            "class Owner { value = 1; create() { return () => this.value; } }",
            "capturing this or new.target",
        ),
        (
            "const callback = function() { return this.value; }; callback();",
            "using this or new.target",
        ),
        (
            "function named() { return this.value; } const alias = named; alias();",
            "using this or new.target",
        ),
        (
            "function named(value = 1) { return value; } const alias = named; alias();",
            "rest, default, or arguments parameters",
        ),
        (
            "function named(...values) { return values.length; } const alias = named; alias();",
            "rest, default, or arguments parameters",
        ),
        (
            "const callback = value => value; callback(...[1]);",
            "spread arguments",
        ),
    ] {
        let error = perry_wit::compiler::compile_typescript_raw(source, "unsupported-callback.ts")
            .unwrap_err();
        assert!(error.to_string().contains(message), "{source}: {error}");
    }
}

#[test]
fn string_search_and_array_join_preserve_surrogate_units() {
    let source = r#"
        const emoji = "😀";
        const high = emoji.charAt(0);
        const low = emoji.charAt(1);
        console.log(high.includes("�"));
        console.log(high.startsWith("�"));
        console.log(low.endsWith("�"));
        console.log(emoji.includes(high));
        console.log(emoji.startsWith(high));
        console.log(emoji.endsWith(low));
        console.log(high.includes(low));
        console.log(high.includes(""));
        console.log(high.startsWith(""));
        console.log(low.endsWith(""));
        console.log([high, low].join("") === emoji);
        console.log(JSON.stringify([high, low].join("-")));
        console.log(JSON.stringify([high, "x"].join(low)));
        console.log(["a", null, undefined, "b"].join());
        console.log(["a", "b"].join(0));
        console.log([].join(high));
    "#;
    let expected = Command::new("node")
        .args(["--eval", source])
        .output()
        .unwrap();
    assert!(expected.status.success());
    let actual = run(source, None, None);
    assert_eq!(stdout(&actual), stdout(&expected));
}

#[test]
fn strings_use_utf16_code_units_for_methods_indices_and_length() {
    let source = r#"
        const text = "😀x𝄞é";
        console.log(text.length);
        console.log(text.charAt(2));
        console.log(text[2]);
        for (let i = 0; i < text.length; i++) {
            const fromMethod = text.charAt(i);
            const fromIndex = text[i];
            console.log(fromMethod.length);
            console.log(text.charCodeAt(i));
            console.log(fromMethod.charCodeAt(0));
            console.log(fromIndex.charCodeAt(0));
            console.log(fromMethod === fromIndex);
            console.log(JSON.stringify(fromIndex));
        }
        console.log(text[0] === text[1]);
        console.log(text[0] + text[1] === "😀");
        console.log(JSON.stringify([text[0], text[1], text[2]]));
        const indices = [-1, -0.5, 1.9, NaN, Infinity, undefined, 99];
        for (let i = 0; i < indices.length; i++) {
            console.log(JSON.stringify(text.charAt(indices[i])));
            console.log(text.charCodeAt(indices[i]));
        }
        console.log(text[-1]);
        console.log(text[1.5]);
        console.log(text[99]);
        console.log(text["2"]);
    "#;
    let expected = Command::new("node")
        .args(["--eval", source])
        .output()
        .unwrap();
    let actual = run(source, None, None);
    assert_eq!(stdout(&actual), stdout(&expected));
}

#[test]
fn equality_compares_strings_primitives_and_object_identity() {
    let output = run(
        r#"
        const a: any = JSON.parse('{"x":1}');
        const b: any = JSON.parse('{"x":1}');
        console.log("same" === ("sa" + "me"));
        console.log("same" !== "other");
        console.log(a === a);
        console.log(a === b);
        console.log(NaN === NaN);
        console.log(0 === -0);
        console.log(true === 1);
        console.log(null === undefined);
        console.log(null == undefined);
        console.log("42" == 42);
        console.log(false == "");
        console.log(" 0x2a " == 42);
        console.log("inf" == Infinity);
        console.log([1] == 1);
        console.log("x" == 0);
    "#,
        None,
        None,
    );
    assert_eq!(
        stdout(&output),
        "true\ntrue\ntrue\nfalse\nfalse\ntrue\nfalse\nfalse\ntrue\ntrue\ntrue\ntrue\nfalse\ntrue\nfalse\n"
    );
}

#[test]
fn malformed_json_fails_while_valid_primitives_keep_their_types() {
    let output = run(
        "console.log(JSON.parse('{invalid')); console.log('after');",
        None,
        None,
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("JSON parse error"));
    let output = run(
        r#"
        console.log(JSON.parse("null"));
        console.log(JSON.parse("true"));
        console.log(JSON.parse("42") * 2);
        console.log(JSON.parse("42") + 1);
        console.log(JSON.parse("true") + 1);
        console.log(JSON.parse("null") + 1);
        console.log(undefined + 1);
        console.log(JSON.parse('"hello"'));
    "#,
        None,
        None,
    );
    assert_eq!(stdout(&output), "null\ntrue\n84\n43\n2\n1\nNaN\nhello\n");
}

#[test]
fn process_exit_terminates_immediately_with_wasi_status() {
    for (argument, success) in [("", true), ("0", true), ("5", false), ("-1", false)] {
        let output = run(
            &format!("console.log('before'); process.exit({argument}); console.log('after');"),
            None,
            None,
        );
        assert_eq!(output.status.success(), success);
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "before\n");
    }
}

#[test]
fn truthiness_distinguishes_tagged_values_from_numeric_nan() {
    let output = run(
        r#"
        const values: any[] = ["hello", "", JSON.parse("{}"), [], true, false, null, undefined, 0, -0, 1, -1, NaN];
        for (const value of values) {
            if (value) { console.log("truthy"); } else { console.log("falsy"); }
        }
    "#,
        None,
        None,
    );
    assert_eq!(
        stdout(&output),
        "truthy\nfalsy\ntruthy\ntruthy\ntruthy\nfalsy\nfalsy\nfalsy\nfalsy\nfalsy\ntruthy\ntruthy\nfalsy\n"
    );
}

#[test]
fn console_writes_preserve_large_utf8_messages_on_both_streams() {
    for message in ["a".repeat(4096), "é".repeat(6000)] {
        let literal = serde_json::to_string(&message).unwrap();
        let output = run(
            &format!("console.log({literal}); console.error({literal});"),
            None,
            None,
        );
        assert_eq!(stdout(&output), format!("{message}\n"));
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            format!("{message}\n")
        );
    }
}
