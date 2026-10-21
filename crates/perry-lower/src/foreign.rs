//! Imported async functions specialize only their canonical input views.
use super::*;
use perry_mir::{ForeignOp, Phase};

/// Whether `actual` is structurally compatible with outbound import parameter type `expected`.
pub(super) fn accepts_input(ctx: &Context, expected: TypeHandle, actual: TypeHandle) -> bool {
    fn accepts(
        ctx: &Context,
        expected: TypeHandle,
        actual: TypeHandle,
        seen: &mut HashSet<(TypeHandle, TypeHandle)>,
    ) -> bool {
        if expected == actual || !seen.insert((expected, actual)) {
            return true;
        }
        let expected = expected.deref(ctx);
        let actual = actual.deref(ctx);
        if expected.is::<perry_mir::ByteInputType>() {
            return actual.is::<BytesType>() || actual.is::<StringType>();
        }
        if let (Some(a), Some(b)) = (
            expected.downcast_ref::<perry_mir::OptionalType>(),
            actual.downcast_ref::<perry_mir::OptionalType>(),
        ) {
            return (!b.absence().admits_null() || a.absence().admits_null())
                && (!b.absence().admits_undefined() || a.absence().admits_undefined())
                && accepts(ctx, a.value(), b.value(), seen);
        }
        if let (Some(a), Some(b)) = (
            expected.downcast_ref::<ArrayType>(),
            actual.downcast_ref::<ArrayType>(),
        ) {
            return accepts(ctx, a.element(), b.element(), seen);
        }
        if let (Some(a), Some(b)) = (
            expected.downcast_ref::<RecordType>(),
            actual.downcast_ref::<RecordType>(),
        ) {
            let (Ok(a), Ok(b)) = (a.fields(), b.fields()) else {
                return false;
            };
            return a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|((an, at), (bn, bt))| an == bn && accepts(ctx, *at, *bt, seen));
        }
        if let (Some(a), Some(b)) = (
            expected.downcast_ref::<perry_mir::VariantType>(),
            actual.downcast_ref::<perry_mir::VariantType>(),
        ) {
            return a.cases().len() == b.cases().len()
                && a.cases().iter().zip(b.cases()).all(|((an, at), (bn, bt))| {
                    an == bn
                        && match (at, bt) {
                            (Some(a), Some(b)) => accepts(ctx, *a, *b, seen),
                            (None, None) => true,
                            _ => false,
                        }
                });
        }
        false
    }
    accepts(ctx, expected, actual, &mut HashSet::new())
}

pub(super) fn wrapper<'h>(
    ctx: &mut Context,
    shapes: &Shapes<'h>,
    binding: &'h str,
    params: Vec<TypeHandle>,
    result: TypeHandle,
) -> Result<Identifier> {
    let key = (binding, params.clone());
    if let Some(name) = shapes.foreign_wrappers.borrow().get(&key) {
        return Ok(name.clone());
    }
    let mut index = shapes.foreign_wrappers.borrow().len();
    let name = loop {
        let name = format!("perry_host_wrapper_{index}");
        if !shapes.has_function(&name)
            && shapes
                .foreign_wrappers
                .borrow()
                .values()
                .all(|other| other.as_ref() != name)
        {
            break identifier(&name)?;
        }
        index += 1;
    };
    let ty = FunctionType::get(ctx, params, vec![result]);
    let function = FuncOp::new(ctx, name.clone(), ty);
    set_async(ctx, &function);
    let block = function.get_entry_block(ctx);
    let mut inserter = IRInserter::<DummyListener>::new_at_block_end(block);
    let args = block.deref(ctx).arguments().collect();
    let transfer = perry_mir::TransferType::get(ctx).into();
    let promise = PromiseType::get(ctx, transfer).into();
    let start = ForeignOp::new(ctx, Phase::Start, binding, args, vec![promise]);
    inserter.append_op(ctx, &start);
    let pending = start.get_operation().deref(ctx).get_result(0);
    let await_op = AwaitOp::new(ctx, pending)?;
    inserter.append_op(ctx, &await_op);
    let finish = ForeignOp::new(
        ctx,
        Phase::Finish,
        binding,
        vec![await_op.get_result(ctx)],
        vec![result],
    );
    inserter.append_op(ctx, &finish);
    let value = finish.get_operation().deref(ctx).get_result(0);
    let ret = ReturnOp::new(ctx, vec![value]);
    inserter.append_op(ctx, &ret);
    shapes
        .module
        .append_operation(ctx, function.get_operation(), 0);
    shapes
        .foreign_wrappers
        .borrow_mut()
        .insert(key, name.clone());
    Ok(name)
}
