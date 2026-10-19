//! Strings in compiled core modules: scalar positions, UTF-8 ordering, Unicode casing,
//! append and split sharing, and the allocations they avoid.
use crate::core_program::{Counter, Program};
use std::fmt::Write;

#[test]
fn retained_prefix_does_not_keep_later_append_storage_alive() {
    let mut program = Program::checked(
        r#"
let saved = '';
export function run(n: number): number {
  let text = 'é🙂' + String(n);
  saved = text.slice(0, 2);
  for (let i = 0; i < n; i++) text += '漢';
  if (saved !== 'é🙂' || text.charAt(text.length - 1) !== '漢') return -1;
  return text.length;
}
export function retained(n: number): number { return saved.length + saved.codePointAt(1) + n; }
"#,
        "retained-append-prefix.ts",
    );
    program.stress_collection(true);
    for n in [1000.0, 100_000.0, 2000.0] {
        assert_eq!(
            program.number("run", n),
            n + n.to_string().len() as f64 + 2.0
        );
        assert_eq!(program.number("retained", 0.0), 128_580.0);
        assert!(program.counter(Counter::LiveBytes) < 512);
    }
}

#[test]
fn astral_strings_use_scalar_positions_and_keep_append_prefixes() {
    let mut program = Program::new(
        r#"
export function run(n: number): number {
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
"#,
    );
    assert_eq!(program.throwing("run", 100_000.0), (228_583.0, 0));
}

#[test]
fn literal_regex_search_allocates_nothing_and_uses_scalar_positions() {
    let source = r#"
export function run(n: number): number {
    const expression = /[a-z]+\d{2}/u;
    const text = '🙂x42';
    let total = 0;
    for (let i = 0; i < n; i++) total = total + text.search(expression) + (expression.test(text) ? 1 : 0);
    return total;
}
"#;
    let mut program = Program::checked(source, "regex.ts");
    assert_eq!(program.number("run", 20_000.0), 40_000.0);
    let allocations = program.counter(Counter::Allocations);
    assert_eq!(allocations, 0);
}

#[test]
fn replacement_callbacks_preserve_captures_order_and_abrupt_completion() {
    let mut program = Program::checked(
        r#"
let calls = 0;
function replacement(match: string, first: string | undefined, second: string | undefined, offset: number, input: string): string {
  calls++;
  return '[' + match + ':' + first + ':' + second + ':' + offset + ':' + input.length + ']';
}
export function run(n: number): number {
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
"#,
        "replace-callbacks.ts",
    );
    program.stress_collection(true);
    assert_eq!(program.throwing("run", 17.0), (17.0, 0));
}

#[test]
fn named_captures_keep_groups_separate_from_numbered_array_mutation() {
    let mut program = Program::checked(
        r#"
function probe(text: string): string {
  const match = /(?<word>a)/.exec(text);
  return match?.groups?.word ?? 'missing';
}
export function run(n: number): number {
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
        "named-regex-captures.ts",
    );
    program.stress_collection(true);
    assert_eq!(program.throwing("run", 23.0), (23.0, 0));
}

#[test]
fn owned_appends_preserve_observers_and_scalar_cursors() {
    let mut program = Program::checked(
        r#"
let escaped = '';
function keep(value: string): number { escaped = value; return value.length; }
export function run(n: number): number {
  let text = '';
  const saved: string[] = [];
  for (let i = 0; i < n; i++) {
    if (i === 10 || i === 100) saved.push(text);
    if (i === 20) keep(text);
    text = text + 'é😀x';
    if (text[text.length - 2] !== '😀') return -1;
    if (text[text.length - 1] !== 'x') return -2;
  }
  if (saved[0].length !== 30 || saved[1].length !== 300 || escaped.length !== 60) return -3;
  if (saved[0][28] !== '😀' || saved[1][299] !== 'x') return -4;
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
"#,
        "owned-append.ts",
    );
    program.stress_collection(true);
    for n in [1000.0, 2000.0, 1000.0] {
        assert_eq!(program.throwing("run", n), (3.0 * n + 1.0, 0));
    }
}

#[test]
fn exclusive_utf8_append_keeps_scalar_counts_with_few_allocations() {
    let mut program = Program::checked(
        r#"
export function run(n: number): number {
  let text = '';
  for (let i = 0; i < n; i++) text = text + 'é😀x';
  return text.length;
}
"#,
        "scalar-append.ts",
    );
    program.stress_collection(true);
    assert_eq!(program.number("run", 1000.0), 3000.0);
    let allocations = program.counter(Counter::Allocations);
    assert!(allocations < 100, "headers should be reused");
}

#[test]
fn finished_string_builders_transfer_ownership_to_consumers() {
    let mut program = Program::checked(
        r#"
export function run(n: number): number {
  const input = 'a- b'.repeat(n / 4);
  let text = '';
  for (let i = 0; i < input.length; i++) {
    const character = input.charAt(i);
    text += character === '-' || character === ' ' ? '_' : character;
  }
  if (text !== 'a__b'.repeat(n / 4)) return -1;
  return JSON.stringify(text).length;
}
export function aliases(n: number): number {
  const prefix = 'é' + String(n);
  let left = prefix;
  let right = prefix;
  if (n > 0) { left += 'a'; right += 'b'; }
  else { left += 'x'; right += 'y'; }
  if (left !== prefix + (n > 0 ? 'a' : 'x')) return -1;
  if (right !== prefix + (n > 0 ? 'b' : 'y')) return -2;
  return 1;
}
"#,
        "consumed-builder.ts",
    );
    program.stress_collection(true);
    assert_eq!(program.number("run", 1000.0), 1002.0);
    let allocations = program.counter(Counter::Allocations);
    assert!(
        allocations < 100,
        "consumed builder allocated {allocations}"
    );
    for n in [-1.0, 0.0, 1.0] {
        assert_eq!(program.number("aliases", n), 1.0);
    }
}

#[test]
fn returned_string_builders_reuse_headers_without_mutating_retained_aliases() {
    let mut program = Program::checked(
        r#"
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

export function run(n: number): number {
  const result = strip(' x '.repeat(n));
  if (result.charAt(0) !== 'x' || result.charAt(result.length - 1) !== 'x') return -1;
  return result.length;
}
export function aliases(n: number): number {
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
"#,
        "returned-builder.ts",
    );
    program.stress_collection(true);
    assert_eq!(program.number("run", 1000.0), 1999.0);
    let allocations = program.counter(Counter::Allocations);
    assert!(allocations < 100, "{:?}", allocations);
    for n in [-1.0, 0.0, 1.0] {
        assert_eq!(program.number("aliases", n), 1.0);
    }
}

#[test]
fn ascii_characters_in_unicode_text_do_not_allocate() {
    let mut program = Program::checked(
        r#"
export function run(n: number): number {
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
"#,
        "mixed-text.ts",
    );
    program.stress_collection(true);
    assert_eq!(program.number("run", 1000.0), 7000.0);
    let allocations = program.counter(Counter::Allocations);
    assert!(allocations < 20, "{allocations:?}");
}

#[test]
fn short_literal_comparisons_preserve_utf8_order_and_boundaries() {
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
}

#[test]
fn split_reuses_intact_strings_without_mutating_aliases() {
    let mut program = Program::checked(
        r#"
export function run(n: number): number {
  const text = 'x'.repeat(n);
  const pieces = text.split('');
  let total = 0;
  for (let i = 0; i < pieces.length; i++) {
    if (pieces[i] !== 'x') return -1;
    total += 1;
  }
  return total;
}
export function aliases(n: number): number {
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
"#,
        "split-sharing.ts",
    );
    program.stress_collection(true);
    assert_eq!(program.number("run", 1000.0), 1000.0);
    let allocations = program.counter(Counter::Allocations);
    assert!(allocations < 20, "{allocations:?}");
    assert_eq!(program.number("aliases", 1.0), 4.0);
}

#[test]
fn unchanged_casing_borrows_strings_without_changing_aliases() {
    let source = r#"
export function run(n:number):number {
 const lower='ä🙂already lowercase';const upper='Ä🙂ALREADY UPPERCASE';
 let total=0;
 for(let i=0;i<n;i++)total+=lower.toLowerCase().length+upper.toUpperCase().length;
 return total;
}
export function aliases(n:number):number {
 let source='ä🙂'+String(n);
 let lower=source.toLowerCase();const kept=lower;
 source+='X';lower+='!';
 if(source!=='ä🙂'+String(n)+'X'||kept!=='ä🙂'+String(n))return -1;
 return lower==='ä🙂'+String(n)+'!'?1:0;
}
"#;
    let mut program = Program::checked(source, "casing-sharing.ts");
    program.stress_collection(true);
    assert_eq!(program.number("run", 1000.0), 38000.0);
    assert!(program.counter(Counter::Allocations) < 20);
    for n in 0..3 {
        assert_eq!(program.number("aliases", f64::from(n)), 1.0);
    }
}

#[test]
fn scalar_characters_preserve_empty_indices_and_unicode_ordering() {
    let text = "a\0é😀z\u{7f}\u{80}\u{7ff}\u{800}\u{d7ff}\u{e000}\u{ffff}\u{10000}\u{10ffff}";
    let literals = ["", "\0", "a", "b", "é", "Ω", "😀", "😁"];
    let mut source = String::new();
    for (index, literal) in literals.iter().enumerate() {
        let literal = serde_json::to_string(literal).unwrap();
        source += &format!(
            "export function check{index}(n: number): number {{ const c = {}.charAt(n); return {} + c.length * 4096; }}\n",
            serde_json::to_string(text).unwrap(),
            comparison_expression("c", &literal)
        );
    }
    let mut program = Program::new(&source);
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
        for (index, &literal) in literals.iter().enumerate() {
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

#[test]
fn scalar_character_builders_preserve_retained_prefixes_and_async_values() {
    let source = r#"
export async function run(n: number): Promise<number> {
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
export function build(n: number): number {
  let output = '';
  for (let i = 0; i < n; i++) {
    const c = 'é😀'.charAt(i % 2);
    if (c !== '') output += c;
  }
  return output.length;
}
"#;
    let mut program = Program::checked(source, "scalar-characters.ts");
    program.stress_collection(true);
    for n in [0.0, 1.0, 17.0, 80.0] {
        assert_eq!(
            program.throwing("run", n),
            (n * (n - 1.0) / 2.0 * 1000.0 + n, 0)
        );
    }
    program.stress_collection(false);
    let before = program.counter(Counter::Allocations);
    assert_eq!(program.number("build", 20_000.0), 20_000.0);
    let after = program.counter(Counter::Allocations);
    assert!(after - before < 30);
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
