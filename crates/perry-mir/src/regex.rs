//! Immutable literal regular expressions, compiled before guest execution.
mod syntax;

use crate::operation_schema::op_schema;
use crate::signatures::Ty;
use pliron::{
    builtin::attributes::StringAttr, common_traits::Verify, context::Context, op::Op,
    operation::Operation, result::Result,
};

use perry_wit_diagnostics::{Diagnostic, codes, diag};
use pliron::uniqued_any::Uniqued;
use regex_syntax::hir::Hir;
use std::{
    hash::{Hash, Hasher},
    sync::OnceLock,
};

pub struct RegexLiteral {
    spelling: (String, String),
    parsed: OnceLock<perry_wit_diagnostics::Result<ParsedRegex>>,
}

impl PartialEq for RegexLiteral {
    fn eq(&self, other: &Self) -> bool {
        self.spelling == other.spelling
    }
}
impl Eq for RegexLiteral {}
impl Hash for RegexLiteral {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.spelling.hash(state);
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
                regex.literal(ctx).get(ctx).shape().ok()
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
    pub fn intern(ctx: &Context, pattern: String, flags: String) -> Uniqued<Self> {
        Uniqued::new(
            ctx,
            Self {
                spelling: (pattern, flags),
                parsed: OnceLock::new(),
            },
        )
    }

    pub fn global(&self) -> bool {
        self.spelling.1.contains('g')
    }

    pub fn parsed(&self) -> perry_wit_diagnostics::Result<&ParsedRegex> {
        self.parsed
            .get_or_init(|| syntax::parse(&self.spelling.0, &self.spelling.1))
            .as_ref()
            .map_err(Clone::clone)
    }

    pub fn shape(&self) -> perry_wit_diagnostics::Result<RegexShape> {
        const { assert!(perry_abi::layout::regex::MAX_CAPTURES <= u8::MAX as usize) };
        let captures = &self.parsed()?.captures;
        Ok(RegexShape {
            global: Some(self.global()),
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

impl RegexConstOp {
    pub fn literal(&self, ctx: &Context) -> Uniqued<RegexLiteral> {
        RegexLiteral::intern(ctx, self.pattern(ctx), self.flags(ctx))
    }
    pub fn new(ctx: &mut Context, pattern: &str, flags: &str) -> Self {
        let literal = RegexLiteral::intern(ctx, pattern.to_owned(), flags.to_owned());
        Self::from_literal(ctx, literal)
    }
}

op_schema! {
    pub RegexConstOp() -> infallible;
    name = "perry.regex_const";
    format = "attr($perry_regex_pattern, $StringAttr) `, ` attr($perry_regex_flags, $StringAttr) ` : ` type($0)";
    attributes = (perry_regex_pattern: StringAttr, perry_regex_flags: StringAttr);
    constructors { pub from_literal(literal: Uniqued<RegexLiteral>) => infer(); }
    attribute(pattern: String, get_attr_perry_regex_pattern, set_attr_perry_regex_pattern) =
        StringAttr::new(literal.get(ctx).spelling.0.clone()) => InvalidConstant;
    attribute(flags: String, get_attr_perry_regex_flags, set_attr_perry_regex_flags) =
        StringAttr::new(literal.get(ctx).spelling.1.clone()) => InvalidConstant;
    result(ctx): Ty = Ty::Regex;
    error = InvalidConstant;
}

#[derive(Debug, thiserror::Error)]
#[error("literal regex requires pattern, flags and a regex result")]
struct InvalidConstant;
