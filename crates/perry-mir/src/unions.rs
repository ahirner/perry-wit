//! Closed text-or-byte unions preserve native references without allocating wrapper objects.
use crate::{
    optional::optional_value,
    types::{BytesType, OptionalType, StringType, TextOrBytesType},
};
use pliron::{
    builtin::op_interfaces::{OneOpdInterface, OneResultInterface},
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_op},
    op::Op,
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::{TypeHandle, Typed},
    value::Value,
    verify_err,
};

fn alternative(ctx: &Context, ty: TypeHandle) -> bool {
    let ty = ty.deref(ctx);
    ty.is::<StringType>() || ty.is::<BytesType>()
}
#[derive(Debug, thiserror::Error)]
#[error("text/byte injection requires a string or byte view and preserves optionality")]
struct InjectError;

#[pliron_op(name="perry.text_bytes_inject", format="$0 ` : ` type($0)", interfaces=[OneOpdInterface, OneResultInterface])]
pub struct InjectOp;
impl InjectOp {
    pub fn new(ctx: &mut Context, value: Value) -> Self {
        let union = TextOrBytesType::get(ctx).into();
        let result = if optional_value(ctx, value.get_type(ctx)).is_some() {
            OptionalType::get(ctx, union).into()
        } else {
            union
        };
        Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![result],
                vec![value],
                vec![],
                0,
            ),
        }
    }
}
impl Verify for InjectOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let source = self.get_operation().deref(ctx).get_operand(0).get_type(ctx);
        let target = self.get_result(ctx).get_type(ctx);
        if !alternative(ctx, optional_value(ctx, source).unwrap_or(source))
            || !optional_value(ctx, target)
                .unwrap_or(target)
                .deref(ctx)
                .is::<TextOrBytesType>()
            || optional_value(ctx, source).is_some() != optional_value(ctx, target).is_some()
        {
            verify_err!(self.loc(ctx), InjectError)?;
        }
        Ok(())
    }
}
#[op_interface_impl]
impl SideEffects for InjectOp {
    fn has_side_effects(&self, _: &Context) -> bool {
        false
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "text/byte projection requires a union and a string or byte-view result with matching optionality"
)]
struct ProjectError;
#[pliron_op(name="perry.text_bytes_project", format="$0 ` : ` type($0)", interfaces=[OneOpdInterface, OneResultInterface])]
pub struct ProjectOp;
impl ProjectOp {
    pub fn new(ctx: &mut Context, value: Value, target: TypeHandle) -> Self {
        Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![target],
                vec![value],
                vec![],
                0,
            ),
        }
    }
}
impl Verify for ProjectOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let source = self.get_operation().deref(ctx).get_operand(0).get_type(ctx);
        let target = self.get_result(ctx).get_type(ctx);
        if !optional_value(ctx, source)
            .unwrap_or(source)
            .deref(ctx)
            .is::<TextOrBytesType>()
            || !alternative(ctx, optional_value(ctx, target).unwrap_or(target))
            || optional_value(ctx, source).is_some() != optional_value(ctx, target).is_some()
        {
            verify_err!(self.loc(ctx), ProjectError)?;
        }
        Ok(())
    }
}
#[op_interface_impl]
impl crate::exceptions::MayThrowInterface for ProjectOp {
    fn exception(&self, _: &Context) -> crate::attrs::ErrorKind {
        crate::attrs::ErrorKind::TypeError
    }
}
