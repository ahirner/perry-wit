mod support;

use support::{run, stdout};

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
