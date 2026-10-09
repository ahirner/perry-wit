//! Dense typed tuples used by declarations and enumeration entry pairs.
use crate::{
    attrs::{ErrorKind, Tier},
    exceptions::MayThrowInterface,
    types::TupleType,
};
use pliron::{
    builtin::op_interfaces::{NOpdsInterface, NResultsInterface, OneResultInterface},
    common_traits::Verify,
    context::Context,
    derive::{op_interface_impl, pliron_attr, pliron_op},
    op::Op,
    operation::Operation,
    opts::dce::SideEffects,
    result::Result,
    r#type::{TypeHandle, Typed},
    value::Value,
    verify_err,
};

#[derive(Debug, thiserror::Error)]
#[error("tuple operation has incorrect elements, index or value type")]
struct SignatureError;

pub fn element(ctx: &Context, ty: TypeHandle, index: usize) -> Option<TypeHandle> {
    ty.deref(ctx)
        .downcast_ref::<TupleType>()?
        .elements()
        .get(index)
        .copied()
}

#[pliron_attr(name = "perry.tuple_index", format, verifier = "succ")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Index(pub usize);

#[pliron_op(name = "perry.tuple_new", format = "attr($perry_tuple_tier, $Tier) ` (` operands(CharSpace(`,`)) `) : ` type($0)", interfaces = [OneResultInterface], attributes = (perry_tuple_tier: Tier))]
pub struct NewOp;
impl NewOp {
    pub fn new(ctx: &mut Context, values: Vec<Value>) -> Self {
        let ty = TupleType::get(
            ctx,
            values.iter().map(|value| value.get_type(ctx)).collect(),
        );
        let op = Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty.into()],
                values,
                vec![],
                0,
            ),
        };
        op.set_attr_perry_tuple_tier(ctx, Tier::Unassigned);
        op
    }
}
impl Verify for NewOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation().deref(ctx);
        let ty = op.get_result(0).get_type(ctx).deref(ctx);
        if !ty.downcast_ref::<TupleType>().is_some_and(|ty| {
            op.operands()
                .map(|value| value.get_type(ctx))
                .eq(ty.elements().iter().copied())
        }) {
            verify_err!(self.loc(ctx), SignatureError)?;
        }
        Ok(())
    }
}
#[op_interface_impl]
impl SideEffects for NewOp {
    fn has_side_effects(&self, _: &Context) -> bool {
        false
    }
}

#[pliron_op(name = "perry.tuple_get", format = "$0 `[` attr($perry_tuple_get_index, $Index) `] : ` type($0)", interfaces = [OneResultInterface, NOpdsInterface<1>], attributes = (perry_tuple_get_index: Index))]
pub struct GetOp;
impl GetOp {
    pub fn new(ctx: &mut Context, tuple: Value, index: usize) -> Result<Self> {
        let Some(ty) = element(ctx, tuple.get_type(ctx), index) else {
            return verify_err!(pliron::location::Location::Unknown, SignatureError);
        };
        let op = Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![ty],
                vec![tuple],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_tuple_get_index(ctx, Index(index));
        Ok(op)
    }
    pub fn index(&self, ctx: &Context) -> usize {
        self.get_attr_perry_tuple_get_index(ctx)
            .expect("tuple index")
            .0
    }
}
impl Verify for GetOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation().deref(ctx);
        if element(ctx, op.get_operand(0).get_type(ctx), self.index(ctx))
            != Some(op.get_result(0).get_type(ctx))
        {
            verify_err!(self.loc(ctx), SignatureError)?;
        }
        Ok(())
    }
}
#[op_interface_impl]
impl MayThrowInterface for GetOp {
    fn exception(&self, _: &Context) -> ErrorKind {
        ErrorKind::TypeError
    }
}

#[pliron_op(name = "perry.tuple_set", format = "$0 `[` attr($perry_tuple_set_index, $Index) `] = ` $1", interfaces = [NResultsInterface<0>, NOpdsInterface<2>], attributes = (perry_tuple_set_index: Index))]
pub struct SetOp;
impl SetOp {
    pub fn new(ctx: &mut Context, tuple: Value, index: usize, value: Value) -> Self {
        let op = Self {
            op: Operation::new(
                ctx,
                Self::get_concrete_op_info(),
                vec![],
                vec![tuple, value],
                vec![],
                0,
            ),
        };
        op.set_attr_perry_tuple_set_index(ctx, Index(index));
        op
    }
    pub fn index(&self, ctx: &Context) -> usize {
        self.get_attr_perry_tuple_set_index(ctx)
            .expect("tuple index")
            .0
    }
}
impl Verify for SetOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        let op = self.get_operation().deref(ctx);
        if element(ctx, op.get_operand(0).get_type(ctx), self.index(ctx))
            != Some(op.get_operand(1).get_type(ctx))
        {
            verify_err!(self.loc(ctx), SignatureError)?;
        }
        Ok(())
    }
}
#[op_interface_impl]
impl MayThrowInterface for SetOp {
    fn exception(&self, _: &Context) -> ErrorKind {
        ErrorKind::TypeError
    }
}
