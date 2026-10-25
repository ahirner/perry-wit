//! Bounded inlining, read-only argument promotion and scalar replacement.
//!
//! - [inline_leaves]: calls to small synchronous leaf functions take the callee's body,
//!   bottom-up by construction. Callees with zero remaining callers are removed unless exported.
//! - [DecomposeRecords]: a function-local record (accessed solely through
//!   `perry.field_get`/`perry.field_set`) becomes one local per field, which `mem2reg` promotes
//!   to SSA values.
use crate::{
    calls::{AsyncCallOp, CallOp, is_async, is_exported},
    control::{BrOp, ReturnOp},
    exceptions::CaughtOp,
    heap::{AllocOp, FieldGetOp, FieldSetOp},
    locals::{LoadOp, LocalOp, StoreOp},
    semantics::OperationKind,
    types::RecordType,
};
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        attributes::TypeAttr,
        op_interfaces::{OneResultInterface, SymbolOpInterface},
        ops::{FuncOp, ModuleOp},
        type_interfaces::FunctionTypeInterface,
        types::FunctionType,
    },
    context::{Context, Ptr},
    identifier::Identifier,
    irbuild::{
        IRStatus,
        cloning::{IrMapping, clone_blocks_into},
        inserter::{Inserter, OpInsertionPoint},
        listener::DummyListener,
        match_rewrite::{MatchRewrite, MatchRewriter},
        rewriter::{IRRewriter, Rewriter},
    },
    linked_list::{ContainsLinkedList, LinkedList},
    op::Op,
    operation::Operation,
    result::Result,
    r#type::{Typed, TypedHandle},
    value::Value,
};
use std::collections::{BTreeMap, HashMap, HashSet};

mod dictionaries;
pub(crate) use dictionaries::DecomposeDictionaries;

/// Callees with at most this many operations are inlined at every call site. Larger leaf
/// functions are inlined only when they have a single caller and are private to the module.
const SMALL: usize = 25;
/// No callee above this size is inlined.
const LIMIT: usize = 64;

pub fn functions(module: Ptr<Operation>, ctx: &Context) -> Vec<FuncOp> {
    let Some(module) = Operation::get_op::<ModuleOp>(module, ctx) else {
        return Vec::new();
    };
    use pliron::builtin::op_interfaces::SingleBlockRegionInterface;
    module
        .get_body(ctx, 0)
        .deref(ctx)
        .iter(ctx)
        .filter_map(|op| Operation::get_op::<FuncOp>(op, ctx))
        .collect()
}

pub fn operations(ctx: &Context, function: &FuncOp) -> Vec<Ptr<Operation>> {
    use pliron::builtin::op_interfaces::OneRegionInterface;
    let region = function.get_region(ctx);
    let mut operations = Vec::new();
    for block in region.deref(ctx).iter(ctx) {
        operations.extend(block.deref(ctx).iter(ctx));
    }
    operations
}

/// A function that can be inlined: synchronous, at most [LIMIT] operations, and calling
/// nothing (no calls, async calls, awaits, timers or WIT imports).
fn is_leaf(ctx: &Context, function: &FuncOp) -> Option<usize> {
    if is_async(ctx, function) {
        return None;
    }
    let ops = operations(ctx, function);
    if ops.len() > LIMIT
        || ops.iter().any(|&op| {
            matches!(
                OperationKind::classify(ctx, op),
                OperationKind::Call { .. }
                    | OperationKind::Await
                    | OperationKind::Timer
                    | OperationKind::Foreign(_)
            ) || crate::intrinsics::may_throw(ctx, op)
                || op.deref(ctx).num_regions() != 0
        })
    {
        return None;
    }
    Some(ops.len())
}

/// Inline calls to small leaf functions as described in the module documentation.
pub(crate) fn inline_leaves(root: Ptr<Operation>, ctx: &mut Context) -> IRStatus {
    let functions = functions(root, ctx);
    let mut leaves: HashMap<Identifier, (FuncOp, usize)> = HashMap::new();
    for function in &functions {
        if let Some(size) = is_leaf(ctx, function) {
            leaves.insert(function.get_symbol_name(ctx), (*function, size));
        }
    }
    // Caught calls retain their status boundary even for nonthrowing leaf candidates.
    let mut callers: HashMap<Identifier, usize> = HashMap::new();
    let mut sites = Vec::new();
    let mut budgets = HashMap::new();
    for function in &functions {
        let name = function.get_symbol_name(ctx);
        for op in operations(ctx, function) {
            let Some(callee) = crate::calls::callee(ctx, op) else {
                continue;
            };
            *callers.entry(callee.clone()).or_default() += 1;
            let caught = op
                .deref(ctx)
                .get_next()
                .is_some_and(|next| Operation::get_op::<CaughtOp>(next, ctx).is_some());
            if Operation::get_op::<CallOp>(op, ctx).is_some()
                && callee != name
                && leaves.contains_key(&callee)
                && !caught
                && !function
                    .get_operation()
                    .deref(ctx)
                    .attributes
                    .0
                    .contains_key(&crate::facts::SPECIALIZATION)
            {
                sites.push((op, callee, name.clone()));
            }
        }
    }
    let mut status = IRStatus::Unchanged;
    let mut inlined: HashMap<Identifier, usize> = HashMap::new();
    for (call, callee, caller) in sites {
        let (function, size) = leaves[&callee];
        let external =
            is_exported(ctx, &function) || crate::calls::is_specialized_entry(ctx, &function);
        let single = callers[&callee] == 1 && !external;
        if size > SMALL && !single {
            continue;
        }
        let budget = budgets.entry(caller).or_insert(256usize);
        if size > *budget {
            continue;
        }
        *budget -= size;
        inline_call(ctx, call, &function);
        *inlined.entry(callee).or_default() += 1;
        status = IRStatus::Changed;
    }
    // Callees with no call left go, unless the world exports them.
    for (callee, count) in inlined {
        let (function, _) = leaves[&callee];
        let external =
            is_exported(ctx, &function) || crate::calls::is_specialized_entry(ctx, &function);
        if count == callers[&callee] && !external {
            Operation::erase(function.get_operation(), ctx);
        }
    }
    status
}

/// Replace `call` with a copy of `callee`'s body: the caller's block is split after the
/// call, the copy is entered with the call's operands, and its returns branch to the
/// continuation with their values.
fn inline_call(ctx: &mut Context, call: Ptr<Operation>, callee: &FuncOp) {
    use pliron::builtin::op_interfaces::OneRegionInterface;
    let block = call.deref(ctx).get_parent_block().expect("call in a block");
    let region = block
        .deref(ctx)
        .get_parent_region()
        .expect("block in a region");
    let mut rewriter = IRRewriter::<DummyListener>::default();
    let continuation =
        rewriter.split_block(ctx, block, OpInsertionPoint::AfterOperation(call), None);
    let results: Vec<Value> = call.deref(ctx).results().collect();
    for (index, result) in results.iter().enumerate() {
        let ty = result.get_type(ctx);
        BasicBlock::insert_argument(continuation, ctx, index, ty);
        let argument = continuation.deref(ctx).get_argument(index);
        result.replace_all_uses_with(ctx, &argument);
    }
    let body: Vec<Ptr<BasicBlock>> = callee.get_region(ctx).deref(ctx).iter(ctx).collect();
    let mut mapper = IrMapping::new();
    clone_blocks_into(&body, region, ctx, &mut rewriter, &mut mapper);
    let entry = mapper
        .lookup_block(body[0])
        .expect("the callee's entry is cloned");
    // The copy's returns continue after the call.
    for &block in &body {
        let block = mapper.lookup_block(block).expect("cloned block");
        let terminator = block.deref(ctx).get_terminator(ctx).expect("terminated");
        if Operation::get_op::<ReturnOp>(terminator, ctx).is_some() {
            let values: Vec<Value> = terminator.deref(ctx).operands().collect();
            let branch = BrOp::new(ctx, continuation, values).get_operation();
            branch.insert_before(ctx, terminator);
            Operation::erase(terminator, ctx);
        }
    }
    let arguments: Vec<Value> = call.deref(ctx).operands().collect();
    let enter = BrOp::new(ctx, entry, arguments).get_operation();
    enter.insert_after(ctx, call);
    Operation::erase(call, ctx);
}

/// The fields a record parameter is read through when all of its uses are `perry.field_get` reads.
fn read_only_fields(ctx: &Context, parameter: Value) -> Option<BTreeMap<String, FieldReads>> {
    let mut fields = BTreeMap::<_, FieldReads>::new();
    for use_ in parameter.uses(ctx) {
        let get = Operation::get_op::<FieldGetOp>(use_.user_op(), ctx)?;
        fields
            .entry(get.field(ctx))
            .and_modify(|field| field.others.push(get))
            .or_insert_with(|| FieldReads {
                first: get,
                others: Vec::new(),
            });
    }
    Some(fields)
}

/// Verified field projections for a read-only record parameter.
struct FieldReads {
    first: FieldGetOp,
    others: Vec<FieldGetOp>,
}

/// Promote a read-only record parameter of a synchronous, internal function into one
/// parameter per field read, rewriting callers to pass those fields at the call site.
/// Applies when the callee preserves all record fields and stays synchronous, enabling
/// caller-side scalar replacement via [DecomposeRecords].
pub(crate) fn promote_arguments(
    root: Ptr<Operation>,
    ctx: &mut Context,
    effects: &crate::facts::ProgramFacts,
) -> Result<IRStatus> {
    let functions = functions(root, ctx);
    // Direct call sites per callee; callees also called asynchronously are left alone.
    let mut sites: HashMap<Identifier, Vec<CallOp>> = HashMap::new();
    let mut async_callees: HashSet<Identifier> = HashSet::new();
    for function in &functions {
        for op in operations(ctx, function) {
            if let Some(call) = Operation::get_op::<CallOp>(op, ctx) {
                sites.entry(call.callee(ctx)).or_default().push(call);
            } else if let Some(call) = Operation::get_op::<AsyncCallOp>(op, ctx) {
                async_callees.insert(call.callee(ctx));
            }
        }
    }
    let mut status = IRStatus::Unchanged;
    for function in &functions {
        let name = function.get_symbol_name(ctx);
        if is_exported(ctx, function)
            || crate::calls::is_specialized_entry(ctx, function)
            || is_async(ctx, function)
            || effects
                .function_summary(ctx, function)?
                .effects
                .may_mutate_records()
            || async_callees.contains(&name)
        {
            continue;
        }
        let entry = function.get_entry_block(ctx);
        // Promote from the last parameter so earlier indices stay valid.
        let count = entry.deref(ctx).get_num_arguments();
        for index in (0..count).rev() {
            let parameter = entry.deref(ctx).get_argument(index);
            let is_record = parameter.get_type(ctx).deref(ctx).is::<RecordType>();
            if !is_record {
                continue;
            }
            let Some(fields) = read_only_fields(ctx, parameter) else {
                continue;
            };
            promote(
                ctx,
                function,
                index,
                &fields,
                sites.get_mut(&name).map_or(&mut [][..], Vec::as_mut_slice),
            )?;
            status = IRStatus::Changed;
        }
    }
    Ok(status)
}

/// Replace parameter `index` of `function` with one parameter per field in `fields`, at the
/// end, and rewrite `calls` to read those fields.
fn promote(
    ctx: &mut Context,
    function: &FuncOp,
    index: usize,
    fields: &BTreeMap<String, FieldReads>,
    calls: &mut [CallOp],
) -> Result<()> {
    let entry = function.get_entry_block(ctx);
    let mut rewriter = IRRewriter::<DummyListener>::default();
    // The callee: new parameters replace the reads.
    let first = entry.deref(ctx).get_num_arguments();
    for (offset, field) in fields.values().enumerate() {
        let ty = field.first.get_result(ctx).get_type(ctx);
        BasicBlock::insert_argument(entry, ctx, first + offset, ty);
        let value = entry.deref(ctx).get_argument(first + offset);
        for get in std::iter::once(&field.first).chain(&field.others) {
            rewriter.replace_operation_with_values(ctx, get.get_operation(), vec![value]);
        }
    }
    BasicBlock::remove_argument(entry, ctx, index);
    let arguments: Vec<_> = entry
        .deref(ctx)
        .arguments()
        .map(|argument| argument.get_type(ctx))
        .collect();
    let results = TypedHandle::<FunctionType>::from_handle(function.get_type(ctx), ctx)?
        .deref(ctx)
        .res_types();
    let ty = FunctionType::get(ctx, arguments, results);
    function.set_attr_builtin_func_type(ctx, TypeAttr::new(ty.into()));
    // The callers read the fields at the call.
    for site in calls {
        let call = site.get_operation();
        let callee = site.callee(ctx);
        let mut operands: Vec<Value> = call.deref(ctx).operands().collect();
        let record = operands.remove(index);
        for field in fields.keys() {
            let get = FieldGetOp::new(ctx, record, field.clone())?;
            get.get_operation().insert_before(ctx, call);
            operands.push(get.get_result(ctx));
        }
        let results: Vec<_> = call
            .deref(ctx)
            .results()
            .map(|result| result.get_type(ctx))
            .collect();
        let replacement = CallOp::new(ctx, callee, operands, results);
        replacement.get_operation().insert_before(ctx, call);
        rewriter.replace_operation(ctx, call, replacement.get_operation());
        *site = replacement;
    }
    Ok(())
}

/// Split a record into local field slots when all uses are direct field reads and writes.
/// Entry slots combined with stores at the original allocation site preserve per-iteration
/// initialization. A subsequent `mem2reg` pass promotes the slots and inserts control-flow joins.
pub(crate) struct DecomposeRecords;

impl MatchRewrite for DecomposeRecords {
    fn r#match(&mut self, ctx: &Context, op: Ptr<Operation>) -> bool {
        let Some(allocation) = Operation::get_op::<AllocOp>(op, ctx) else {
            return false;
        };
        allocation.get_result(ctx).uses(ctx).iter().all(|usage| {
            usage.find_index(ctx) == 0
                && (Operation::get_op::<FieldGetOp>(usage.user_op(), ctx).is_some()
                    || Operation::get_op::<FieldSetOp>(usage.user_op(), ctx).is_some()
                    || Operation::get_op::<crate::IntrinsicOp>(usage.user_op(), ctx).is_some_and(
                        |intr| {
                            matches!(
                                intr.intrinsic(ctx),
                                crate::Intrinsic::JsonSpread
                                    | crate::Intrinsic::JsonExclude
                                    | crate::Intrinsic::JsonInsert
                                    | crate::Intrinsic::JsonOrderCopy
                            )
                        },
                    ))
        })
    }

    fn rewrite(
        &mut self,
        ctx: &mut Context,
        rewriter: &mut MatchRewriter,
        op: Ptr<Operation>,
    ) -> Result<()> {
        if !self.r#match(ctx, op) {
            return Ok(());
        }
        let allocation = Operation::get_op::<AllocOp>(op, ctx).expect("matched allocation");
        let function = Operation::get_op::<FuncOp>(
            op.deref(ctx)
                .get_parent_op(ctx)
                .expect("allocation in function"),
            ctx,
        )
        .expect("MIR function");
        let reference = allocation.get_result(ctx);
        let uses = reference.uses(ctx);
        let record = TypedHandle::<RecordType>::from_handle(reference.get_type(ctx), ctx)?;
        let fields = record.deref(ctx).fields()?.to_vec();
        let initial: Vec<_> = allocation.get_operation().deref(ctx).operands().collect();
        let mut slots = HashMap::with_capacity(fields.len());
        let mut ordered_slots = Vec::with_capacity(fields.len());
        rewriter.set_insertion_point_to_block_start(function.get_entry_block(ctx));
        for (name, ty) in fields {
            let slot = LocalOp::new(ctx, ty);
            rewriter.insert_op(ctx, &slot);
            let slot_val = slot.get_result(ctx);
            ordered_slots.push(slot_val);
            slots.insert(name, slot_val);
        }
        rewriter.set_insertion_point_before_operation(allocation.get_operation());
        for (slot, value) in ordered_slots.into_iter().zip(initial) {
            let store = StoreOp::new(ctx, slot, value);
            rewriter.insert_op(ctx, &store);
        }
        for use_ in uses {
            let op = use_.user_op();
            rewriter.set_insertion_point_before_operation(op);
            if let Some(get) = Operation::get_op::<FieldGetOp>(op, ctx) {
                let load = LoadOp::new(ctx, slots[&get.field(ctx)])?;
                rewriter.insert_op(ctx, &load);
                rewriter.replace_operation(ctx, op, load.get_operation());
            } else if let Some(set) = Operation::get_op::<FieldSetOp>(op, ctx) {
                let value = op.deref(ctx).get_operand(1);
                let store = StoreOp::new(ctx, slots[&set.field(ctx)], value);
                rewriter.insert_op(ctx, &store);
                rewriter.replace_operation(ctx, op, store.get_operation());
            } else {
                rewriter.erase_operation(ctx, op);
            }
        }
        rewriter.erase_operation(ctx, allocation.get_operation());
        Ok(())
    }
}
