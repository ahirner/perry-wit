mod support;

use std::process::Command;
use support::{run, stdout};

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
