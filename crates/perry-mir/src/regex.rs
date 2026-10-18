//! Immutable literal regular expressions, compiled before guest execution.
use crate::{NewOp, types::RegexType};
use pliron::{
    builtin::{
        attributes::StringAttr,
        op_interfaces::{NOpdsInterface, OneResultInterface},
    },
    common_traits::Verify,
    context::Context,
    derive::pliron_op,
    op::Op,
    result::Result,
    r#type::Typed,
    verify_err,
};

#[pliron_op(name = "perry.regex_const",
    format = "attr($perry_regex_pattern, $StringAttr) `, ` attr($perry_regex_flags, $StringAttr) ` : ` type($0)",
    interfaces = [NOpdsInterface<0>, OneResultInterface],
    attributes = (perry_regex_pattern: StringAttr, perry_regex_flags: StringAttr))]
pub struct RegexConstOp;

impl RegexConstOp {
    pub fn new(ctx: &mut Context, pattern: &str, flags: &str) -> Self {
        let ty = RegexType::get(ctx).into();
        let op = Self {
            op: Self::new_op(ctx, vec![ty], vec![]),
        };
        op.set_attr_perry_regex_pattern(ctx, StringAttr::new(pattern.to_owned()));
        op.set_attr_perry_regex_flags(ctx, StringAttr::new(flags.to_owned()));
        op
    }

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

impl Verify for RegexConstOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if self.get_attr_perry_regex_pattern(ctx).is_none()
            || self.get_attr_perry_regex_flags(ctx).is_none()
            || !self
                .get_result(ctx)
                .get_type(ctx)
                .deref(ctx)
                .is::<RegexType>()
        {
            verify_err!(self.loc(ctx), InvalidConstant)?;
        }
        Ok(())
    }
}

side_effects!(false; RegexConstOp);
