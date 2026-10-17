//! Alias-preserving widening of closed aggregates with identical storage layouts.
use crate::types::{ArrayType, ByteInputType, BytesType, OptionalType, RecordType, VariantType};
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
use std::collections::HashSet;

/// Only matching record fields, array elements, and variant cases may widen their admitted states.
/// No field is added, removed, reordered or converted to another representation.
pub fn can_widen(ctx: &Context, source: TypeHandle, target: TypeHandle) -> bool {
    fn compatible(
        ctx: &Context,
        source: TypeHandle,
        target: TypeHandle,
        seen: &mut HashSet<(TypeHandle, TypeHandle)>,
    ) -> bool {
        if source == target || !seen.insert((source, target)) {
            return true;
        }
        let (a, b) = (source.deref(ctx), target.deref(ctx));
        if a.is::<BytesType>() && b.is::<ByteInputType>() {
            return true;
        }
        if !a.is::<OptionalType>()
            && let Some(b) = b.downcast_ref::<OptionalType>()
        {
            return compatible(ctx, source, b.value(), seen);
        }
        if let (Some(a), Some(b)) = (
            a.downcast_ref::<OptionalType>(),
            b.downcast_ref::<OptionalType>(),
        ) {
            let (from, to) = (a.absence(), b.absence());
            return (!from.admits_null() || to.admits_null())
                && (!from.admits_undefined() || to.admits_undefined())
                && compatible(ctx, a.value(), b.value(), seen);
        }
        if let (Some(a), Some(b)) = (a.downcast_ref::<ArrayType>(), b.downcast_ref::<ArrayType>()) {
            return compatible(ctx, a.element(), b.element(), seen);
        }
        if let (Some(a), Some(b)) = (
            a.downcast_ref::<RecordType>(),
            b.downcast_ref::<RecordType>(),
        ) {
            if a.is_tuple() != b.is_tuple() {
                return false;
            }
            let (Ok(a), Ok(b)) = (a.fields(), b.fields()) else {
                return false;
            };
            return a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|((a, at), (b, bt))| a == b && compatible(ctx, *at, *bt, seen));
        }
        if let (Some(a), Some(b)) = (
            a.downcast_ref::<VariantType>(),
            b.downcast_ref::<VariantType>(),
        ) {
            return a.cases().iter().all(|(tag, a_payload)| {
                b.cases()
                    .iter()
                    .find(|(b_tag, _)| b_tag == tag)
                    .is_some_and(|(_, b_payload)| match (a_payload, b_payload) {
                        (None, None) => true,
                        (Some(at), Some(bt)) => compatible(ctx, *at, *bt, seen),
                        _ => false,
                    })
            });
        }
        false
    }
    let aggregate = source.deref(ctx).is::<RecordType>()
        || source.deref(ctx).is::<ArrayType>()
        || source.deref(ctx).is::<VariantType>();
    aggregate && compatible(ctx, source, target, &mut HashSet::new())
}

#[pliron_op(name = "perry.aggregate_view", format = "$0 ` : ` type($0)", interfaces = [OneOpdInterface, OneResultInterface])]
pub struct AggregateViewOp;

impl AggregateViewOp {
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

#[derive(Debug, thiserror::Error)]
#[error("aggregate widening requires compatible layouts")]
struct IncompatibleLayout;

impl Verify for AggregateViewOp {
    fn verify(&self, ctx: &Context) -> Result<()> {
        if !can_widen(
            ctx,
            self.get_operand(ctx).get_type(ctx),
            self.get_result(ctx).get_type(ctx),
        ) {
            return verify_err!(self.loc(ctx), IncompatibleLayout);
        }
        Ok(())
    }
}

#[op_interface_impl]
impl SideEffects for AggregateViewOp {
    fn has_side_effects(&self, _: &Context) -> bool {
        false
    }
}
