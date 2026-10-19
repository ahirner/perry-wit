use super::{DecimalSyntax, Number, parse_integer, scan_decimal, split_sign};

#[test]
fn decimal_grammars_preserve_token_and_error_boundaries() {
    for (text, prefix, json) in [
        ("0", Ok(1), Ok(1)),
        ("01", Ok(2), Ok(1)),
        (".5", Ok(2), Err(0)),
        ("1.", Ok(2), Err(2)),
        ("1e", Ok(1), Err(2)),
        ("1e+tail", Ok(1), Err(3)),
        ("1.25e-2tail", Ok(7), Ok(7)),
        (".", Err(1), Err(0)),
        ("", Err(0), Err(0)),
    ] {
        for (syntax, expected) in [(DecimalSyntax::Prefix, prefix), (DecimalSyntax::Json, json)] {
            assert_eq!(
                scan_decimal::<true>(text.as_bytes(), syntax).map(|v| v.consumed),
                expected,
                "{text}"
            );
            assert_eq!(
                scan_decimal::<false>(text.as_bytes(), syntax).map(|v| v.consumed),
                expected,
                "{text}"
            );
        }
    }
}

fn check(literal: &str, expected: u64) {
    let input = format!("{literal}tail");
    let (negative, body) = split_sign(input.as_bytes());
    let token = scan_decimal::<false>(body, DecimalSyntax::Prefix).expect("decimal grammar");
    assert_eq!(token.consumed + 4, body.len());
    let actual = token.float(negative).expect("scanned decimal");
    assert_eq!(actual.to_bits(), expected, "{literal}: {actual:e}");
}

#[test]
fn decimal_tokens_preserve_rounding_and_sign() {
    for (literal, expected) in [
        (".5", 0x3fe0000000000000),
        ("1.", 0x3ff0000000000000),
        ("001.2", 0x3ff3333333333333),
        ("0", 0x0000000000000000),
        ("-0", 0x8000000000000000),
        ("0.0", 0x0000000000000000),
        ("1", 0x3ff0000000000000),
        ("-1", 0xbff0000000000000),
        ("0.1", 0x3fb999999999999a),
        ("0.3", 0x3fd3333333333333),
        ("1e23", 0x44b52d02c7e14af6),
        ("8.98846567431158e307", 0x7fe0000000000000),
        ("1.7976931348623157e308", 0x7fefffffffffffff),
        ("1.7976931348623158e308", 0x7fefffffffffffff),
        ("1.7976931348623159e308", 0x7ff0000000000000),
        ("2e308", 0x7ff0000000000000),
        ("4.9e-324", 0x0000000000000001),
        ("2.4703282292062327e-324", 0x0000000000000000),
        ("2.4703282292062328e-324", 0x0000000000000001),
        ("2.2250738585072011e-308", 0x000fffffffffffff),
        ("2.2250738585072012e-308", 0x0010000000000000),
        ("5e-324", 0x0000000000000001),
        ("1e-400", 0x0000000000000000),
        ("9007199254740993", 0x4340000000000000),
        ("9007199254740992.5", 0x4340000000000000),
        ("123456789012345678901234567890", 0x45f8ee90ff6c373e),
        (
            "0.000000000000000000000000000000000000001",
            0x37d5c72fb1552d83,
        ),
        ("1e1000000000000", 0x7ff0000000000000),
        ("1e-1000000000000", 0x0000000000000000),
        ("7.038531e-26", 0x3ab5c87fb0000000),
        ("3.4028235677973366e38", 0x47effffff0000000),
        (
            "1.00000000000000011102230246251565404236316680908203125",
            0x3ff0000000000000,
        ),
        (
            "1.00000000000000011102230246251565404236316680908203124",
            0x3ff0000000000000,
        ),
        (
            "1.00000000000000011102230246251565404236316680908203126",
            0x3ff0000000000001,
        ),
        (
            "100000000000000000000000000000000000000000000000000000000000000000000001e-70",
            0x4024000000000000,
        ),
        ("123.456e7", 0x41d2657900000000),
        ("0.00001234e10", 0x40fe208000000000),
        ("1E+2", 0x4059000000000000),
        ("1e-2", 0x3f847ae147ae147b),
        ("100e-2", 0x3ff0000000000000),
        ("1.000e2", 0x4059000000000000),
    ] {
        check(literal, expected);
    }
    let long = format!("1.{}1e0", "0".repeat(900));
    check(&long, 1.0_f64.to_bits());
    let tie = format!("9007199254740993{}", "0".repeat(800));
    check(&format!("{tie}e-800"), 0x4340000000000000);
    check(&format!("{tie}1e-801"), 0x4340000000000001);
}

#[test]
fn decimal_tokens_round_trip_float_bits() {
    let mut bits = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..50_000 {
        bits ^= bits << 13;
        bits ^= bits >> 7;
        bits ^= bits << 17;
        let value = f64::from_bits(bits & 0xffef_ffff_ffff_ffff);
        check(&format!("{value:e}"), value.to_bits());
    }
}

#[test]
fn parse_integer_handles_exact_64_bit_bounds_and_overflow() {
    for (magnitude, negative) in [
        (0, false),
        (1 << 63, true),
        (i64::MAX as u64, false),
        (u64::MAX, false),
    ] {
        for radix in 2..=36 {
            let mut buffer = [0; 66];
            let text = super::format_integer(magnitude, negative, radix, &mut buffer);
            let text = core::str::from_utf8(text).expect("integer digits are ASCII");
            let actual = parse_integer(text, Number::Unsigned(u64::from(radix)));
            let expected = if negative {
                Number::Signed(i64::MIN)
            } else if magnitude <= i64::MAX as u64 {
                Number::Signed(magnitude as i64)
            } else {
                Number::Unsigned(magnitude)
            };
            assert_eq!(actual, expected, "{text} in base {radix}");
        }
    }
    assert_eq!(
        parse_integer("9007199254740993", Number::Float(f64::NAN)),
        Number::Signed(9_007_199_254_740_993)
    );
    assert_eq!(
        parse_integer("9223372036854775807", Number::Float(f64::NAN)),
        Number::Signed(i64::MAX)
    );
    assert_eq!(
        parse_integer("9223372036854775808", Number::Float(f64::NAN)),
        Number::Unsigned(1 << 63)
    );
    assert_eq!(
        parse_integer("18446744073709551615", Number::Float(f64::NAN)),
        Number::Unsigned(u64::MAX)
    );
    assert_eq!(
        parse_integer("-9223372036854775808", Number::Float(f64::NAN)),
        Number::Signed(i64::MIN)
    );
    assert!(matches!(
        parse_integer("18446744073709551616", Number::Float(f64::NAN)),
        Number::Float(v) if v.is_nan()
    ));
    assert!(matches!(
        parse_integer("-9223372036854775809", Number::Float(f64::NAN)),
        Number::Float(v) if v.is_nan()
    ));
}

#[test]
fn parse_integer_wraps_radix_by_to_int32() {
    assert_eq!(
        parse_integer("10", Number::Float(4_294_967_298.0)),
        Number::Signed(2)
    );
    assert_eq!(
        parse_integer("10", Number::Unsigned(4_294_967_298)),
        Number::Signed(2)
    );
    assert_eq!(
        parse_integer("0x10", Number::Float(4_294_967_296.0)),
        Number::Signed(16)
    );
    assert!(matches!(
        parse_integer("10", Number::Float(37.0)),
        Number::Float(v) if v.is_nan()
    ));
}
