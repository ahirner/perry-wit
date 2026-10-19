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
    attribute(_pattern, get_attr_perry_regex_pattern, set_attr_perry_regex_pattern) =
        StringAttr::new(pattern.to_owned()) => InvalidConstant;
    attribute(_flags, get_attr_perry_regex_flags, set_attr_perry_regex_flags) =
        StringAttr::new(flags.to_owned()) => InvalidConstant;
    result(ctx): Ty = Ty::Regex;
    error = InvalidConstant;
}

impl RegexConstOp {
    pub fn pattern(&self, ctx: &Context) -> String {
        self.get_attr_perry_regex_pattern(ctx)
            .expect("pattern")
            .as_str()
            .to_owned()
    }
    pub fn flags(&self, ctx: &Context) -> String {
        self.get_attr_perry_regex_flags(ctx)
            .expect("flags")
            .as_str()
            .to_owned()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("literal regex requires pattern, flags and a regex result")]
struct InvalidConstant;
