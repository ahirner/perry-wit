//! Alias-preserving widening of closed aggregates with identical storage layouts.
use crate::operation_schema::op_schema;
use crate::signatures::{Accepts, OutputContract};
use crate::types::{ArrayType, ByteInputType, BytesType, OptionalType, RecordType, VariantType};
use pliron::{
    common_traits::Verify,
    context::Context,
    op::Op,
    operation::Operation,
    result::Result,
    r#type::{TypeHandle, Typed},
    value::Value,
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

op_schema! {
    pub AggregateViewOp(value) -> infallible;
    name = "perry.aggregate_view";
    format = "$0 ` : ` type($0)";
    constructors { pub new(value: Value, target: TypeHandle) => explicit(target); }
    result(ctx, value: Value): impl OutputContract = Accepts(move |ctx: &Context, target| {
        can_widen(ctx, value.get_type(ctx), target)
    });
    error = IncompatibleLayout;
}

#[derive(Debug, thiserror::Error)]
#[error("aggregate widening requires compatible layouts")]
struct IncompatibleLayout;
