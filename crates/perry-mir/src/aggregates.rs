//! Bounded inlining, read-only argument promotion and scalar replacement.
//!
//! - [inline_leaves]: calls to small synchronous leaf functions (no calls, awaits or
//!   timers of their own) take the callee's body, bottom-up by construction since leaves
//!   call nothing. Callees left without callers are removed unless exported.
//! - [scalar_replace]: a record that does not escape its function (it is only read and
//!   written through `perry.field_get`/`perry.field_set`) becomes one local per field, which
//!   `mem2reg` then promotes to SSA values: the allocation disappears.
use crate::{
    calls::{AsyncCallOp, AwaitOp, CallOp, TimerOp, is_async, is_exported},
    control::{BrOp, ReturnOp},
    exceptions::{CaughtOp, MayThrowInterface},
    heap::{AllocOp, FieldGetOp, FieldSetOp},
    locals::{LoadOp, LocalOp, StoreOp},
    types::RecordType,
};
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        op_interfaces::{OneRegionInterface, OneResultInterface, SymbolOpInterface},
        ops::{FuncOp, ModuleOp},
    },
    context::{Context, Ptr},
    identifier::Identifier,
    irbuild::{
        IRStatus,
        cloning::{IrMapping, clone_blocks_into},
        inserter::{Inserter, OpInsertionPoint},
        listener::DummyListener,
        rewriter::{IRRewriter, Rewriter},
    },
    linked_list::{ContainsLinkedList, LinkedList},
    op::{Op, op_cast},
    operation::Operation,
    result::Result,
    r#type::{Typed, TypedHandle},
    value::Value,
};
use std::collections::{HashMap, HashSet};

/// Callees of at most this many operations are inlined at every call site; larger leaves
/// only where they have a single caller (and are not exported), so code does not grow.
const SMALL: usize = 25;
/// No callee above this size is inlined.
const LIMIT: usize = 64;

pub(crate) fn functions(module: Ptr<Operation>, ctx: &Context) -> Vec<FuncOp> {
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

pub(crate) fn operations(ctx: &Context, function: &FuncOp) -> Vec<Ptr<Operation>> {
    use pliron::builtin::op_interfaces::OneRegionInterface;
    let region = function.get_region(ctx);
    let blocks: Vec<Ptr<BasicBlock>> = region.deref(ctx).iter(ctx).collect();
    blocks
        .into_iter()
        .flat_map(|block| block.deref(ctx).iter(ctx).collect::<Vec<_>>())
        .collect()
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
            Operation::get_op::<CallOp>(op, ctx).is_some()
                || Operation::get_op::<AsyncCallOp>(op, ctx).is_some()
                || Operation::get_op::<AwaitOp>(op, ctx).is_some()
                || Operation::get_op::<TimerOp>(op, ctx).is_some()
                || Operation::get_op::<crate::foreign::ForeignOp>(op, ctx).is_some()
                || op_cast::<dyn MayThrowInterface>(Operation::get_op_dyn(op, ctx).as_ref())
                    .is_some()
                || op.deref(ctx).num_regions() != 0
        })
    {
        return None;
    }
    Some(ops.len())
}

/// Inline calls to small leaf functions; see the module documentation.
pub fn inline_leaves(root: Ptr<Operation>, ctx: &mut Context) -> IRStatus {
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
            let callee = if let Some(call) = Operation::get_op::<CallOp>(op, ctx) {
                call.callee(ctx)
            } else if let Some(call) = Operation::get_op::<AsyncCallOp>(op, ctx) {
                call.callee(ctx)
            } else {
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
            {
                sites.push((op, callee, name.clone()));
            }
        }
    }
    let mut status = IRStatus::Unchanged;
    let mut inlined: HashMap<Identifier, usize> = HashMap::new();
    for (call, callee, caller) in sites {
        let (function, size) = leaves[&callee];
        let single = callers[&callee] == 1 && !is_exported(ctx, &function);
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
        if count == callers[&callee] && !is_exported(ctx, &function) {
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
    let cloned: Vec<Ptr<BasicBlock>> = body
        .iter()
        .map(|block| mapper.lookup_block(*block).expect("cloned block"))
        .collect();
    for block in cloned {
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

/// Functions that may write a record field while they run: those with a `perry.field_set`
/// or an `await` (other tasks run meanwhile), and, to a fixpoint, those calling one.
fn writers(ctx: &Context, functions: &[FuncOp]) -> HashSet<Identifier> {
    let mut calls: HashMap<Identifier, Vec<Identifier>> = HashMap::new();
    let summaries = crate::effects::summarize(ctx, functions);
    let mut writers: HashSet<_> = summaries
        .iter()
        .filter(|(_, s)| s.may_suspend || s.may_reenter)
        .map(|(name, _)| name.clone())
        .collect();
    for function in functions {
        let name = function.get_symbol_name(ctx);
        for op in operations(ctx, function) {
            if Operation::get_op::<FieldSetOp>(op, ctx).is_some()
                || Operation::get_op::<AwaitOp>(op, ctx).is_some()
            {
                writers.insert(name.clone());
            } else if let Some(call) = Operation::get_op::<CallOp>(op, ctx) {
                calls
                    .entry(name.clone())
                    .or_default()
                    .push(call.callee(ctx));
            } else if let Some(call) = Operation::get_op::<AsyncCallOp>(op, ctx) {
                calls
                    .entry(name.clone())
                    .or_default()
                    .push(call.callee(ctx));
            }
        }
    }
    loop {
        let before = writers.len();
        for (caller, callees) in &calls {
            if callees
                .iter()
                .any(|callee| writers.contains(callee) || !summaries.contains_key(callee))
            {
                writers.insert(caller.clone());
            }
        }
        if writers.len() == before {
            return writers;
        }
    }
}

/// The fields a record parameter is read through, when those reads are its only uses (the
/// parameter's escape summary: it does not escape and is not written through).
fn read_only_fields(ctx: &Context, parameter: Value) -> Option<Vec<String>> {
    let mut fields = std::collections::BTreeSet::new();
    for use_ in parameter.uses(ctx) {
        let get = Operation::get_op::<FieldGetOp>(use_.user_op(), ctx)?;
        let field = get.field(ctx);
        fields.insert(field);
    }
    Some(fields.into_iter().collect())
}

/// A record parameter of a
/// synchronous, non-exported function that is only read through fields becomes one parameter
/// per field read, and callers read the fields at the call. This is exact when nothing can
/// write the record while the callee runs: the callee writes no record field, directly or
/// through a call, and does not await. The caller's record then often no longer escapes, so
/// `scalar_replace` dissolves it.
pub fn promote_arguments(root: Ptr<Operation>, ctx: &mut Context) -> IRStatus {
    let functions = functions(root, ctx);
    let writers = writers(ctx, &functions);
    // Direct call sites per callee; callees also called asynchronously are left alone.
    let mut sites: HashMap<Identifier, Vec<Ptr<Operation>>> = HashMap::new();
    let mut async_callees: HashSet<Identifier> = HashSet::new();
    for function in &functions {
        for op in operations(ctx, function) {
            if let Some(call) = Operation::get_op::<CallOp>(op, ctx) {
                sites.entry(call.callee(ctx)).or_default().push(op);
            } else if let Some(call) = Operation::get_op::<AsyncCallOp>(op, ctx) {
                async_callees.insert(call.callee(ctx));
            }
        }
    }
    let mut status = IRStatus::Unchanged;
    for function in &functions {
        let name = function.get_symbol_name(ctx);
        if is_exported(ctx, function)
            || is_async(ctx, function)
            || writers.contains(&name)
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
                sites.get(&name).map_or(&[][..], |v| v),
            );
            // The call sites were replaced; find them again for the next parameter.
            let calls: Vec<Ptr<Operation>> = functions
                .iter()
                .flat_map(|f| operations(ctx, f))
                .filter(|&op| {
                    Operation::get_op::<CallOp>(op, ctx).is_some_and(|c| c.callee(ctx) == name)
                })
                .collect();
            sites.insert(name.clone(), calls);
            status = IRStatus::Changed;
        }
    }
    status
}

/// Replace parameter `index` of `function` with one parameter per field in `fields`, at the
/// end, and rewrite `calls` to read those fields.
fn promote(
    ctx: &mut Context,
    function: &FuncOp,
    index: usize,
    fields: &[String],
    calls: &[Ptr<Operation>],
) {
    use pliron::builtin::{
        attributes::TypeAttr, type_interfaces::FunctionTypeInterface, types::FunctionType,
    };
    let entry = function.get_entry_block(ctx);
    let parameter = entry.deref(ctx).get_argument(index);
    let field_types: Vec<_> = fields
        .iter()
        .map(|field| {
            let record = parameter.get_type(ctx);
            let record = record.deref(ctx);
            let record = record.downcast_ref::<RecordType>().expect("a record");
            record
                .fields()
                .expect("fields")
                .iter()
                .find(|(name, _)| name == field)
                .map(|(_, ty)| *ty)
                .expect("a field of the record")
        })
        .collect();
    // The callee: new parameters replace the reads.
    let first = entry.deref(ctx).get_num_arguments();
    for (offset, ty) in field_types.iter().enumerate() {
        BasicBlock::insert_argument(entry, ctx, first + offset, *ty);
    }
    for use_ in parameter.uses(ctx) {
        let get = use_.user_op();
        let field = Operation::get_op::<FieldGetOp>(get, ctx)
            .expect("read-only parameter")
            .field(ctx);
        let position = fields
            .iter()
            .position(|f| *f == field)
            .expect("listed field");
        let value = entry.deref(ctx).get_argument(first + position);
        let result = get.deref(ctx).get_result(0);
        result.replace_all_uses_with(ctx, &value);
        Operation::erase(get, ctx);
    }
    BasicBlock::remove_argument(entry, ctx, index);
    let arguments: Vec<_> = entry
        .deref(ctx)
        .arguments()
        .map(|argument| argument.get_type(ctx))
        .collect();
    let results = {
        let ty = function.get_type(ctx);
        let ty = ty.deref(ctx);
        ty.downcast_ref::<FunctionType>()
            .expect("a function type")
            .res_types()
    };
    let ty = FunctionType::get(ctx, arguments, results);
    function.set_attr_builtin_func_type(ctx, TypeAttr::new(ty.into()));
    // The callers read the fields at the call.
    for &call in calls {
        let callee = Operation::get_op::<CallOp>(call, ctx)
            .expect("a call")
            .callee(ctx);
        let mut operands: Vec<Value> = call.deref(ctx).operands().collect();
        let record = operands.remove(index);
        for field in fields {
            let get = FieldGetOp::new(ctx, record, field.clone())
                .expect("a field of the record")
                .get_operation();
            get.insert_before(ctx, call);
            operands.push(get.deref(ctx).get_result(0));
        }
        let results: Vec<_> = call
            .deref(ctx)
            .results()
            .map(|result| result.get_type(ctx))
            .collect();
        let replacement = CallOp::new(ctx, callee, operands, results).get_operation();
        replacement.insert_before(ctx, call);
        let old: Vec<Value> = call.deref(ctx).results().collect();
        for (position, value) in old.into_iter().enumerate() {
            let new = replacement.deref(ctx).get_result(position);
            value.replace_all_uses_with(ctx, &new);
        }
        Operation::erase(call, ctx);
    }
}

/// Split a record into local field slots only when all uses are direct field reads/writes.
/// Entry slots plus stores at the original allocation preserve per-iteration initialization;
/// mem2reg then removes the slots and supplies the required control-flow joins.
pub(crate) fn scalar_replace(root: Ptr<Operation>, ctx: &mut Context) -> Result<bool> {
    let mut changed = false;
    let mut rewriter = IRRewriter::<DummyListener>::default();
    for function in functions(root, ctx) {
        let allocations: Vec<_> = function
            .get_region(ctx)
            .deref(ctx)
            .iter(ctx)
            .flat_map(|block| block.deref(ctx).iter(ctx).collect::<Vec<_>>())
            .filter_map(|op| Operation::get_op::<AllocOp>(op, ctx))
            .collect();
        for allocation in allocations {
            let reference = allocation.get_result(ctx);
            let uses = reference.uses(ctx);
            if uses.iter().any(|u| {
                u.find_index(ctx) != 0
                    || (Operation::get_op::<FieldGetOp>(u.user_op(), ctx).is_none()
                        && Operation::get_op::<FieldSetOp>(u.user_op(), ctx).is_none())
            }) {
                continue;
            }
            let record = TypedHandle::<RecordType>::from_handle(reference.get_type(ctx), ctx)?;
            let fields = record.deref(ctx).fields()?.to_vec();
            let initial: Vec<_> = allocation.get_operation().deref(ctx).operands().collect();
            let mut slots = HashMap::new();
            rewriter.set_insertion_point_to_block_start(function.get_entry_block(ctx));
            for (name, ty) in fields {
                let slot = LocalOp::new(ctx, ty);
                rewriter.insert_op(ctx, &slot);
                slots.insert(name, slot.get_result(ctx));
            }
            rewriter.set_insertion_point_before_operation(allocation.get_operation());
            let names: Vec<_> = record
                .deref(ctx)
                .fields()?
                .iter()
                .map(|(name, _)| name.clone())
                .collect();
            for (name, value) in names.into_iter().zip(initial) {
                let store = StoreOp::new(ctx, slots[&name], value);
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
                }
            }
            rewriter.erase_operation(ctx, allocation.get_operation());
            changed = true;
        }
    }
    Ok(changed)
}
