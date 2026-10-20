//! Strings in compiled core modules: scalar positions, UTF-8 ordering, Unicode casing,
//! normalization, append and split sharing, and the allocations they avoid.
use crate::core_program::{Counter, Program};
use std::fmt::Write;

#[test]
fn normalization_preserves_complete_combining_segments() {
    let mut program = Program::checked(
        r#"
export function boundaries(n: number): number {
  const marks = '\u0315'.repeat(n);
  const source = 'a' + marks + '\u0300';
  if (source.normalize('NFD') !== 'a\u0300' + marks) return -1;
  if (source.normalize('NFC') !== '\u00e0' + marks) return -2;
  if (source.normalize('NFKD') !== 'a\u0300' + marks) return -3;
  if (source.normalize('NFKC') !== '\u00e0' + marks) return -4;
  if (source.normalize() !== '\u00e0' + marks) return -5;

  const leading = marks + '\u0301\u0300';
  const sameClass = '\u0301\u0300'.repeat(n);
  const blocked = 'a' + '\u0305'.repeat(n + 1) + '\u0300';
  const forms = ['NFD', 'NFC', 'NFKD', 'NFKC'];
  for (let i = 0; i < forms.length; i++) {
    const form = forms[i];
    if (leading.normalize(form) !== '\u0301\u0300' + marks) return -6;
    if (('x' + sameClass + '\u0323').normalize(form) !== 'x\u0323' + sameClass) return -7;
    if (blocked.normalize(form) !== blocked) return -8;
  }

  const lower = '\u0334'.repeat(n);
  if (('a' + lower + '\u0300').normalize('NFC') !== '\u00e0' + lower) return -9;
  if (('a' + marks + '\u0344').normalize('NFD') !== 'a\u0308\u0301' + marks) return -10;
  if (('a' + marks + '\u0344').normalize('NFC') !== '\u00e4\u0301' + marks) return -11;
  if (('\uff41' + marks + '\u0300').normalize('NFKD') !== 'a\u0300' + marks) return -12;
  if (('\uff41' + marks + '\u0300').normalize('NFKC') !== '\u00e0' + marks) return -13;

  const sequence = (leading + source + '\u1100\u1161\u11a8').repeat(3);
  const composed = ('\u0301\u0300' + marks + '\u00e0' + marks + '\uac01').repeat(3);
  if (sequence.normalize('NFC') !== composed) return -14;
  const result = source.normalize('NFC');
  if (result.length !== n + 1 || result.charAt(0) !== '\u00e0') return -15;
  if (source !== 'a' + marks + '\u0300') return -16;
  return n;
}

export function scaling(n: number): number {
  const source = 'x' + '\u0315\u0300'.repeat(n);
  const normalized = source.normalize('NFD');
  if (normalized !== 'x' + '\u0300'.repeat(n) + '\u0315'.repeat(n)) return -1;
  return normalized.length;
}
"#,
        "normalization.ts",
    );
    program.stress_collection(true);
    for n in [0, 1, 62, 63, 64, 65, 126, 127, 128, 129, 1024] {
        assert_eq!(program.number("boundaries", f64::from(n)), f64::from(n));
    }
    let fuel = [2048, 8192].map(|n| {
        let (result, consumed) =
            program.call("scaling", &[wasmtime::Val::F64(f64::from(n).to_bits())]);
        assert_eq!(result[0].unwrap_f64(), f64::from(2 * n + 1));
        consumed
    });
    assert!(
        fuel[1] < fuel[0] * 6,
        "combining-run sorting must not grow quadratically: {fuel:?}"
    );
    let memory = program.memory_bytes();
    for _ in 0..3 {
        assert_eq!(program.number("scaling", 8192.0), 16385.0);
        assert!(program.counter(Counter::LiveBytes) < 1024);
    }
    assert_eq!(
        program.memory_bytes(),
        memory,
        "scratch storage is reclaimed"
    );
    eprintln!(
        "normalization fuel (2048, 8192 pairs): {fuel:?}; memory: {memory}; checked module: {} bytes",
        program.wasm.len()
    );
}

#[test]
fn append_and_builders_reuse_storage_and_preserve_aliases() {
    let mut program = Program::checked(
        r#"
let saved = '';
let escaped = '';
function keep(value: string): number { escaped = value; return value.length; }

function strip(text: string): string {
  let result = '';
  let inSpace = false;
  for (let i = 0; i < text.length; i++) {
    const ch = text.charAt(i);
    if (ch === ' ') {
      if (!inSpace && result.length > 0) { result += ' '; inSpace = true; }
    } else { result += ch; inSpace = false; }
  }
  if (result.length > 0 && result.charAt(result.length - 1) === ' ') {
    result = result.slice(0, result.length - 1);
  }
  return result;
}

export function retained_run(n: number): number {
  let text = 'é🙂' + String(n);
  saved = text.slice(0, 2);
  for (let i = 0; i < n; i++) text += '漢';
  if (saved !== 'é🙂' || text.charAt(text.length - 1) !== '漢') return -1;
  return text.length;
}
export function retained(n: number): number { return saved.length + saved.codePointAt(1) + n; }

export function astral(n: number): number {
  let text = '🙂é';
  const prefix = text + '!';
  text = prefix;
  for (let i = 0; i < n; i++) { text = text + '漢'; }
  if (prefix !== '🙂é!') { throw new Error('retained prefix changed'); }
  if (text.slice(0, 2) !== '🙂é') { throw new Error('scalar slice'); }
  if (text.charAt(0) !== '🙂') { throw new Error('scalar character'); }
  if (text.codePointAt(text.length) !== undefined) { throw new Error('out of bounds'); }
  return text.length + text.indexOf('!') + text.codePointAt(0);
}

export function owned(n: number): number {
  let text = '';
  const kept: string[] = [];
  for (let i = 0; i < n; i++) {
    if (i === 10 || i === 100) kept.push(text);
    if (i === 20) keep(text);
    text = text + 'é😀x';
    if (text[text.length - 2] !== '😀') return -1;
    if (text[text.length - 1] !== 'x') return -2;
  }
  if (kept[0].length !== 30 || kept[1].length !== 300 || escaped.length !== 60) return -3;
  if (kept[0][28] !== '😀' || kept[1][299] !== 'x') return -4;
  const prefix = text.slice(0, 17);
  text = text + '尾';
  if (prefix.length !== 17 || prefix[16] !== '😀') return -5;
  let self = 'é😀';
  for (let i = 0; i < 5; i++) self = self + self;
  if (self.length !== 64 || self[63] !== '😀') return -6;
  let left = 'é' + '😀';
  let right = left + '';
  right = right + 'x';
  if (left !== 'é😀' || right !== 'é😀x') return -7;
  let emptyLeft = '' + left;
  emptyLeft = emptyLeft + 'z';
  if (left !== 'é😀' || emptyLeft !== 'é😀z') return -8;
  let merged = '';
  for (let i = 0; i < 100; i++) {
    const old = merged;
    if (i % 2 === 0) merged = merged + 'a';
    else merged = merged + '😀';
    if (old.length !== i) return -9;
  }
  if (merged.length !== 100 || merged[99] !== '😀') return -10;
  return text.length;
}

export function exclusive(n: number): number {
  let text = '';
  for (let i = 0; i < n; i++) text = text + 'é😀x';
  return text.length;
}

export function consumed_run(n: number): number {
  const input = 'a- b'.repeat(n / 4);
  let text = '';
  for (let i = 0; i < input.length; i++) {
    const character = input.charAt(i);
    text += character === '-' || character === ' ' ? '_' : character;
  }
  if (text !== 'a__b'.repeat(n / 4)) return -1;
  return JSON.stringify(text).length;
}
export function consumed_aliases(n: number): number {
  const prefix = 'é' + String(n);
  let left = prefix;
  let right = prefix;
  if (n > 0) { left += 'a'; right += 'b'; }
  else { left += 'x'; right += 'y'; }
  if (left !== prefix + (n > 0 ? 'a' : 'x')) return -1;
  if (right !== prefix + (n > 0 ? 'b' : 'y')) return -2;
  return 1;
}

export function returned_run(n: number): number {
  const result = strip(' x '.repeat(n));
  if (result.charAt(0) !== 'x' || result.charAt(result.length - 1) !== 'x') return -1;
  return result.length;
}
export function returned_aliases(n: number): number {
  const original = 'é😀' + String(n);
  let text = original;
  if (n > 0) text = text + 'x';
  for (let i = 0; i < 20; i++) text = text + 'z';
  if (original !== 'é😀' + String(n)) return -1;
  if (text !== original + (n > 0 ? 'x' : '') + 'z'.repeat(20)) return -2;
  let built = '';
  for (let i = 0; i < 20; i++) built += 'é😀';
  const view = built.slice(0, 3);
  built += '尾';
  if (view !== 'é😀é' || view.charAt(1) !== '😀' || built.length !== 41) return -3;
  return 1;
}

export async function async_builders(n: number): Promise<number> {
  const input = 'aé😀\0';
  const prefixes: string[] = [];
  let output = '';
  for (let i = 0; i < n; i++) {
    prefixes.push(output);
    const c = input.charAt(i % 4);
    await Promise.resolve(i);
    output += c;
    output += input.charAt(-1);
  }
  let sum = 0;
  for (let i = 0; i < prefixes.length; i++) {
    const prefix = prefixes[i];
    if (prefix === undefined || prefix.length !== i) throw 1;
    for (let j = 0; j < prefix.length; j++) {
      if (prefix.charAt(j) !== input.charAt(j % 4)) throw 2;
    }
    sum += prefix.length;
  }
  return sum * 1000 + output.length;
}
export function sync_build(n: number): number {
  let output = '';
  for (let i = 0; i < n; i++) {
    const c = 'é😀'.charAt(i % 2);
    if (c !== '') output += c;
  }
  return output.length;
}
"#,
        "append-and-builders.ts",
    );
    program.stress_collection(true);
    for n in [1000.0, 100_000.0, 2000.0] {
        assert_eq!(
            program.number("retained_run", n),
            n + n.to_string().len() as f64 + 2.0
        );
        assert_eq!(program.number("retained", 0.0), 128_580.0);
        assert!(program.counter(Counter::LiveBytes) < 512);
    }

    assert_eq!(program.throwing("astral", 100_000.0), (228_583.0, 0));

    for n in [1000.0, 2000.0, 1000.0] {
        assert_eq!(program.throwing("owned", n), (3.0 * n + 1.0, 0));
    }

    let before = program.counter(Counter::Allocations);
    assert_eq!(program.number("exclusive", 1000.0), 3000.0);
    let allocations = program.counter(Counter::Allocations) - before;
    assert!(allocations < 100, "headers should be reused");

    let before = program.counter(Counter::Allocations);
    assert_eq!(program.number("consumed_run", 1000.0), 1002.0);
    let allocations = program.counter(Counter::Allocations) - before;
    assert!(
        allocations < 100,
        "consumed builder allocated {allocations}"
    );
    for n in [-1.0, 0.0, 1.0] {
        assert_eq!(program.number("consumed_aliases", n), 1.0);
    }

    let before = program.counter(Counter::Allocations);
    assert_eq!(program.number("returned_run", 1000.0), 1999.0);
    let allocations = program.counter(Counter::Allocations) - before;
    assert!(allocations < 100, "{allocations:?}");
    for n in [-1.0, 0.0, 1.0] {
        assert_eq!(program.number("returned_aliases", n), 1.0);
    }

    for n in [0.0, 1.0, 17.0, 80.0] {
        assert_eq!(
            program.throwing("async_builders", n),
            (n * (n - 1.0) / 2.0 * 1000.0 + n, 0)
        );
    }
    program.stress_collection(false);
    let before = program.counter(Counter::Allocations);
    assert_eq!(program.number("sync_build", 20_000.0), 20_000.0);
    let after = program.counter(Counter::Allocations);
    assert!(after - before < 30);
}

#[test]
fn regex_search_replacements_and_named_captures() {
    let mut program = Program::checked(
        r#"
let calls = 0;
function replacement(match: string, first: string | undefined, second: string | undefined, offset: number, input: string): string {
  calls++;
  return '[' + match + ':' + first + ':' + second + ':' + offset + ':' + input.length + ']';
}
function probe(text: string): string {
  const match = /(?<word>a)/.exec(text);
  return match?.groups?.word ?? 'missing';
}
export function search(n: number): number {
  const expression = /[a-z]+\d{2}/u;
  const text = '🙂x42';
  let total = 0;
  for (let i = 0; i < n; i++) total = total + text.search(expression) + (expression.test(text) ? 1 : 0);
  return total;
}
export function replace(n: number): number {
  const map: Record<string, string> = {a: 'A', b: 'B'};
  if ('caba'.replace(/[ab]/g, ch => map[ch]) !== 'cABA') return -1;
  calls = 0;
  if ('ab a'.replace(/(a)(b)?/g, replacement) !== '[ab:a:b:0:4] [a:a:undefined:3:4]' || calls !== 2) return -2;
  const alias = (match: string, offset: number, input: string) => '$&' + offset + input.length;
  if ('aba'.replace('a', alias) !== '$&03ba') return -3;
  if ('aba'.replaceAll('a', alias) !== '$&03b$&23') return -4;
  if ('ab'.replaceAll('', (_, offset) => String(offset)) !== '0a1b2') return -5;
  if ('ab'.replace(/(?:)/g, (_, offset) => String(offset)) !== '0a1b2') return -6;
  if (''.replace(/(?:)/g, () => 'x') !== 'x') return -7;
  if ('aéa'.replace(/a/g, (_, offset) => String(offset)) !== '0é2') return -8;
  calls = 0;
  if ('xxx'.replace(/a/g, () => { calls++; return '!'; }) !== 'xxx' || calls !== 0) return -9;
  let source = 'aba';
  const result = source.replace(/a/g, (_, offset, original) => {
    source = 'changed';
    return String(offset) + original;
  });
  if (source !== 'changed' || result !== '0abab2aba') return -10;
  calls = 0;
  try { 'aaa'.replaceAll(/a/g, () => { calls++; if (calls === 2) throw new Error('stop'); return 'x'; }); }
  catch (error) { if (error.message !== 'stop' || calls !== 2) return -11; }
  if (calls !== 2) return -12;
  if ('ab'.replace(/[ab]/g, () => 7) !== '77') return -13;
  return n;
}
export function named(n: number): number {
  if (probe('a') !== 'a' || probe('b') !== 'missing') return -1;
  const result = /(?<word>a)(?<tail>b)?/.exec('xa');
  if (result === null || result.groups === undefined) return -2;
  const groups = result.groups;
  if (groups.word !== 'a' || groups.tail !== undefined || result[2] !== undefined) return -3;
  if (result.index !== 1 || result.input !== 'xa' || result.length !== 3 || result.join('|') !== 'a|a|') return -4;
  result[1] = 'changed';
  for (let i = 0; i < 20; i++) result.push('padding');
  if (groups.word !== 'a' || result.groups.word !== 'a') return -5;
  const unnamed = /(a)/.exec('a');
  if (unnamed?.groups !== undefined) return -6;
  const matched = 'zab'.match(/(?<letter>a)(?<tail>b)/);
  if (matched?.groups?.letter !== 'a') return -7;
  const regex = /(?<letter>a)(b)?/g;
  if ('ab a'.replace(regex, (match, first, second, offset, input, groups) =>
      groups.letter + ':' + second + ':' + offset + ':' + input.length) !== 'a:b:0:4 a:undefined:3:4') return -8;
  if ('ab a'.replace(/(?<first>a)(?<last>b)?/g, '$<last>:$<first>:$<missing>') !== 'b:a: :a:') return -9;
  if ('a'.replace(/(a)/, '$<first>') !== '$<first>') return -10;
  return n;
}
"#,
        "regex.ts",
    );
    assert_eq!(program.number("search", 20_000.0), 40_000.0);
    assert_eq!(program.counter(Counter::Allocations), 0);

    program.stress_collection(true);
    assert_eq!(program.throwing("replace", 17.0), (17.0, 0));
    assert_eq!(program.throwing("named", 23.0), (23.0, 0));
}

#[test]
fn ascii_split_and_casing_sharing_avoid_allocations() {
    let mut program = Program::checked(
        r#"
export function ascii(n: number): number {
  const text = 'é' + 'x'.repeat(n) + '😀';
  let sum = 0;
  for (let i = 1; i <= n; i++) {
    const ch = text.charAt(i);
    if (ch !== 'x' || ch < 'w' || ch > 'y') return -1;
    sum += 'é😀xyzx'.indexOf(ch) + 'é😀xyzx'.lastIndexOf(ch);
  }
  if (text.charAt(0) !== 'é' || text.charAt(n + 1) !== '😀') return -2;
  return sum;
}
export function split_run(n: number): number {
  const text = 'x'.repeat(n);
  const pieces = text.split('');
  let total = 0;
  for (let i = 0; i < pieces.length; i++) {
    if (pieces[i] !== 'x') return -1;
    total += 1;
  }
  return total;
}
export function split_aliases(n: number): number {
  let source = 'é😀' + String(n);
  const pieces = source.split(',');
  source += '尾';
  if (pieces.length !== 1 || pieces[0] !== 'é😀' + String(n)) return -1;
  let piece = pieces[0];
  if (piece === undefined) return -2;
  piece += '!';
  if (pieces[0] !== 'é😀' + String(n) || source !== 'é😀' + String(n) + '尾') return -3;
  const chars = 'é,x,😀,'.split(',');
  if (chars.length !== 4 || chars[0] !== 'é' || chars[1] !== 'x' || chars[2] !== '😀' || chars[3] !== '') return -4;
  if (''.split(',').length !== 1 || ''.split('').length !== 0) return -5;
  return piece.length;
}
export function casing_run(n: number): number {
  const lower = 'ä🙂already lowercase'; const upper = 'Ä🙂ALREADY UPPERCASE';
  let total = 0;
  for (let i = 0; i < n; i++) total += lower.toLowerCase().length + upper.toUpperCase().length;
  return total;
}
export function casing_aliases(n: number): number {
  let source = 'ä🙂' + String(n);
  let lower = source.toLowerCase(); const kept = lower;
  source += 'X'; lower += '!';
  if (source !== 'ä🙂' + String(n) + 'X' || kept !== 'ä🙂' + String(n)) return -1;
  return lower === 'ä🙂' + String(n) + '!' ? 1 : 0;
}
"#,
        "sharing.ts",
    );
    program.stress_collection(true);
    let before = program.counter(Counter::Allocations);
    assert_eq!(program.number("ascii", 1000.0), 7000.0);
    let allocations = program.counter(Counter::Allocations) - before;
    assert!(allocations < 20, "{allocations:?}");

    let before = program.counter(Counter::Allocations);
    assert_eq!(program.number("split_run", 1000.0), 1000.0);
    let allocations = program.counter(Counter::Allocations) - before;
    assert!(allocations < 20, "{allocations:?}");
    assert_eq!(program.number("split_aliases", 1.0), 4.0);

    let before = program.counter(Counter::Allocations);
    assert_eq!(program.number("casing_run", 1000.0), 38000.0);
    assert!(program.counter(Counter::Allocations) - before < 20);
    for n in 0..3 {
        assert_eq!(program.number("casing_aliases", f64::from(n)), 1.0);
    }
}

#[test]
fn short_literal_and_scalar_character_comparisons_preserve_utf8_order() {
    let samples = [
        "",
        "\0",
        " ",
        "a",
        "aa",
        "abc",
        "abcd",
        "abcde",
        "abcdef",
        "abcdefg",
        "abcdefgh",
        "abcdefghi",
        "é",
        "😀",
        "é😀",
        "a\0",
        "ä",
    ];
    let literals = [
        "", "\0", " ", "a", "abcd", "abcde", "abcdef", "abcdefg", "abcdefgh", "é😀",
    ];
    let text = "a\0é😀z\u{7f}\u{80}\u{7ff}\u{800}\u{d7ff}\u{e000}\u{ffff}\u{10000}\u{10ffff}";
    let char_literals = ["", "\0", "a", "b", "é", "Ω", "😀", "😁"];
    let mut source = format!(
        "const samples: string[] = {};\n",
        serde_json::to_string(&samples).unwrap()
    );
    for (index, literal) in literals.iter().enumerate() {
        let literal = serde_json::to_string(literal).unwrap();
        source += &format!(
            "export function compare{index}(n: number): number {{ const s = samples[n]; if (s === undefined) return -1; return {}; }}\n",
            comparison_expression("s", &literal)
        );
    }
    for (index, literal) in char_literals.iter().enumerate() {
        let literal = serde_json::to_string(literal).unwrap();
        source += &format!(
            "export function check{index}(n: number): number {{ const c = {}.charAt(n); return {} + c.length * 4096; }}\n",
            serde_json::to_string(text).unwrap(),
            comparison_expression("c", &literal)
        );
    }
    let mut program = Program::new(&source);
    for (index, literal) in literals.iter().enumerate() {
        for (position, sample) in samples.iter().enumerate() {
            assert_eq!(
                program.number(&format!("compare{index}"), position as f64),
                f64::from(comparison_mask(sample, literal)),
                "{sample:?} against {literal:?}"
            );
        }
        assert_eq!(program.number(&format!("compare{index}"), -1.0), -1.0);
    }
    for position in [
        f64::NEG_INFINITY,
        -2.0,
        -0.5,
        0.0,
        0.75,
        1.0,
        2.0,
        3.0,
        4.0,
        5.0,
        f64::INFINITY,
        f64::NAN,
    ]
    .into_iter()
    .chain((0..text.chars().count()).map(|index| index as f64))
    {
        let sample = if position.is_nan() {
            Some('a')
        } else if position.trunc() >= 0.0 && position < text.chars().count() as f64 {
            text.chars().nth(position as usize)
        } else {
            None
        };
        let sample = sample.map_or_else(String::new, |c| c.to_string());
        for (index, &literal) in char_literals.iter().enumerate() {
            let expected =
                comparison_mask(&sample, literal) + if sample.is_empty() { 0 } else { 4096 };
            assert_eq!(
                program.number(&format!("check{index}"), position),
                f64::from(expected),
                "{position} against {literal:?}"
            );
        }
    }
}

fn comparison_expression(variable: &str, literal: &str) -> String {
    let mut expression = String::new();
    for (bit, operator) in ["===", "!==", "<", "<=", ">", ">="].iter().enumerate() {
        if bit != 0 {
            expression.push_str(" + ");
        }
        let _ = write!(
            expression,
            "({variable} {operator} {literal} ? {} : 0) + ({literal} {operator} {variable} ? {} : 0)",
            1 << bit,
            1 << (bit + 6)
        );
    }
    expression
}

fn comparison_mask(left: &str, right: &str) -> i32 {
    let mut mask = 0;
    for (shift, (a, b)) in [(0, (left, right)), (6, (right, left))] {
        for (bit, value) in [a == b, a != b, a < b, a <= b, a > b, a >= b]
            .into_iter()
            .enumerate()
        {
            mask |= i32::from(value) << (shift + bit);
        }
    }
    mask
}
