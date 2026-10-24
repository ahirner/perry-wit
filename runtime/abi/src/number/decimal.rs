use super::{Decimal, parse_exponent};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DecimalSyntax {
    Json,
    Prefix,
}

/// A borrowed token and the conversion facts collected during its lexical scan.
pub struct DecimalLiteral<'a> {
    token: &'a [u8],
    pub mantissa: &'a [u8],
    pub decimal: Decimal,
    pub exponent: i64,
    pub consumed: usize,
}

impl DecimalLiteral<'_> {
    pub fn float(&self, negative: bool) -> Option<f64> {
        let value: f64 = core::str::from_utf8(self.token).ok()?.parse().ok()?;
        Some(if negative { -value } else { value })
    }
}

pub fn split_sign(input: &[u8]) -> (bool, &[u8]) {
    match input.first() {
        Some(b'-') => (true, &input[1..]),
        Some(b'+') => (false, &input[1..]),
        _ => (false, input),
    }
}

/// Scan an unsigned decimal, reporting syntax errors with the byte offset of the expected digit.
pub fn scan_decimal<const EXACT: bool>(
    input: &[u8],
    syntax: DecimalSyntax,
) -> Result<DecimalLiteral<'_>, usize> {
    let mut at = 0;
    let mut decimal = Decimal::default();
    let mut digits = |at: &mut usize, fractional| {
        let start = *at;
        while let Some(byte @ b'0'..=b'9') = input.get(*at) {
            if EXACT {
                decimal.digit(*byte, fractional);
            }
            *at += 1;
        }
        *at - start
    };
    let integer_digits = if syntax == DecimalSyntax::Json && input.first() == Some(&b'0') {
        at = 1;
        1
    } else {
        digits(&mut at, false)
    };
    if syntax == DecimalSyntax::Json && integer_digits == 0 {
        return Err(at);
    }
    let fraction_digits = if input.get(at) == Some(&b'.') {
        at += 1;
        let count = digits(&mut at, true);
        if syntax == DecimalSyntax::Json && count == 0 {
            return Err(at);
        }
        count
    } else {
        0
    };
    if integer_digits + fraction_digits == 0 {
        return Err(at);
    }
    let mantissa_end = at;
    let mut exponent = 0;
    if matches!(input.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(input.get(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let start = at;
        while matches!(input.get(at), Some(b'0'..=b'9')) {
            at += 1;
        }
        if at == start {
            if syntax == DecimalSyntax::Json {
                return Err(at);
            }
            at = mantissa_end;
        } else if EXACT {
            exponent = parse_exponent(&input[mantissa_end..at]);
        }
    }
    Ok(DecimalLiteral {
        token: &input[..at],
        mantissa: &input[..mantissa_end],
        decimal,
        exponent,
        consumed: at,
    })
}
