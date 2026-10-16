//! Pure typed programs shared by contracts, source emission, and concrete replay.
use std::fmt::{self, Display};

use serde::{Deserialize, Serialize};

pub(crate) const INPUTS: [f64; 24] = [
    -0.0,
    0.0,
    -1.0,
    1.0,
    0.5,
    2147483648.0,
    4294967295.0,
    1e-300,
    1e300,
    f64::NAN,
    f64::INFINITY,
    f64::NEG_INFINITY,
    -0.5,
    -0.5000000000000001,
    f64::from_bits(0.5_f64.to_bits() - 1),
    f64::MIN_POSITIVE,
    f64::from_bits(1),
    f64::MAX,
    -f64::MAX,
    255.9,
    -257.9,
    8640000000000000.0,
    8640000000000001.0,
    -8640000000000001.0,
];
pub(crate) const FIXTURE: &str = "é\0abc";
/// Value of `PERRY_CONFORMANCE` in command cases; non-ASCII so both runtimes must decode it.
pub(crate) const ENV_FIXTURE: &str = "grüezi";
pub(crate) const WIT: &str = "package test:generated; world generated {
    record observation { value: f64, trace: list<f64> }
    export run: func(x: f64) -> result<observation>;
}";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Number {
    Integer(i64),
    NegativeZero,
    Input,
    Binary(Binary, Box<Self>, Box<Self>),
    Choose(Box<Condition>, Box<Self>, Box<Self>),
    Mark(Box<Self>),
    Array(Box<Self>, Box<Self>, Box<Self>, u8),
    Field(Box<Self>, Box<Self>),
    Length(Box<Text>),
    Call(Box<Self>),
    Math(Math, Box<Self>),
    IndexOf(Box<Text>, Box<Text>, Box<Self>),
    DateTime(Box<Self>),
    ByteGet(Box<Self>, Box<Self>, Box<Self>, u8),
    EncodedLength(Box<Text>, Box<Self>, Box<Self>),
    JsonLength(Box<Text>),
    JsonStoredLength(Box<Text>),
    TypedJson(Box<Self>),
    ByteAlias(Box<Self>, Box<Self>),
    ClosedLoop(Box<Self>, u8),
    Handled(Box<Self>),
    ParsedAggregate(Box<Self>),
    PausedIterator(Box<Self>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Binary {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Math {
    Floor,
    Ceil,
    Trunc,
    Abs,
    Round,
}

impl Display for Math {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Floor => "floor",
            Self::Ceil => "ceil",
            Self::Trunc => "trunc",
            Self::Abs => "abs",
            Self::Round => "round",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Condition {
    Literal(bool),
    Less(Box<Number>, Box<Number>),
    Equal(Box<Number>, Box<Number>),
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
    Not(Box<Self>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Text {
    Literal(String),
    Concat(Box<Self>, Box<Self>),
    Choose(Box<Condition>, Box<Self>, Box<Self>),
    Lower(Box<Self>),
    Upper(Box<Self>),
    CharAt(Box<Self>, Box<Number>),
    Slice(Box<Self>, i8, i8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Form {
    Direct,
    Local,
    Branch,
    Loop,
    InertException,
    Timer,
    StoredTimer,
    FirstCompletion,
    LastCompletion,
    FileRead,
    FileRoundTrip,
    FileMetadata,
    FileBytes,
    FileByteRoundTrip,
    FileRejectedRead,
}

impl Form {
    /// The metamorphic forms of a direct program; [Form::Direct] comes first.
    pub(crate) const ALL: [Self; 7] = [
        Self::Direct,
        Self::Local,
        Self::Branch,
        Self::Loop,
        Self::InertException,
        Self::FirstCompletion,
        Self::LastCompletion,
    ];

    pub(crate) fn is_async(self) -> bool {
        matches!(
            self,
            Self::Timer
                | Self::StoredTimer
                | Self::FirstCompletion
                | Self::LastCompletion
                | Self::FileRead
                | Self::FileRoundTrip
                | Self::FileMetadata
                | Self::FileBytes
                | Self::FileByteRoundTrip
                | Self::FileRejectedRead
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Program {
    pub expression: Number,
    pub form: Form,
}

impl Program {
    pub(crate) fn source(&self) -> String {
        let expression = &self.expression;
        let fixture = literal(FIXTURE);
        let body = match self.form {
            Form::FileByteRoundTrip => format!(
                "const bytes = new Uint8Array([19,0,255,128,42,20]); const view = bytes.subarray(1,5); const pending = writeFile('view.bin',view); const value = {expression}; await pending; view[0] = 99; const restored = await readFile('view.bin'); if(restored.byteLength !== 4 || restored[0] !== 0 || restored[1] !== 255 || restored[2] !== 128 || restored[3] !== 42 || bytes[1] !== 99) {{throw 95;}} await writeFile('view.bin',view.subarray(2,2)); const empty = await readFile('view.bin'); if(empty.byteLength !== 0) {{throw 96;}}"
            ),
            Form::FileRejectedRead => format!(
                "const pending = readFile('missing.txt','utf8'); let failures = 0; try {{await pending;}} catch {{failures++;}} const value = {expression}; try {{await pending;}} catch {{failures++;}} if(failures !== 2) {{throw 97;}} const restored = await readFile('fixture.txt','utf8'); if(restored !== {fixture}) {{throw 98;}}"
            ),
            Form::FileMetadata => format!(
                "const pending = stat('fixture.txt'); const value = {expression}; const info = await pending; const names = await readdir('.'); if(!info.isFile() || info.isDirectory() || info.size !== 6 || names.length !== 1 || names[0] !== 'fixture.txt') {{throw 93;}}"
            ),
            Form::FileBytes => format!(
                "const pending = readFile('fixture.txt'); const value = {expression}; const bytes = await pending; if(bytes.byteLength !== 6 || new TextDecoder().decode(bytes) !== {fixture}) {{throw 94;}}"
            ),
            Form::FileRead => format!(
                "const pending = readFile('fixture.txt','utf8'); const value = {expression}; const text = await pending; if(text !== {fixture}) {{throw 91;}}"
            ),
            Form::FileRoundTrip => format!(
                "const value = {expression}; const text = JSON.stringify({{text:{fixture},value:value}}); if(text === undefined) {{throw 90;}} await writeFile('roundtrip.json',text); const restored = await readFile('roundtrip.json','utf8'); if(restored !== text) {{throw 92;}}"
            ),
            Form::FirstCompletion | Form::LastCompletion => {
                let (first_delay, second_delay) = if self.form == Form::FirstCompletion {
                    (1, 2)
                } else {
                    (2, 1)
                };
                format!(
                    "const input = {expression}; const first = setTimeout({first_delay}, input); const second = setTimeout({second_delay}, input); const all = Promise.all([first, second, first]); const values = await all; const value = values[0] as number; await first; await second;"
                )
            }
            Form::Timer => format!("const value = await setTimeout(0, {expression});"),
            Form::StoredTimer => format!(
                "const pending = setTimeout(0, {expression}); const value = await pending; await pending;"
            ),
            Form::Direct => format!("const value = {expression};"),
            Form::Local => format!("const fresh = {expression}; const value = fresh;"),
            Form::Branch => format!(
                "let value = 0; if (x === x) {{ value = {expression}; }} else {{ value = {expression}; }}"
            ),
            Form::Loop => {
                format!("let value = 0; for (let i = 0; i < 1; i++) {{ value = {expression}; }}")
            }
            Form::InertException => {
                format!("let value = 0; try {{ value = {expression}; }} finally {{ }}")
            }
        };
        let import = match self.form {
            Form::Timer | Form::StoredTimer | Form::FirstCompletion | Form::LastCompletion => {
                "import {setTimeout} from 'node:timers/promises';\n"
            }
            Form::FileRead
            | Form::FileRoundTrip
            | Form::FileMetadata
            | Form::FileBytes
            | Form::FileByteRoundTrip
            | Form::FileRejectedRead => {
                "import {readFile,writeFile,stat,readdir} from 'node:fs/promises';\n"
            }
            _ => "",
        };
        let asynchronous = if self.form.is_async() { "async " } else { "" };
        let result = if self.form.is_async() {
            "Promise<{value:number,trace:number[]}>"
        } else {
            "{value:number,trace:number[]}"
        };
        format!(
            "{import}{TYPED_HELPERS}\nfunction mark(trace:number[], value:number):number {{ trace.push(value); return value; }}\nfunction identity(value:number):number {{ return value; }}\nexport {asynchronous}function run(x:number):{result} {{\n  const trace:number[] = [];\n  {body}\n  return {{value:value, trace:trace}};\n}}\n"
        )
    }
}

const TYPED_HELPERS: &str = r#"
const sharedJsonCell: { value: number } = { value: 0 };
function clobberSharedJsonCell(): void {
    sharedJsonCell.value = -999;
}
function typedJson(value: number): number {
    sharedJsonCell.value = value;
    const snapshot = JSON.stringify(sharedJsonCell);
    clobberSharedJsonCell();
    const preserved: { value: number | null } = JSON.parse(snapshot);
    const parsed: { value: number | null; rows: number[] } = JSON.parse(JSON.stringify({ value: preserved.value, rows: [1, 2, 3] }));
    parsed.rows[0] = 4;
    return parsed.value === null ? 0 : parsed.value;
}
function byteAlias(first: number, second: number): number {
    const bytes = new Uint8Array([first, second]);
    const alias = bytes.subarray(1);
    alias[0] = first;
    return bytes[1] as number;
}
function closedLoop(value: number, count: number): number {
    const state = { value: value, step: 0 };
    for (let i = 0; i < count; i++) {
        if (i % 2 === 0) state.value += i;
        else state.value -= i;
        state.step++;
    }
    return state.value;
}
function handled(value: number): number {
    try { if (value < 0) throw value; return value; }
    catch (error) { return error as number; }
}
type RecNode = { a: number; b: number; next?: RecNode };
function parsedAggregate(value: number): number {
    const p = parseInt("42", 4294967306);
    const n1: RecNode = { a: p, b: 1.5, next: { a: p + 1, b: 2.5 } };
    const n2: RecNode = { a: 1.5, b: p, next: { a: 2.5, b: p + 1 } };
    const list: number[] = [n1.a, n2.b, p % 5];
    return (list[0] as number) + (list[1] as number) + (list[2] as number) === 86 ? value : -1;
}
function pausedIterator(value: number): number {
    const s = new Set<number>();
    s.add(1);
    s.add(2);
    const it = s.values();
    let acc = 0;
    for (const v of it) { acc += v; break; }
    s.clear();
    s.add(10);
    for (const v of it) { acc += v; }
    const m = new Map<number, number>();
    for (let i = 0; i < 16; i++) m.set(i, i + 1);
    const mit = m.values();
    for (const v of mit) { acc += v; break; }
    for (let i = 0; i < 14; i++) m.delete(i);
    for (const v of mit) { acc += v; }
    return acc === 43 ? value : -1;
}
"#;

impl Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(n) => write!(f, "({n})"),
            Self::NegativeZero => f.write_str("(-0)"),
            Self::Input => f.write_str("x"),
            Self::Binary(op, a, b) => write!(f, "({a} {op} {b})"),
            Self::Choose(c, a, b) => write!(f, "({c} ? {a} : {b})"),
            Self::Mark(value) => write!(f, "mark(trace, {value})"),
            Self::Array(a, b, c, index) => write!(f, "([{a}, {b}, {c}][{index}])"),
            Self::Field(a, b) => write!(f, "({{first:{a}, second:{b}}}.first)"),
            Self::Length(value) => write!(f, "({value}.length)"),
            Self::Call(value) => write!(f, "identity({value})"),
            Self::TypedJson(value) => write!(f, "typedJson({value})"),
            Self::ByteAlias(a, b) => write!(f, "byteAlias({a}, {b})"),
            Self::ClosedLoop(value, count) => write!(f, "closedLoop({value}, {count})"),
            Self::Handled(value) => write!(f, "handled({value})"),
            Self::ParsedAggregate(value) => write!(f, "parsedAggregate({value})"),
            Self::PausedIterator(value) => write!(f, "pausedIterator({value})"),
            Self::Math(op, value) => write!(f, "Math.{op}({value})"),
            Self::DateTime(value) => write!(f, "new Date({value}).getTime()"),
            Self::ByteGet(a, b, c, index) => {
                write!(f, "(new Uint8Array([{a}, {b}, {c}])[{index}])")
            }
            Self::EncodedLength(value, start, end) => write!(
                f,
                "(new TextEncoder().encode({value}).subarray({start}, {end}).byteLength)"
            ),
            Self::JsonStoredLength(value) => write!(
                f,
                "identity(JSON.parse([JSON.stringify({{text:{value}}})][0]).text.length)"
            ),
            Self::JsonLength(value) => write!(
                f,
                "identity(JSON.parse(JSON.stringify({{text:{value}}})).text.length)"
            ),
            Self::IndexOf(text, needle, start) => write!(f, "({text}.indexOf({needle}, {start}))"),
        }
    }
}

impl Display for Binary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Rem => "%",
        })
    }
}

impl Display for Condition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(value) => write!(f, "{value}"),
            Self::Less(a, b) => write!(f, "({a} < {b})"),
            Self::Equal(a, b) => write!(f, "(({a} as number) === ({b} as number))"),
            Self::And(a, b) => write!(f, "({a} && {b})"),
            Self::Or(a, b) => write!(f, "({a} || {b})"),
            Self::Not(value) => write!(f, "(!{value})"),
        }
    }
}

impl Display for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(value) => f.write_str(&literal(value)),
            Self::Concat(a, b) => write!(f, "({a} + {b})"),
            Self::Choose(c, a, b) => write!(f, "({c} ? {a} : {b})"),
            Self::Lower(value) => write!(f, "({value}.toLowerCase())"),
            Self::Upper(value) => write!(f, "({value}.toUpperCase())"),
            Self::CharAt(value, index) => write!(f, "({value}.charAt({index}))"),
            Self::Slice(value, a, b) => write!(f, "({value}.slice({a}, {b}))"),
        }
    }
}

/// The JSON text of a string or byte list, which is also its TypeScript literal.
pub(crate) fn literal(value: &(impl Serialize + ?Sized)) -> String {
    serde_json::to_string(value).expect("strings and byte lists always serialize to JSON")
}
