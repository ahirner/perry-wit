//! Immutable literal regular expressions, compiled before guest execution.
mod syntax;

use crate::operation_schema::op_schema;
use crate::signatures::Ty;
use pliron::{
    builtin::attributes::StringAttr,
    common_traits::Verify,
    context::Context,
    derive::{format, pliron_attr},
    op::Op,
    operation::Operation,
    parsable::{Parsable, ParseResult, StateStream},
    printable::{Printable, State},
    result::Result,
};

use perry_wit_diagnostics::{Diagnostic, codes, diag};
use pliron::uniqued_any::Uniqued;
use regex_syntax::hir::Hir;
use std::{
    hash::{Hash, Hasher},
    sync::OnceLock,
};

#[format("$0 `, ` $1")]
#[derive(PartialEq, Eq, Hash)]
struct Spelling(StringAttr, StringAttr);

struct Literal {
    spelling: Spelling,
    parsed: OnceLock<perry_wit_diagnostics::Result<ParsedRegex>>,
}

impl PartialEq for Literal {
    fn eq(&self, other: &Self) -> bool {
        self.spelling == other.spelling
    }
}
impl Eq for Literal {}
impl Hash for Literal {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.spelling.hash(state);
    }
}

#[pliron_attr(name = "perry.regex_literal", verifier = "succ")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RegexLiteral(Uniqued<Literal>);

impl Printable for RegexLiteral {
    fn fmt(
        &self,
        ctx: &Context,
        state: &State,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        self.0.get(ctx).spelling.fmt(ctx, state, f)
    }
}

impl Parsable for RegexLiteral {
    type Arg = ();
    type Parsed = Self;

    fn parse<'a>(stream: &mut StateStream<'a>, (): ()) -> ParseResult<'a, Self> {
        Spelling::parse(stream, ())
            .map(|(spelling, rest)| (Self::from_spelling(stream.state.ctx, spelling), rest))
    }
}

pub struct ParsedRegex {
    pub hir: Hir,
    pub captures: Captures,
}

#[derive(Default)]
pub struct Captures {
    pub count: u32,
    pub names: Vec<(usize, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct RegexShape {
    pub global: Option<bool>,
    pub captures: Option<(u8, bool)>,
}

impl RegexShape {
    pub const UNKNOWN_FLAGS: &'static str = "regex flags must be statically known; use a regex literal or a local assigned only literals with the same g flag";

    pub fn of(ctx: &Context, value: pliron::value::Value) -> Self {
        crate::facts::guards::join_origins(
            ctx,
            value,
            |op| {
                let regex = Operation::get_op::<RegexConstOp>(op, ctx)?;
                regex.literal(ctx).shape(ctx).ok()
            },
            |a, b| Some(a.join(b)),
        )
        .unwrap_or_default()
    }

    pub fn split(self) -> crate::Intrinsic {
        match self.captures {
            Some((0, _)) => crate::Intrinsic::RegexSplitStrings,
            _ => crate::Intrinsic::RegexSplit,
        }
    }

    pub fn join(self, other: Self) -> Self {
        Self {
            global: self.global.filter(|g| Some(*g) == other.global),
            captures: self.captures.filter(|c| Some(*c) == other.captures),
        }
    }
}

impl RegexLiteral {
    pub fn intern(ctx: &Context, pattern: String, flags: String) -> Self {
        Self::from_spelling(ctx, Spelling(pattern.into(), flags.into()))
    }

    fn from_spelling(ctx: &Context, spelling: Spelling) -> Self {
        Self(Uniqued::new(
            ctx,
            Literal {
                spelling,
                parsed: OnceLock::new(),
            },
        ))
    }

    pub fn global(self, ctx: &Context) -> bool {
        self.0.get(ctx).spelling.1.as_str().contains('g')
    }

    pub fn parsed(self, ctx: &Context) -> perry_wit_diagnostics::Result<&ParsedRegex> {
        let literal = self.0.get(ctx);
        literal
            .parsed
            .get_or_init(|| syntax::parse(literal.spelling.0.as_str(), literal.spelling.1.as_str()))
            .as_ref()
            .map_err(Clone::clone)
    }

    pub fn shape(self, ctx: &Context) -> perry_wit_diagnostics::Result<RegexShape> {
        const { assert!(perry_abi::layout::regex::MAX_CAPTURES <= u8::MAX as usize) };
        let captures = &self.parsed(ctx)?.captures;
        Ok(RegexShape {
            global: Some(self.global(ctx)),
            captures: Some((captures.count as u8, !captures.names.is_empty())),
        })
    }

    pub fn build_error(err: &(dyn std::error::Error + 'static)) -> Diagnostic {
        diag!(
            codes::UNSUPPORTED_REGEX,
            "Unsupported or oversized literal regex"
        )
        .with_source_notes(err)
    }
}

op_schema! {
    pub RegexConstOp() -> infallible;
    name = "perry.regex_const";
    format = "attr($perry_regex_literal, $RegexLiteral) ` : ` type($0)";
    attributes = (perry_regex_literal: RegexLiteral);
    constructors { pub from_literal(literal: RegexLiteral) => infer(); }
    attribute(literal: RegexLiteral, get_attr_perry_regex_literal, set_attr_perry_regex_literal) =
        literal => InvalidConstant;
    result(ctx): Ty = Ty::Regex;
    error = InvalidConstant;
}

#[derive(Debug, thiserror::Error)]
#[error("literal regex requires pattern, flags and a regex result")]
struct InvalidConstant;
