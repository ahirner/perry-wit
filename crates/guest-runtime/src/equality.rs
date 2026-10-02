use std::borrow::Cow;

use crate::nanbox::{POINTER_TAG, STRING_TAG, TAG_FALSE, TAG_NULL, TAG_TRUE, TAG_UNDEFINED};
use crate::state::RuntimeState;

#[derive(PartialEq)]
enum Value<'a> {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(Cow<'a, [u16]>),
    Object(i64),
}

fn decode(state: &RuntimeState, value: i64) -> Value<'_> {
    match value as u64 {
        TAG_UNDEFINED => Value::Undefined,
        TAG_NULL => Value::Null,
        TAG_TRUE => Value::Bool(true),
        TAG_FALSE => Value::Bool(false),
        bits if bits >> 48 == STRING_TAG => Value::String(Cow::Borrowed(
            state
                .strings
                .get((bits & 0xffff_ffff) as usize)
                .map(Vec::as_slice)
                .unwrap_or_default(),
        )),
        bits if bits >> 48 == POINTER_TAG => Value::Object(value),
        bits => Value::Number(f64::from_bits(bits)),
    }
}

pub(crate) fn string_number(value: &str) -> f64 {
    let value = value.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    match value {
        "" => return 0.0,
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefixes, radix) in [(["0x", "0X"], 16), (["0o", "0O"], 8), (["0b", "0B"], 2)] {
        if let Some(digits) = prefixes
            .iter()
            .find_map(|prefix| value.strip_prefix(prefix))
        {
            if digits.is_empty() {
                return f64::NAN;
            }
            return digits
                .chars()
                .try_fold(0.0, |number, digit| {
                    digit
                        .to_digit(radix)
                        .map(|digit| number * radix as f64 + digit as f64)
                })
                .unwrap_or(f64::NAN);
        }
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | '+' | '-' | 'e' | 'E'))
    {
        return f64::NAN;
    }
    value.parse().unwrap_or(f64::NAN)
}

fn object_primitive(state: &RuntimeState, object: i64) -> Value<'static> {
    Value::String(Cow::Owned(
        state.coerce_string(object).encode_utf16().collect(),
    ))
}

fn loose_equal(state: &RuntimeState, left: Value<'_>, right: Value<'_>) -> bool {
    match (left, right) {
        (Value::Null, Value::Undefined) | (Value::Undefined, Value::Null) => true,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Bool(value), other) => {
            loose_equal(state, Value::Number(u8::from(value) as f64), other)
        }
        (other, Value::Bool(value)) => {
            loose_equal(state, other, Value::Number(u8::from(value) as f64))
        }
        (Value::Number(number), Value::String(string))
        | (Value::String(string), Value::Number(number)) => {
            number == string_number(&String::from_utf16_lossy(&string))
        }
        (Value::Object(object), other @ (Value::String(_) | Value::Number(_))) => {
            loose_equal(state, object_primitive(state, object), other)
        }
        (other @ (Value::String(_) | Value::Number(_)), Value::Object(object)) => {
            loose_equal(state, other, object_primitive(state, object))
        }
        (left, right) => left == right,
    }
}

pub(crate) fn equal(state: &RuntimeState, left: i64, right: i64, loose: bool) -> bool {
    let (left, right) = (decode(state, left), decode(state, right));
    if loose {
        loose_equal(state, left, right)
    } else {
        left == right
    }
}
