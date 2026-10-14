//! Correctly rounded decimal-to-double conversion for JSON number literals, small enough for
//! the helper: `core`'s `str::parse::<f64>` costs about 18 KB of code and tables.
//!
//! A literal is `D × 10^e` with `D` its significant digits. When `D < 2^53` and `10^|e|` is
//! exact (Clinger's fast path, nearly every literal in practice) one multiplication or
//! division rounds correctly. Otherwise an approximation from the leading 19 digits is
//! corrected by exact comparison against the midpoints of neighbouring doubles, on a stack
//! big integer. Digits after the 780th only decide ties: a midpoint has at most 767
//! significant digits, so they act as one sticky digit.
pub(super) use perry_abi::number::Decimal;

#[cfg(test)]
fn parse_number(literal: &[u8]) -> f64 {
    let negative = literal.first() == Some(&b'-');
    let body = &literal[usize::from(negative)..];
    let mut at = 0;
    let mut fractional = false;
    let mut decimal = Decimal::default();
    while at < body.len() && !matches!(body[at], b'e' | b'E') {
        let byte = body[at];
        at += 1;
        if byte == b'.' {
            fractional = true;
        } else {
            decimal.digit(byte, fractional);
        }
    }
    decimal.finish(
        &body[..at],
        negative,
        perry_abi::number::parse_exponent(&body[at..]),
    )
}

#[cfg(test)]
mod tests {
    use super::parse_number;

    fn check(literal: &str) {
        let expected: f64 = literal.parse().unwrap();
        let actual = parse_number(literal.as_bytes());
        let mut reader = super::super::reader::Reader::new(literal);
        let unsigned = literal.trim_start_matches('-').as_bytes();
        let valid_json =
            unsigned[0] != b'0' || matches!(unsigned.get(1), None | Some(b'.' | b'e' | b'E'));
        let fused = reader.numeric().expect("generated decimal grammar");
        if valid_json {
            assert_eq!(reader.position, literal.len(), "fused token {literal}");
            assert_eq!(fused.to_bits(), expected.to_bits(), "fused {literal}");
        } else {
            assert!(
                reader.position < literal.len(),
                "JSON rejects leading zeros: {literal}"
            );
        }
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
}
