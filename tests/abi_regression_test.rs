mod support;

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
            "package test:abi; world test {{ export run-task: func(input: {ty}) -> string; }}"
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
