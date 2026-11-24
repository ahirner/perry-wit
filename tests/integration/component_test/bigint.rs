use super::*;

#[test]
fn numeric_bigint_conversion_preserves_transport_bits_and_checks_domain() -> Result<()> {
    let mut component = Instance::compile(
        "package test:bigint-conversion; world boundary { export signed: func(value: f64) -> result<s64, string>; export unsigned: func(value: f64) -> result<u64, string>; }",
        r#"
        export function signed(value: number) {
            try { return BigInt(value); }
            catch (error) { if (error instanceof RangeError) throw 'range'; throw 'unexpected'; }
        }
        export function unsigned(value: number) { return signed(value); }
        "#,
    )?;
    for value in [0i64, -1, 9007199254740992, i64::MIN] {
        component.check("signed", (value as f64,), &Ok::<_, String>(value))?;
    }
    let wide = u64::MAX - 2047;
    component.check("unsigned", (wide as f64,), &Ok::<_, String>(wide))?;
    let range_error = Err::<i64, _>("range".to_owned());
    for value in [0.5, f64::NAN, f64::INFINITY, 18446744073709551616.0] {
        component.check("signed", (value,), &range_error)?;
    }
    Ok(())
}

#[test]
fn bigint_arithmetic_is_exact_beyond_transport_widths() -> Result<()> {
    let wit = r#"package test:bigint-arithmetic;
world boundary {
    export describe: func(a: s64, b: u64) -> string;
    export powers: func(n: u32, seed: u64) -> string;
    export unsigned: func(a: u64, b: u64) -> u64;
    export signed: func(a: s64, b: s64) -> s64;
    export rounding: func(offset: u64) -> f64;
    export probe: func(input: string) -> string;
    export truncation: func(value: u64) -> string;
    export literals: func() -> string;
}"#;
    let source = r#"
import type { u64 } from 'perry:types';
function sum(a: bigint, b: bigint) { return a + b; }
function difference(a: bigint, b: bigint) { return a - b; }
export function describe(a: bigint, b: bigint) {
    const total = sum(a, b);
    const delta = difference(a, b);
    if (difference(total, b) !== a || sum(delta, b) !== a) throw 'inverse';
    if ((delta < total) !== (b > 0n)) throw 'order';
    return String(total) + ':' + String(delta) + ':' + String(-total) + ':' + String(a) + ':' + String(b);
}
export function powers(n: number, seed: bigint) {
    let value = seed;
    for (let i = 0; i < n; i++) value = sum(value, value);
    const below = difference(value, 1n);
    if (sum(below, 1n) !== value || difference(value, value) !== 0n) throw 'carry';
    return String(value) + ':' + String(below) + ':' + String(Number(value));
}
export function unsigned(a: bigint, b: bigint) { return difference(sum(a, b), b); }
export function signed(a: bigint, b: bigint) { return sum(difference(a, b), b); }
export function rounding(offset: bigint) {
    const wide = 18446744073709551616n;
    return Number(sum(wide, offset));
}
export function probe(input: string) {
    const a: bigint = BigInt(input.length);
    const b: bigint = 1n;
    return String(a + b) + ':' + String(a - b);
}
export function truncation(value: bigint) {
    const total = value + 1n;
    const wrapped = total as u64;
    return String(total) + ':' + String(wrapped);
}
export function literals() {
    const wide = 340_282_366_920_938_463_463_374_607_431_768_211_456n;
    if (wide !== 0x100000000000000000000000000000000n) throw 'hex';
    if (18446744073709551616n !== 0o2000000000000000000000n) throw 'octal';
    if (18446744073709551616n !== 0b10000000000000000000000000000000000000000000000000000000000000000n) throw 'binary';
    if (wide - 1n !== 340282366920938463463374607431768211455n) throw 'carry';
    return String(wide) + ':' + String(-wide);
}
"#;
    let mut instance = Instance::compile(wit, source)?;
    instance.check(
        "literals",
        (),
        &"340282366920938463463374607431768211456:-340282366920938463463374607431768211456"
            .to_owned(),
    )?;
    for a in [i64::MIN, -9007199254740993, -1, 0, 1, i64::MAX] {
        for b in [0, 1, 9007199254740993, u64::MAX] {
            let (left, right) = (i128::from(a), i128::from(b));
            instance.check(
                "describe",
                (a, b),
                &format!(
                    "{}:{}:{}:{a}:{b}",
                    left + right,
                    left - right,
                    -(left + right)
                ),
            )?;
        }
    }
    for value in [0, 1, 9007199254740993, u64::MAX] {
        instance.check("unsigned", (value, u64::MAX), &value)?;
    }
    for value in [i64::MIN, -1, 0, i64::MAX] {
        instance.check("signed", (value, i64::MAX), &value)?;
    }
    for (power, expected) in [
        (
            64u32,
            "18446744073709551616:18446744073709551615:18446744073709552000",
        ),
        (
            128,
            "340282366920938463463374607431768211456:340282366920938463463374607431768211455:3.402823669209385e+38",
        ),
    ] {
        instance.check("powers", (power, 1u64), &expected.to_owned())?;
    }
    // 2^1536 exceeds the bounded number-formatting utility's capacity and overflows Number.
    let huge: String = instance.call("powers", (1536u32, 1u64))?;
    let (power, below) = huge
        .strip_suffix(":Infinity")
        .and_then(|s| s.split_once(':'))
        .ok_or_else(|| anyhow::anyhow!("invalid large bigint result"))?;
    assert_eq!(power.len(), 463);
    assert_eq!(&power[..power.len() - 1], &below[..below.len() - 1]);
    assert!(power.ends_with('6') && below.ends_with('5'));
    for offset in [0u64, 2048, 2049, 6144] {
        instance.check(
            "rounding",
            (offset,),
            &((u128::from(u64::MAX) + 1 + u128::from(offset)) as f64),
        )?;
    }
    for (input, expected) in [("", "1:-1"), ("abc", "4:2")] {
        instance.check("probe", (input,), &expected.to_owned())?;
    }
    instance.check(
        "truncation",
        (u64::MAX,),
        &"18446744073709551616:0".to_owned(),
    )?;
    Ok(())
}

#[test]
fn bigint_boundary_rejects_results_outside_transport_range() -> Result<()> {
    let mut instance = Instance::compile(
        "package test:bigint-overflow; world boundary { export unsigned: func(value: u64) -> u64; export signed: func(value: s64) -> s64; export constant: func() -> u64; export reinterpret: func(value: s64) -> u64; }",
        "export function unsigned(value: bigint) { return value + 1n; } export function signed(value: bigint) { return value - 1n; } export function constant() { return 18446744073709551616n; } export function reinterpret(value: bigint) { return value; }",
    )?;
    assert!(instance.call::<_, u64>("unsigned", (u64::MAX,)).is_err());
    assert!(instance.call::<_, i64>("signed", (i64::MIN,)).is_err());
    assert!(instance.call::<_, u64>("constant", ()).is_err());
    // A bigint kept in an `i64` register is checked like one on the heap.
    assert!(instance.call::<_, u64>("reinterpret", (-1i64,)).is_err());
    Ok(())
}
