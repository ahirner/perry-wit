//! Typed, terminating programs shared by generation, replay, and structural reduction.
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
}

impl Form {
    pub const ALL: [Self; 4] = [Self::Direct, Self::Local, Self::Branch, Self::Loop];

    pub fn is_async(self) -> bool {
        matches!(
            self,
            Self::Timer | Self::StoredTimer | Self::FileRead | Self::FileRoundTrip
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
            Form::FileRead | Form::FileRoundTrip => {
                "import {readFile,writeFile} from 'node:fs/promises';\n"
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

    pub fn reductions(&self) -> Vec<Self> {
        self.expression
            .reductions()
            .into_iter()
            .map(|expression| Self {
                expression,
                form: self.form,
            })
            .collect()
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

/// SplitMix64 fixes the seed mapping independently of platform or dependency versions.
pub struct Generator {
    state: u64,
    api_level: u8,
}

impl Generator {
    pub fn new(seed: u64) -> Self {
        Self::with_apis(seed, 2)
    }

    pub fn with_apis(seed: u64, api_level: u8) -> Self {
        Self {
            state: seed,
            api_level,
        }
    }

    fn pick(&mut self, count: u64) -> usize {
        self.state = self.state.wrapping_add(0x9e3779b97f4a7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        ((value ^ (value >> 31)) % count) as usize
    }

    pub fn number(&mut self, depth: u8) -> Number {
        if depth == 0 {
            match self.pick(9) {
                0..=2 => Number::Input,
                3 => Number::NegativeZero,
                index => Number::Integer([0, 1, -1, 31, 4294967295][index - 4]),
            }
        } else {
            let child = depth - 1;
            match self.pick(match self.api_level {
                0 => 10,
                1 => 12,
                _ => 16,
            }) {
                0 => self.number(0),
                1..=3 => {
                    let op = [
                        Binary::Add,
                        Binary::Sub,
                        Binary::Mul,
                        Binary::Div,
                        Binary::Rem,
                    ][self.pick(5)];
                    Number::Binary(
                        op,
                        Box::new(self.number(child)),
                        Box::new(self.number(child)),
                    )
                }
                4 => Number::Choose(
                    Box::new(self.condition(child)),
                    Box::new(self.number(child)),
                    Box::new(self.number(child)),
                ),
                5 => Number::Mark(Box::new(self.number(child))),
                6 => Number::Array(
                    Box::new(self.number(child)),
                    Box::new(self.number(child)),
                    Box::new(self.number(child)),
                    self.pick(3) as u8,
                ),
                7 => Number::Field(Box::new(self.number(child)), Box::new(self.number(child))),
                8 => Number::Length(Box::new(self.text(child))),
                9 => Number::Call(Box::new(self.number(child))),
                10 => Number::Math(
                    [Math::Floor, Math::Ceil, Math::Trunc, Math::Abs, Math::Round][self.pick(5)],
                    Box::new(self.number(child)),
                ),
                11 => Number::IndexOf(
                    Box::new(self.text(child)),
                    Box::new(self.text(child)),
                    Box::new(self.number(child)),
                ),
                12 => Number::DateTime(Box::new(self.number(child))),
                13 => Number::ByteGet(
                    Box::new(self.number(child)),
                    Box::new(self.number(child)),
                    Box::new(self.number(child)),
                    self.pick(3) as u8,
                ),
                14 => Number::EncodedLength(
                    Box::new(self.text(child)),
                    Box::new(self.number(child)),
                    Box::new(self.number(child)),
                ),
                _ => Number::JsonLength(Box::new(self.text(child))),
            }
        }
    }

    fn condition(&mut self, depth: u8) -> Condition {
        if depth == 0 {
            Condition::Literal(self.pick(2) == 0)
        } else {
            let child = depth - 1;
            match self.pick(5) {
                0 => Condition::Less(Box::new(self.number(child)), Box::new(self.number(child))),
                1 => Condition::Equal(Box::new(self.number(child)), Box::new(self.number(child))),
                2 => Condition::And(
                    Box::new(self.condition(child)),
                    Box::new(self.condition(child)),
                ),
                3 => Condition::Or(
                    Box::new(self.condition(child)),
                    Box::new(self.condition(child)),
                ),
                _ => Condition::Not(Box::new(self.condition(child))),
            }
        }
    }

    fn text(&mut self, depth: u8) -> Text {
        if depth == 0 {
            Text::Literal(["", "a", "  abc  ", "é漢", "é", "a\0b", "\n\t"][self.pick(7)].into())
        } else {
            let child = depth - 1;
            match self.pick(if self.api_level == 0 { 5 } else { 7 }) {
                0 => self.text(0),
                1 => Text::Concat(Box::new(self.text(child)), Box::new(self.text(child))),
                2 => Text::Choose(
                    Box::new(self.condition(child)),
                    Box::new(self.text(child)),
                    Box::new(self.text(child)),
                ),
                3 => Text::Lower(Box::new(self.text(child))),
                4 if self.api_level != 0 => Text::Upper(Box::new(self.text(child))),
                5 => Text::CharAt(Box::new(self.text(child)), Box::new(self.number(child))),
                _ => Text::Slice(
                    Box::new(self.text(child)),
                    self.pick(7) as i8 - 3,
                    self.pick(7) as i8 - 3,
                ),
            }
        }
    }
}

impl Number {
    pub fn reductions(&self) -> Vec<Self> {
        let mut candidates = vec![Self::Integer(0), Self::Input];
        match self {
            Self::Binary(op, a, b) => {
                candidates.extend([*a.clone(), *b.clone()]);
                candidates.extend(
                    a.reductions()
                        .into_iter()
                        .map(|v| Self::Binary(*op, Box::new(v), b.clone())),
                );
                candidates.extend(
                    b.reductions()
                        .into_iter()
                        .map(|v| Self::Binary(*op, a.clone(), Box::new(v))),
                );
            }
            Self::Choose(c, a, b) => {
                candidates.extend([*a.clone(), *b.clone()]);
                candidates.extend(
                    c.reductions()
                        .into_iter()
                        .map(|v| Self::Choose(Box::new(v), a.clone(), b.clone())),
                );
                candidates.extend(
                    a.reductions()
                        .into_iter()
                        .map(|v| Self::Choose(c.clone(), Box::new(v), b.clone())),
                );
                candidates.extend(
                    b.reductions()
                        .into_iter()
                        .map(|v| Self::Choose(c.clone(), a.clone(), Box::new(v))),
                );
            }
            Self::Mark(value) | Self::Call(value) | Self::DateTime(value) => {
                candidates.push(*value.clone());
                candidates.extend(value.reductions().into_iter().map(|v| {
                    if matches!(self, Self::Mark(_)) {
                        Self::Mark(Box::new(v))
                    } else if matches!(self, Self::DateTime(_)) {
                        Self::DateTime(Box::new(v))
                    } else {
                        Self::Call(Box::new(v))
                    }
                }));
            }
            Self::Array(a, b, c, index) | Self::ByteGet(a, b, c, index) => {
                let constructor = if matches!(self, Self::Array(..)) {
                    Self::Array
                } else {
                    Self::ByteGet
                };
                candidates.extend([*a.clone(), *b.clone(), *c.clone()]);
                candidates.extend(
                    a.reductions()
                        .into_iter()
                        .map(|v| constructor(Box::new(v), b.clone(), c.clone(), *index)),
                );
                candidates.extend(
                    b.reductions()
                        .into_iter()
                        .map(|v| constructor(a.clone(), Box::new(v), c.clone(), *index)),
                );
                candidates.extend(
                    c.reductions()
                        .into_iter()
                        .map(|v| constructor(a.clone(), b.clone(), Box::new(v), *index)),
                );
            }
            Self::Field(a, b) => {
                candidates.extend([*a.clone(), *b.clone()]);
                candidates.extend(
                    a.reductions()
                        .into_iter()
                        .map(|v| Self::Field(Box::new(v), b.clone())),
                );
                candidates.extend(
                    b.reductions()
                        .into_iter()
                        .map(|v| Self::Field(a.clone(), Box::new(v))),
                );
            }
            Self::Math(op, value) => {
                candidates.push(*value.clone());
                candidates.extend(
                    value
                        .reductions()
                        .into_iter()
                        .map(|v| Self::Math(*op, Box::new(v))),
                );
            }
            Self::IndexOf(text, needle, start) => {
                candidates.push(*start.clone());
                candidates.extend(
                    text.reductions()
                        .into_iter()
                        .map(|v| Self::IndexOf(Box::new(v), needle.clone(), start.clone())),
                );
                candidates.extend(
                    needle
                        .reductions()
                        .into_iter()
                        .map(|v| Self::IndexOf(text.clone(), Box::new(v), start.clone())),
                );
                candidates.extend(
                    start
                        .reductions()
                        .into_iter()
                        .map(|v| Self::IndexOf(text.clone(), needle.clone(), Box::new(v))),
                );
            }
            Self::EncodedLength(text, start, end) => {
                candidates.extend([*start.clone(), *end.clone()]);
                candidates.extend(
                    text.reductions()
                        .into_iter()
                        .map(|v| Self::EncodedLength(Box::new(v), start.clone(), end.clone())),
                );
                candidates.extend(
                    start
                        .reductions()
                        .into_iter()
                        .map(|v| Self::EncodedLength(text.clone(), Box::new(v), end.clone())),
                );
                candidates.extend(
                    end.reductions()
                        .into_iter()
                        .map(|v| Self::EncodedLength(text.clone(), start.clone(), Box::new(v))),
                );
            }
            Self::JsonLength(value) => candidates.extend(
                value
                    .reductions()
                    .into_iter()
                    .map(|v| Self::JsonLength(Box::new(v))),
            ),
            Self::Length(value) => candidates.extend(
                value
                    .reductions()
                    .into_iter()
                    .map(|v| Self::Length(Box::new(v))),
            ),
            _ => {}
        }
        let size = self.to_string().len();
        candidates.retain(|candidate| candidate.to_string().len() < size);
        candidates
    }
}

impl Condition {
    fn reductions(&self) -> Vec<Self> {
        let mut candidates = vec![Self::Literal(false), Self::Literal(true)];
        match self {
            Self::Less(a, b) | Self::Equal(a, b) => {
                for (a, b) in a
                    .reductions()
                    .into_iter()
                    .map(|v| (Box::new(v), b.clone()))
                    .chain(b.reductions().into_iter().map(|v| (a.clone(), Box::new(v))))
                {
                    candidates.push(if matches!(self, Self::Less(..)) {
                        Self::Less(a, b)
                    } else {
                        Self::Equal(a, b)
                    });
                }
            }
            Self::And(a, b) | Self::Or(a, b) => {
                candidates.extend([*a.clone(), *b.clone()]);
                for (a, b) in a
                    .reductions()
                    .into_iter()
                    .map(|v| (Box::new(v), b.clone()))
                    .chain(b.reductions().into_iter().map(|v| (a.clone(), Box::new(v))))
                {
                    candidates.push(if matches!(self, Self::And(..)) {
                        Self::And(a, b)
                    } else {
                        Self::Or(a, b)
                    });
                }
            }
            Self::Not(value) => {
                candidates.push(*value.clone());
                candidates.extend(
                    value
                        .reductions()
                        .into_iter()
                        .map(|v| Self::Not(Box::new(v))),
                );
            }
            Self::Literal(_) => {}
        }
        let size = self.to_string().len();
        candidates.retain(|candidate| candidate.to_string().len() < size);
        candidates
    }
}

impl Text {
    fn reductions(&self) -> Vec<Self> {
        let mut candidates = vec![Self::Literal(String::new())];
        match self {
            Self::Concat(a, b) => {
                candidates.extend([*a.clone(), *b.clone()]);
                candidates.extend(
                    a.reductions()
                        .into_iter()
                        .map(|v| Self::Concat(Box::new(v), b.clone())),
                );
                candidates.extend(
                    b.reductions()
                        .into_iter()
                        .map(|v| Self::Concat(a.clone(), Box::new(v))),
                );
            }
            Self::Choose(c, a, b) => {
                candidates.extend([*a.clone(), *b.clone()]);
                candidates.extend(
                    c.reductions()
                        .into_iter()
                        .map(|v| Self::Choose(Box::new(v), a.clone(), b.clone())),
                );
                candidates.extend(
                    a.reductions()
                        .into_iter()
                        .map(|v| Self::Choose(c.clone(), Box::new(v), b.clone())),
                );
                candidates.extend(
                    b.reductions()
                        .into_iter()
                        .map(|v| Self::Choose(c.clone(), a.clone(), Box::new(v))),
                );
            }
            Self::Lower(value) | Self::Upper(value) | Self::Slice(value, ..) => {
                candidates.push(*value.clone());
                candidates.extend(value.reductions().into_iter().map(|v| match self {
                    Self::Lower(_) => Self::Lower(Box::new(v)),
                    Self::Upper(_) => Self::Upper(Box::new(v)),
                    Self::Slice(_, a, b) => Self::Slice(Box::new(v), *a, *b),
                    _ => unreachable!(),
                }));
            }
            Self::CharAt(value, index) => {
                candidates.push(*value.clone());
                candidates.extend(
                    value
                        .reductions()
                        .into_iter()
                        .map(|v| Self::CharAt(Box::new(v), index.clone())),
                );
                candidates.extend(
                    index
                        .reductions()
                        .into_iter()
                        .map(|v| Self::CharAt(value.clone(), Box::new(v))),
                );
            }
            Self::Literal(_) => {}
        }
        let size = self.to_string().len();
        candidates.retain(|candidate| candidate.to_string().len() < size);
        candidates
    }
}
