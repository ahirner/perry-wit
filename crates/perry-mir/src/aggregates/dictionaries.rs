use super::*;
use crate::{
    ArrayType, RecordKind,
    collections::{ArrayAtOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArrayStoreOp},
};
use pliron::r#type::TypeHandle;

struct Dictionary {
    fields: Vec<(String, TypeHandle)>,
    lengths: Vec<Ptr<Operation>>,
    reads: Vec<ArrayAtOp>,
    appends: Vec<(Ptr<Operation>, AllocOp)>,
}

impl Dictionary {
    fn private(ctx: &Context, op: Ptr<Operation>) -> Option<Self> {
        let array = Operation::get_op::<ArrayNewOp>(op, ctx)?;
        if op.deref(ctx).get_num_operands() != 0 {
            return None;
        }
        let value = array.get_result(ctx);
        let array = TypedHandle::<ArrayType>::from_handle(value.get_type(ctx), ctx).ok()?;
        let record =
            TypedHandle::<RecordType>::from_handle(array.deref(ctx).element(), ctx).ok()?;
        if record.deref(ctx).kind() != RecordKind::DictionaryEntry {
            return None;
        }
        let mut dictionary = Self {
            fields: record.deref(ctx).fields().ok()?.to_vec(),
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
                if !read.get_result(ctx).uses(ctx).iter().all(|usage| {
                    usage.find_index(ctx) == 0
                        && crate::is_any::<(FieldGetOp, FieldSetOp)>(ctx, usage.user_op())
                }) {
                    return None;
                }
                dictionary.reads.push(read);
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
        (!dictionary.fields.is_empty() && !dictionary.appends.is_empty()).then_some(dictionary)
    }
}

/// Split private, append-only dictionaries after entry reads have proven bounds.
/// Fresh entries have no observable identity; paired appends keep both column lengths equal.
/// Calls, serialization, entry sharing and structural edits retain the ordinary representation.
pub(crate) struct DecomposeDictionaries;

impl MatchRewrite for DecomposeDictionaries {
    fn r#match(&mut self, ctx: &Context, op: Ptr<Operation>) -> bool {
        Dictionary::private(ctx, op).is_some()
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
        let columns: Vec<_> = dictionary
            .fields
            .iter()
            .map(|(_, ty)| {
                let array = ArrayNewOp::new(ctx, *ty, Vec::new());
                rewriter.insert_op(ctx, &array);
                array.get_result(ctx)
            })
            .collect();
        for length in dictionary.lengths {
            Operation::replace_operand(length, ctx, 0, columns[0]);
        }
        for read in dictionary.reads {
            let index = read.get_operation().deref(ctx).get_operand(1);
            for usage in read.get_result(ctx).uses(ctx) {
                let user = usage.user_op();
                rewriter.set_insertion_point_before_operation(user);
                let get = Operation::get_op::<FieldGetOp>(user, ctx);
                let field = get.map_or_else(
                    || {
                        Operation::get_op::<FieldSetOp>(user, ctx)
                            .expect("field write")
                            .field(ctx)
                    },
                    |get| get.field(ctx),
                );
                let column = columns[dictionary
                    .fields
                    .iter()
                    .position(|(name, _)| *name == field)
                    .expect("verified field")];
                let replacement = if get.is_some() {
                    ArrayAtOp::new(ctx, column, index)?.get_operation()
                } else {
                    let value = user.deref(ctx).get_operand(1);
                    ArrayStoreOp::new(ctx, column, index, value).get_operation()
                };
                rewriter.insert_operation(ctx, replacement);
                rewriter.replace_operation(ctx, user, replacement);
            }
            rewriter.erase_operation(ctx, read.get_operation());
        }
        for (append, allocation) in dictionary.appends {
            rewriter.set_insertion_point_before_operation(append);
            let values: Vec<_> = allocation.get_operation().deref(ctx).operands().collect();
            for (&column, value) in columns.iter().zip(values) {
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
