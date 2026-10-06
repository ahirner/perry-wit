//! Pure typed programs shared by contracts, source emission, and concrete replay.
use std::fmt::{self, Display};

use serde::{Deserialize, Serialize};

pub const INPUTS: [f64; 24] = [
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
pub const FIXTURE: &str = "é\0abc";
pub const WIT: &str = "package test:generated; world generated {
    record observation { value: f64, trace: list<f64> }
    export run: func(x: f64) -> observation;
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
    Timer,
    StoredTimer,
    FileRead,
    FileRoundTrip,
    FileMetadata,
    FileBytes,
    FileByteRoundTrip,
    FileRejectedRead,
}

impl Form {
    pub const ALL: [Self; 4] = [Self::Direct, Self::Local, Self::Branch, Self::Loop];

    pub fn is_async(self) -> bool {
        matches!(
            self,
            Self::Timer
                | Self::StoredTimer
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
    pub fn source(&self) -> String {
        let expression = &self.expression;
        let fixture = serde_json::to_string(FIXTURE).unwrap();
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
        };
        let import = match self.form {
            Form::Timer | Form::StoredTimer => "import {setTimeout} from 'node:timers/promises';\n",
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
            "{import}function mark(trace:number[], value:number):number {{ trace.push(value); return value; }}\nfunction identity(value:number):number {{ return value; }}\nexport {asynchronous}function run(x:number):{result} {{\n  const trace:number[] = [];\n  {body}\n  return {{value:value, trace:trace}};\n}}\n"
        )
    }
}

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
            Self::Literal(value) => f.write_str(&serde_json::to_string(value).unwrap()),
            Self::Concat(a, b) => write!(f, "({a} + {b})"),
            Self::Choose(c, a, b) => write!(f, "({c} ? {a} : {b})"),
            Self::Lower(value) => write!(f, "({value}.toLowerCase())"),
            Self::Upper(value) => write!(f, "({value}.toUpperCase())"),
            Self::CharAt(value, index) => write!(f, "({value}.charAt({index}))"),
            Self::Slice(value, a, b) => write!(f, "({value}.slice({a}, {b}))"),
        }
    }
}
