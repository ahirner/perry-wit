use super::*;
use crate::{
    ArrayType, RecordKind,
    collections::{ArrayAtOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArrayStoreOp},
};

struct Dictionary {
    record: TypedHandle<RecordType>,
    lengths: Vec<Ptr<Operation>>,
    reads: Vec<(ArrayAtOp, Vec<(usize, FieldAccess)>)>,
    appends: Vec<(Ptr<Operation>, AllocOp)>,
}

impl Dictionary {
    fn record(ctx: &Context, op: Ptr<Operation>) -> Option<TypedHandle<RecordType>> {
        let array = Operation::get_op::<ArrayNewOp>(op, ctx)?;
        if op.deref(ctx).get_num_operands() != 0 {
            return None;
        }
        let value = array.get_result(ctx);
        let array = TypedHandle::<ArrayType>::from_handle(value.get_type(ctx), ctx).ok()?;
        let record =
            TypedHandle::<RecordType>::from_handle(array.deref(ctx).element(), ctx).ok()?;
        if record.deref(ctx).kind() != RecordKind::DictionaryEntry
            || record.deref(ctx).fields().ok()?.is_empty()
        {
            return None;
        }
        Some(record)
    }

    fn private(ctx: &Context, op: Ptr<Operation>) -> Option<Self> {
        let record = Self::record(ctx, op)?;
        let value = op.deref(ctx).get_result(0);
        let mut dictionary = Self {
            record,
            lengths: Vec::new(),
            reads: Vec::new(),
            appends: Vec::new(),
        };
        for usage in value.uses(ctx) {
            if usage.find_index(ctx) != 0 {
                return None;
            }
            let user = usage.user_op();
            if Operation::get_op::<ArrayLenOp>(user, ctx).is_some() {
                dictionary.lengths.push(user);
            } else if let Some(read) = Operation::get_op::<ArrayAtOp>(user, ctx) {
                let fields = read
                    .get_result(ctx)
                    .uses(ctx)
                    .into_iter()
                    .map(|usage| {
                        if usage.find_index(ctx) != 0 {
                            return None;
                        }
                        FieldAccess::resolve(ctx, usage.user_op(), record)
                    })
                    .collect::<Option<Vec<_>>>()?;
                dictionary.reads.push((read, fields));
            } else if Operation::get_op::<ArrayPushOp>(user, ctx).is_some() {
                let entry = user.deref(ctx).get_operand(1);
                let allocation = Operation::get_op::<AllocOp>(entry.defining_op()?, ctx)?;
                if entry.uses(ctx).len() != 1
                    || allocation.get_operation().deref(ctx).get_parent_block()
                        != user.deref(ctx).get_parent_block()
                {
                    return None;
                }
                dictionary.appends.push((user, allocation));
            } else {
                return None;
            }
        }
        (!dictionary.appends.is_empty()).then_some(dictionary)
    }
}

/// Split private, append-only dictionaries after entry reads have proven bounds.
/// Fresh entries have no observable identity; paired appends keep both column lengths equal.
/// Calls, serialization, entry sharing and structural edits retain the ordinary representation.
pub(crate) struct DecomposeDictionaries;

impl MatchRewrite for DecomposeDictionaries {
    fn r#match(&mut self, ctx: &Context, op: Ptr<Operation>) -> bool {
        Dictionary::record(ctx, op).is_some()
    }

    fn rewrite(
        &mut self,
        ctx: &mut Context,
        rewriter: &mut MatchRewriter,
        op: Ptr<Operation>,
    ) -> Result<()> {
        let Some(dictionary) = Dictionary::private(ctx, op) else {
            return Ok(());
        };
        rewriter.set_insertion_point_before_operation(op);
        let field_count = dictionary.record.deref(ctx).fields()?.len();
        let mut columns = Vec::with_capacity(field_count);
        for index in 0..field_count {
            let ty = dictionary.record.deref(ctx).fields()?[index].1;
            let array = ArrayNewOp::new(ctx, ty, Vec::new());
            rewriter.insert_op(ctx, &array);
            columns.push(array.get_result(ctx));
        }
        for length in dictionary.lengths {
            Operation::replace_operand(length, ctx, 0, columns[0]);
        }
        for (read, fields) in dictionary.reads {
            for (field, access) in fields {
                let index = read.get_operation().deref(ctx).get_operand(1);
                let (user, replacement) = match access {
                    FieldAccess::Read(get) => (
                        get.get_operation(),
                        ArrayAtOp::new(ctx, columns[field], index)?.get_operation(),
                    ),
                    FieldAccess::Write(set) => {
                        let value = set.get_operation().deref(ctx).get_operand(1);
                        (
                            set.get_operation(),
                            ArrayStoreOp::new(ctx, columns[field], index, value).get_operation(),
                        )
                    }
                };
                rewriter.set_insertion_point_before_operation(user);
                rewriter.insert_operation(ctx, replacement);
                rewriter.replace_operation(ctx, user, replacement);
            }
            rewriter.erase_operation(ctx, read.get_operation());
        }
        for (append, allocation) in dictionary.appends {
            rewriter.set_insertion_point_before_operation(append);
            for (index, &column) in columns.iter().enumerate() {
                let value = allocation.get_operation().deref(ctx).get_operand(index);
                let push = ArrayPushOp::new(ctx, column, value);
                rewriter.insert_op(ctx, &push);
            }
            rewriter.erase_operation(ctx, append);
            rewriter.erase_operation(ctx, allocation.get_operation());
        }
        rewriter.erase_operation(ctx, op);
        Ok(())
    }
}
