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

fn check(literal: &str) {
    let expected: f64 = literal.parse().unwrap();
    let (negative, body) = split_sign(literal.as_bytes());
    let token =
        scan_decimal::<true>(body, DecimalSyntax::Prefix).expect("generated decimal grammar");
    assert_eq!(token.consumed, body.len());
    let actual = token
        .decimal
        .finish(token.mantissa, negative, token.exponent);
    assert_eq!(
        actual.to_bits(),
        expected.to_bits(),
        "{literal}: {actual:e} != {expected:e}"
    );
}

#[test]
fn hard_cases_round_like_core() {
    for literal in [
        "0",
        "-0",
        "0.0",
        "1",
        "-1",
        "0.1",
        "0.3",
        "1e23",
        "8.98846567431158e307",
        "1.7976931348623157e308",
        "1.7976931348623158e308",
        "1.7976931348623159e308",
        "2e308",
        "4.9e-324",
        "2.4703282292062327e-324",
        "2.4703282292062328e-324",
        "2.2250738585072011e-308",
        "2.2250738585072012e-308",
        "5e-324",
        "1e-400",
        "9007199254740993",
        "9007199254740992.5",
        "123456789012345678901234567890",
        "0.000000000000000000000000000000000000001",
        "1e1000000000000",
        "1e-1000000000000",
        "7.038531e-26",
        "3.4028235677973366e38",
        "1.00000000000000011102230246251565404236316680908203125",
        "1.00000000000000011102230246251565404236316680908203124",
        "1.00000000000000011102230246251565404236316680908203126",
        "100000000000000000000000000000000000000000000000000000000000000000000001e-70",
        "123.456e7",
        "0.00001234e10",
        "1E+2",
        "1e-2",
        "100e-2",
        "1.000e2",
    ] {
        check(literal);
    }
    let long = format!("1.{}1e0", "0".repeat(900));
    check(&long);
    let tie = format!("9007199254740993{}", "0".repeat(800));
    check(&format!("{tie}e-800"));
    check(&format!("{tie}1e-801"));
}

#[test]
fn random_literals_round_like_core() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..200_000 {
        let r = next();
        let literal = match r % 4 {
            // Shortest round-trip forms of random doubles.
            0 => format!("{:e}", f64::from_bits(next() & 0x7fef_ffff_ffff_ffff)),
            // Random digit strings with random exponents.
            1 => {
                let digits = 1 + (next() % 40) as usize;
                let text: String = (0..digits)
                    .map(|_| char::from(b'0' + (next() % 10) as u8))
                    .collect();
                format!("{text}e{}", (next() % 700) as i64 - 350)
            }
            // Long expansions of random doubles, cut at a random length.
            2 => {
                let x = f64::from_bits(next() & 0x7fef_ffff_ffff_ffff);
                format!("{:.*e}", 15 + (next() % 40) as usize, x)
            }
            _ => format!("{}", (next() % 1_000_000_000) as f64 / 1000.0),
        };
        check(&literal);
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
