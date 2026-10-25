use super::*;

/// Extents belong to a particular view. Every path back to its construction must retain
/// the owner; a subview additionally requires its source extent at construction time.
pub(super) fn extent(
    ctx: &Context,
    numeric: &crate::facts::ProgramFacts,
    view: Value,
    consumer: Ptr<Operation>,
    depth: u8,
) -> Option<(i128, i128)> {
    let remaining = depth.checked_sub(1)?;
    let origin = view.defining_op()?;
    if !crate::facts::paths::preserves(ctx, origin, consumer, (), |op, ()| {
        (!may_detach(ctx, op)).then_some(())
    }) {
        return None;
    }
    let operand = |i| origin.deref(ctx).get_operand(i);
    match intrinsic(ctx, origin)? {
        // A view exists only after its constructor accepted the requested length.
        Intrinsic::BytesNew => numeric.integral_interval(ctx, operand(0)),
        Intrinsic::BytesCopy => extent(ctx, numeric, operand(0), origin, remaining),
        Intrinsic::BytesSubarray => {
            let (lower, upper) = extent(ctx, numeric, operand(0), origin, remaining)?;
            if lower != upper {
                return None;
            }
            let relative = |value| {
                let (a, b) = numeric.integral_interval(ctx, value)?;
                if a < 0 && b >= 0 {
                    return None;
                }
                let clamp = |index: i128| {
                    if index < 0 {
                        (lower + index).max(0)
                    } else {
                        index.min(lower)
                    }
                };
                Some((clamp(a), clamp(b)))
            };
            let (start_min, start_max) = relative(operand(1))?;
            let (end_min, end_max) = relative(operand(2))?;
            Some(((end_min - start_max).max(0), (end_max - start_min).max(0)))
        }
        _ => None,
    }
}
