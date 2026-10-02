mod support;

use support::{run, stdout};

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
