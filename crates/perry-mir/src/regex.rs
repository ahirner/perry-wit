//! Immutable literal regular expressions, compiled before guest execution.
use crate::operation_schema::op_schema;
use crate::signatures::Ty;
use pliron::{
    builtin::attributes::StringAttr, common_traits::Verify, context::Context, op::Op,
    operation::Operation, result::Result,
};

op_schema! {
    pub RegexConstOp() -> infallible;
    name = "perry.regex_const";
    format = "attr($perry_regex_pattern, $StringAttr) `, ` attr($perry_regex_flags, $StringAttr) ` : ` type($0)";
    attributes = (perry_regex_pattern: StringAttr, perry_regex_flags: StringAttr);
    constructors { pub new(pattern: &str, flags: &str) => infer(); }
    attribute(pattern: String, get_attr_perry_regex_pattern, set_attr_perry_regex_pattern) =
        StringAttr::new(pattern.to_owned()) => InvalidConstant;
    attribute(flags: String, get_attr_perry_regex_flags, set_attr_perry_regex_flags) =
        StringAttr::new(flags.to_owned()) => InvalidConstant;
    result(ctx): Ty = Ty::Regex;
    error = InvalidConstant;
}

impl RegexConstOp {
    /// The callback arity of a literal: explicit captures and an optional groups argument.
    /// Full syntax validation remains with the regex compiler.
    pub fn captures(&self, ctx: &Context) -> Option<(usize, bool)> {
        let pattern = self.get_attr_perry_regex_pattern(ctx)?;
        let mut chars = pattern.as_str().chars().peekable();
        let mut in_class = false;
        let mut count = 0;
        let mut named = false;
        while let Some(ch) = chars.next() {
            match ch {
                '\\' => {
                    chars.next();
                }
                '[' => in_class = true,
                ']' => in_class = false,
                '(' if !in_class => {
                    if chars.peek() != Some(&'?') {
                        count += 1;
                    } else {
                        chars.next();
                        if chars.next() == Some('<') && !matches!(chars.peek(), Some('=' | '!')) {
                            count += 1;
                            named = true;
                        }
                    }
                }
                _ => {}
            }
        }
        Some((count, named))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("literal regex requires pattern, flags and a regex result")]
struct InvalidConstant;
