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
    let aggregate = source.deref(ctx).is::<RecordType>()
        || source.deref(ctx).is::<ArrayType>()
        || source.deref(ctx).is::<crate::PromiseType>()
        || source.deref(ctx).is::<VariantType>()
        || source.deref(ctx).is::<crate::ValueUnionType>();
    aggregate && Relation::Widen.accepts(ctx, source, target, &mut HashSet::new())
}
/// Whether the import ABI can consume a value through its existing MIR representation.
pub fn accepts_import_input(ctx: &Context, source: TypeHandle, target: TypeHandle) -> bool {
    Relation::Import.accepts(ctx, source, target, &mut HashSet::new())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Relation {
    Widen,
    Import,
}

impl Relation {
    fn accepts(
        self,
        ctx: &Context,
        source: TypeHandle,
        target: TypeHandle,
        seen: &mut HashSet<(TypeHandle, TypeHandle)>,
    ) -> bool {
        if source == target || !seen.insert((source, target)) {
            return true;
        }
        let (a, b) = (source.deref(ctx), target.deref(ctx));
        if self == Self::Widen && a.is::<crate::ValueUnionType>() && b.is::<crate::ValueUnionType>()
        {
            let (Ok(a), Ok(b)) = (
                crate::variant_cases(ctx, source),
                crate::variant_cases(ctx, target),
            ) else {
                return false;
            };
            return a.iter().all(|case| b.contains(case));
        }
        if b.is::<ByteInputType>() {
            return a.is::<BytesType>() || (self == Self::Import && a.is::<crate::StringType>());
        }
        if self == Self::Widen
            && !a.is::<OptionalType>()
            && let Some(b) = b.downcast_ref::<OptionalType>()
        {
            return self.accepts(ctx, source, b.value(), seen);
        }
        if let (Some(a), Some(b)) = (
            a.downcast_ref::<OptionalType>(),
            b.downcast_ref::<OptionalType>(),
        ) {
            let (from, to) = (a.absence(), b.absence());
            return (!from.admits_null() || to.admits_null())
                && (!from.admits_undefined() || to.admits_undefined())
                && self.accepts(ctx, a.value(), b.value(), seen);
        }
        if let (Some(a), Some(b)) = (a.downcast_ref::<ArrayType>(), b.downcast_ref::<ArrayType>()) {
            return self.accepts(ctx, a.element(), b.element(), seen);
        }
        if self == Self::Widen
            && let (Some(a), Some(b)) = (
                a.downcast_ref::<crate::PromiseType>(),
                b.downcast_ref::<crate::PromiseType>(),
            )
        {
            return self.accepts(ctx, a.value(), b.value(), seen);
        }
        if let (Some(a), Some(b)) = (
            a.downcast_ref::<RecordType>(),
            b.downcast_ref::<RecordType>(),
        ) {
            if self == Self::Widen && a.kind() != b.kind() {
                return false;
            }
            let (Ok(a), Ok(b)) = (a.fields(), b.fields()) else {
                return false;
            };
            return a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|((a, at), (b, bt))| a == b && self.accepts(ctx, *at, *bt, seen));
        }
        if let (Some(a), Some(b)) = (
            a.downcast_ref::<VariantType>(),
            b.downcast_ref::<VariantType>(),
        ) {
            let mut payloads = |from: Option<TypeHandle>, to: Option<TypeHandle>| match (from, to) {
                (None, None) => true,
                (Some(from), Some(to)) => self.accepts(ctx, from, to, seen),
                _ => false,
            };
            return match self {
                Self::Widen => a.cases().iter().all(|(name, from)| {
                    b.cases()
                        .iter()
                        .find(|(other, _)| name == other)
                        .is_some_and(|(_, to)| payloads(*from, *to))
                }),
                Self::Import => {
                    a.cases().len() == b.cases().len()
                        && a.cases()
                            .iter()
                            .zip(b.cases())
                            .all(|((n, from), (m, to))| n == m && payloads(*from, *to))
                }
            };
        }
        false
    }
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
