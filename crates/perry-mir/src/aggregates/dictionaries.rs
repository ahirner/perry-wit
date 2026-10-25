use super::*;
use crate::{
    ArrayType, CompareKind, CompareOp, Intrinsic, IntrinsicOp, RecordKind, StringType,
    collections::{ArrayAtOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArrayStoreOp},
};

use pliron::value::DefiningEntity;

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

    fn key_comparisons(&self, ctx: &Context) -> Vec<CompareOp> {
        if !self.record.deref(ctx).fields().is_ok_and(|fields| {
            fields
                .first()
                .is_some_and(|(_, ty)| ty.deref(ctx).is::<StringType>())
        }) {
            return Vec::new();
        }
        let mut seen = HashSet::new();
        let mut comparisons = Vec::new();
        for (_, fields) in &self.reads {
            for (field, access) in fields {
                if *field != 0 {
                    continue;
                }
                let FieldAccess::Read(get) = access else {
                    continue;
                };
                for usage in get.get_result(ctx).uses(ctx) {
                    let user = usage.user_op();
                    if let Some(compare) = Operation::get_op::<CompareOp>(user, ctx)
                        && compare.kind(ctx) == CompareKind::Eq
                        && seen.insert(user)
                    {
                        comparisons.push(compare);
                    }
                }
            }
        }
        comparisons
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
                        FieldAccess::resolve(ctx, usage.user_op(), |field| {
                            record.deref(ctx).field(field).map(|(index, _)| index)
                        })
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
/// String keys cache fingerprints in their slots; equality still verifies the original strings.
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
        let comparisons = dictionary.key_comparisons(ctx);
        let cached_keys = (!comparisons.is_empty()).then_some(columns[0]);
        let mut fingerprints = HashMap::new();
        for length in dictionary.lengths {
            Operation::replace_operand(length, ctx, 0, columns[0]);
        }
        for (read, fields) in dictionary.reads {
            for (field, access) in fields {
                let index = read.get_operation().deref(ctx).get_operand(1);
                let read_field = matches!(access, FieldAccess::Read(_));
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
                if field == 0
                    && let Some(column) = cached_keys
                {
                    if read_field {
                        let prefix =
                            IntrinsicOp::new(ctx, Intrinsic::KeyFingerprintAt, vec![column, index]);
                        rewriter.insert_op(ctx, &prefix);
                        fingerprints.insert(
                            replacement.deref(ctx).get_result(0),
                            prefix.get_operation().deref(ctx).get_result(0),
                        );
                    } else {
                        let key = user.deref(ctx).get_operand(1);
                        let fingerprint = key_fingerprint(ctx, rewriter, &mut fingerprints, key);
                        rewriter.set_insertion_point_before_operation(user);
                        let store = IntrinsicOp::new(
                            ctx,
                            Intrinsic::KeyFingerprintStore,
                            vec![column, index, fingerprint],
                        );
                        rewriter.insert_op(ctx, &store);
                    }
                }
                rewriter.replace_operation(ctx, user, replacement);
            }
            rewriter.erase_operation(ctx, read.get_operation());
        }
        for (append, allocation) in dictionary.appends {
            rewriter.set_insertion_point_before_operation(append);
            for (index, &column) in columns.iter().enumerate() {
                let value = allocation.get_operation().deref(ctx).get_operand(index);
                let fingerprint = if index == 0 && cached_keys.is_some() {
                    let prefix = key_fingerprint(ctx, rewriter, &mut fingerprints, value);
                    rewriter.set_insertion_point_before_operation(append);
                    let length = ArrayLenOp::new(ctx, column);
                    rewriter.insert_op(ctx, &length);
                    Some((length.get_result(ctx), prefix))
                } else {
                    None
                };
                let push = ArrayPushOp::new(ctx, column, value);
                rewriter.insert_op(ctx, &push);
                if let Some((index, fingerprint)) = fingerprint {
                    let cache = IntrinsicOp::new(
                        ctx,
                        Intrinsic::KeyFingerprintStore,
                        vec![column, index, fingerprint],
                    );
                    rewriter.insert_op(ctx, &cache);
                }
            }
            rewriter.erase_operation(ctx, append);
            rewriter.erase_operation(ctx, allocation.get_operation());
        }
        for compare in comparisons {
            let op = compare.get_operation();
            let lhs = op.deref(ctx).get_operand(0);
            let rhs = op.deref(ctx).get_operand(1);
            let left = key_fingerprint(ctx, rewriter, &mut fingerprints, lhs);
            let right = key_fingerprint(ctx, rewriter, &mut fingerprints, rhs);
            rewriter.set_insertion_point_before_operation(op);
            let equal =
                IntrinsicOp::new(ctx, Intrinsic::StringKeyEqual, vec![lhs, left, rhs, right]);
            rewriter.insert_op(ctx, &equal);
            rewriter.replace_operation(ctx, op, equal.get_operation());
        }
        rewriter.erase_operation(ctx, op);
        Ok(())
    }
}

/// Strings are immutable; a fingerprint placed at their definition dominates every lookup use.
fn key_fingerprint(
    ctx: &mut Context,
    rewriter: &mut MatchRewriter,
    fingerprints: &mut HashMap<Value, Value>,
    key: Value,
) -> Value {
    if let Some(&prefix) = fingerprints.get(&key) {
        return prefix;
    }
    match key.defining_entity() {
        DefiningEntity::Op(op) => rewriter.set_insertion_point_after_operation(op),
        DefiningEntity::Block(block) => rewriter.set_insertion_point_to_block_start(block),
    }
    let fingerprint = IntrinsicOp::new(ctx, Intrinsic::StringKeyFingerprint, vec![key]);
    rewriter.insert_op(ctx, &fingerprint);
    let fingerprint = fingerprint.get_operation().deref(ctx).get_result(0);
    fingerprints.insert(key, fingerprint);
    fingerprint
}
