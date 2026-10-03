//! Unit & integration test suite for R4.1 UTF-8 Scalar Value Contract & Operation Matrix.

use perry_wit::compile_typescript_waffle;
use perry_wit::waffle_backend::WaffleCompileOptions;
use perry_wit::waffle_backend::text_contract::{
    IndexUnit, OperationStatus, TextContractMatrix, scalar_char_at, scalar_index_of, scalar_length,
    scalar_slice, validate_hir_text, validate_source_text, validate_utf8_boundary,
};

#[test]
fn test_operation_matrix_completeness_and_units() {
    let entries = TextContractMatrix::entries();
    assert!(
        entries.len() >= 15,
        "Matrix must cover the complete inventory of supported and diagnosed string operations"
    );

    // Verify key operations have explicit contracts and deliberate Node differences
    let length_op = entries.iter().find(|e| e.operation == "length").unwrap();
    assert_eq!(length_op.unit, IndexUnit::ScalarValue);
    assert_eq!(length_op.status, OperationStatus::SupportedScalar);
    assert!(length_op.node_difference.contains("Unicode scalar values"));

    let index_op = entries
        .iter()
        .find(|e| e.operation == "index_access")
        .unwrap();
    assert_eq!(index_op.unit, IndexUnit::ScalarValue);
    assert_eq!(index_op.status, OperationStatus::SupportedScalar);
    assert!(index_op.node_difference.contains("complete scalar"));

    let char_at_op = entries.iter().find(|e| e.operation == "charAt").unwrap();
    assert_eq!(char_at_op.unit, IndexUnit::ScalarValue);
    assert!(char_at_op.node_difference.contains("complete scalar"));

    let code_point_at_op = entries
        .iter()
        .find(|e| e.operation == "codePointAt")
        .unwrap();
    assert_eq!(code_point_at_op.unit, IndexUnit::ScalarValue);
    assert_eq!(code_point_at_op.status, OperationStatus::SupportedScalar);

    let char_code_at_op = entries
        .iter()
        .find(|e| e.operation == "charCodeAt")
        .unwrap();
    assert_eq!(char_code_at_op.unit, IndexUnit::Utf16CodeUnit);
    assert_eq!(char_code_at_op.status, OperationStatus::DisallowedUtf16);
    assert!(
        char_code_at_op
            .node_difference
            .contains("must use codePointAt")
    );

    let from_code_point_op = entries
        .iter()
        .find(|e| e.operation == "fromCodePoint")
        .unwrap();
    assert_eq!(from_code_point_op.unit, IndexUnit::ScalarValue);
    assert_eq!(from_code_point_op.status, OperationStatus::SupportedScalar);

    let from_char_code_op = entries
        .iter()
        .find(|e| e.operation == "fromCharCode")
        .unwrap();
    assert_eq!(from_char_code_op.unit, IndexUnit::Utf16CodeUnit);
    assert_eq!(from_char_code_op.status, OperationStatus::DisallowedUtf16);

    let slice_op = entries.iter().find(|e| e.operation == "slice").unwrap();
    assert_eq!(slice_op.unit, IndexUnit::ScalarValue);

    let index_of_op = entries.iter().find(|e| e.operation == "indexOf").unwrap();
    assert_eq!(index_of_op.unit, IndexUnit::ScalarValue);

    let split_op = entries.iter().find(|e| e.operation == "split").unwrap();
    assert_eq!(split_op.unit, IndexUnit::ScalarValue);

    let abi_op = entries
        .iter()
        .find(|e| e.operation == "canonical_abi_string")
        .unwrap();
    assert_eq!(abi_op.unit, IndexUnit::Byte);
    assert_eq!(abi_op.status, OperationStatus::SupportedBoundary);
}

#[test]
fn test_valid_text_and_paired_escapes_accepted() {
    let valid_sources = [
        r#"let s = "hello";"#,
        r#"let s = "café \u00e9";"#,
        r#"let s = "😀";"#,
        r#"let s = "\u{1F600}";"#,
        r#"let s = "\uD83D\uDE00";"#, // Valid paired surrogate escape
        r#"let s = "e\u0301";"#,      // Combining sequence
        r#"let s = "";"#,             // Empty string
        r#"let s = "a\0b";"#,         // Embedded NUL
        r#"// Comment with \uD800"#,  // Surrogate in comment must be ignored
        r#"/* Block comment with \uDC00 */"#,
        r#"let s = "escaped backslash \\uD800";"#,
    ];

    for src in valid_sources {
        assert!(
            validate_source_text(src).is_ok(),
            "Expected valid text to pass validation: {src}"
        );
    }
}

#[test]
fn test_unpaired_surrogates_eagerly_rejected() {
    let invalid_cases = [
        (r#"let s = "\uD800";"#, "Unpaired high surrogate escape"),
        (r#"let s = "\uD83D";"#, "Unpaired high surrogate escape"),
        (r#"let s = "\uDC00";"#, "Unpaired low surrogate escape"),
        (r#"let s = "\uDE00";"#, "Unpaired low surrogate escape"),
        (
            r#"let s = "\uDE00\uD83D";"#, // Reversed pair
            "Unpaired low surrogate escape",
        ),
        (
            r#"let s = "\uD83D\uD83D";"#, // Two high surrogates
            "Unpaired high surrogate escape",
        ),
        (
            r#"let s = "\u{D800}";"#, // Braced surrogate
            "Surrogate code point escape",
        ),
        (
            r#"let s = `template with \uD800`;"#, // In template literal
            "Unpaired high surrogate escape",
        ),
    ];

    for (src, expected_err) in invalid_cases {
        let res = validate_source_text(src);
        assert!(
            res.is_err(),
            "Expected invalid surrogate to be rejected: {src}"
        );
        let err_msg = res.unwrap_err().to_string();
        assert!(
            err_msg.contains(expected_err),
            "Expected error containing '{expected_err}', got '{err_msg}'"
        );
    }
}

#[test]
fn test_compiler_pipeline_rejects_unpaired_surrogates_cleanly() {
    let source = r#"
        export function run(input: number): number {
            let invalid = "\uD800";
            return input;
        }
    "#;
    let res = compile_typescript_waffle(
        source,
        "invalid_surrogate.ts",
        &WaffleCompileOptions::default(),
    );
    assert!(res.is_err());
    let err = format!("{:#}", res.unwrap_err());
    assert!(err.contains("Unpaired high surrogate escape"));
}

#[test]
fn test_compiler_pipeline_rejects_template_unpaired_surrogates() {
    let source = r#"
        export function run(input: number): number {
            let invalid = `template with \uD83D`;
            return input;
        }
    "#;
    let res = compile_typescript_waffle(
        source,
        "invalid_template.ts",
        &WaffleCompileOptions::default(),
    );
    assert!(res.is_err());
    let err = format!("{:#}", res.unwrap_err());
    assert!(err.contains("Unpaired high surrogate escape"));
}

#[test]
fn test_hir_validator_rejects_wtf_strings() {
    let mut hir = perry_hir::ir::Module::new("test");
    hir.init
        .push(perry_hir::ir::Stmt::Expr(perry_hir::ir::Expr::WtfString(
            vec![0xED, 0xA0, 0x80],
        )));

    let res = validate_hir_text(&hir);
    assert!(res.is_err());
    let err = res.unwrap_err().to_string();
    assert!(err.contains("WTF-8 string literal"));
}

#[test]
fn test_hir_validator_diagnoses_char_code_at() {
    let mut hir = perry_hir::ir::Module::new("test");
    hir.init
        .push(perry_hir::ir::Stmt::Expr(perry_hir::ir::Expr::Call {
            callee: Box::new(perry_hir::ir::Expr::PropertyGet {
                object: Box::new(perry_hir::ir::Expr::String("test".into())),
                property: "charCodeAt".into(),
                byte_offset: 0,
            }),
            args: vec![perry_hir::ir::Expr::Integer(0)],
            type_args: vec![],
            byte_offset: 0,
        }));

    let res = validate_hir_text(&hir);
    assert!(res.is_err());
    let err = res.unwrap_err().to_string();
    assert!(err.contains("charCodeAt is disallowed"));
    assert!(err.contains("codePointAt"));
}

#[test]
fn test_scalar_operations_semantics() {
    // 1. Length: Unicode scalar values count
    assert_eq!(scalar_length(""), 0);
    assert_eq!(scalar_length("hello"), 5);
    assert_eq!(scalar_length("café"), 4);
    assert_eq!(scalar_length("😀"), 1);
    assert_eq!(scalar_length("😀x"), 2);
    assert_eq!(scalar_length("a\0b"), 3);

    // 2. Index access / charAt
    assert_eq!(scalar_char_at("😀x", 0), Some("😀"));
    assert_eq!(scalar_char_at("😀x", 1), Some("x"));
    assert_eq!(scalar_char_at("😀x", 2), None);

    // 3. Slicing with scalar indices
    assert_eq!(scalar_slice("😀hello", 0, Some(1)), "😀");
    assert_eq!(scalar_slice("😀hello", 1, Some(3)), "he");
    assert_eq!(scalar_slice("😀hello", 1, None), "hello");
    assert_eq!(scalar_slice("😀hello", -2, None), "lo");
    assert_eq!(scalar_slice("😀hello", 0, Some(0)), "");
    assert_eq!(scalar_slice("😀hello", 5, Some(2)), "");

    // 4. IndexOf with scalar offsets
    assert_eq!(scalar_index_of("hello 😀 world", "😀", 0), Some(6));
    assert_eq!(scalar_index_of("hello 😀 world", "world", 0), Some(8));
    assert_eq!(scalar_index_of("hello 😀 world", "world", 7), Some(8));
    assert_eq!(scalar_index_of("hello 😀 world", "world", 9), None);
    assert_eq!(scalar_index_of("hello 😀 world", "missing", 0), None);

    // 5. UTF-8 boundary validation
    let valid_bytes = "valid UTF-8 \u{1F600}".as_bytes();
    assert_eq!(
        validate_utf8_boundary(valid_bytes).unwrap(),
        "valid UTF-8 \u{1F600}"
    );

    let invalid_bytes = &[0xFF, 0xFE, 0xFD];
    assert!(validate_utf8_boundary(invalid_bytes).is_err());
}

#[test]
fn test_template_interpolation_boundaries() {
    for source in [
        r#"let s = `outer${`\uD800`}`;"#,
        r#"let s = `outer${`inner${"\uDC00"}`}`;"#,
        r#"let s = `outer${{ value: `\u{D800}` }.value}`;"#,
        r#"let s = `\uD83D${"ok"}\uDE00`;"#,
    ] {
        assert!(validate_source_text(source).is_err(), "{source}");
    }
    for source in [
        r#"let s = `outer${/* \uD800 ` ${ } */ `\uD83D\uDE00`}`;"#,
        "let s = `outer${// \\uD800 ` ${ }\n `ok`}`;",
        r#"let s = `outer${{ value: `nested${"ok"}` }.value}`;"#,
        r#"let s = `escaped \${text} \\uD800 \` end`;"#,
        r#"let s = `outer${/\uD800/.source}`;"#,
    ] {
        validate_source_text(source).unwrap_or_else(|error| panic!("{source}: {error:#}"));
    }
}
