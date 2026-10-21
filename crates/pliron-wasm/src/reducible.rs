//! Making control flow reducible, so that [crate::emit] can structure it.
//!
//! A cycle with several entry blocks gets a *guard* block that every edge into an entry,
//! from outside or inside the cycle, is redirected through. The guard takes an `i32` label
//! naming the original target followed by the block arguments of every entry, and dispatches
//! on the label. It becomes the cycle's only header. This follows the guard/dispatch
//! construction of Johnson and Unger (as in LLVM's `FixIrreducible`): every run takes the
//! original blocks plus the guard, and since any path in the new graph minus the guards is a
//! path in the old one, definitions still dominate their uses.
//!
//! Cycles are found as strongly connected components, outermost first. After fixing a
//! component, the components nested within it (excluding its header) are processed recursively.
use crate::{
    attrs::NumOpcode,
    control::{BrOp, CondBrOp},
    ops::NumOp,
    types::{I32_BITS, I64_BITS, integer_attr, val_type},
};
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        attr_interfaces::TypedAttrInterface,
        attributes::{FPDoubleAttr, FPSingleAttr},
        op_interfaces::{BranchOpInterface, OneRegionInterface},
        ops::{ConstantOp, FuncOp},
        types::{IntegerType, Signedness},
    },
    context::{Context, Ptr},
    op::Op,
    operation::Operation,
    region::Region,
    r#type::{TypeHandle, Typed},
    value::Value,
};
use std::collections::{HashMap, HashSet};
use wasm_encoder::ValType;

/// Make the CFG of `function` reducible. Returns the number of guard blocks inserted.
///
/// # Panics
///
/// Panics if a `wasm.br_table` branches directly to an entry of a multi-entry cycle rather
/// than routing through an intermediate block.
pub fn make_reducible(ctx: &mut Context, function: &FuncOp) -> usize {
    let entry = function.get_entry_block(ctx);
    let region = function.get_region(ctx);
    let mut guards = 0;
    let mut work = vec![reachable(ctx, entry)];
    while let Some(nodes) = work.pop() {
        let set: HashSet<_> = nodes.iter().copied().collect();
        for component in components(ctx, &nodes, &set) {
            if !is_cycle(ctx, &component) {
                continue;
            }
            let members: HashSet<_> = component.iter().copied().collect();
            let preds = predecessors(ctx, entry);
            let entries: Vec<_> = component
                .iter()
                .copied()
                .filter(|block| {
                    *block == entry
                        || preds
                            .get(block)
                            .is_some_and(|preds| preds.iter().any(|p| !members.contains(p)))
                })
                .collect();
            let header = if entries.len() > 1 {
                assert!(
                    !entries.contains(&entry),
                    "the function entry block cannot be a loop entry"
                );
                guards += 1;
                insert_guard(ctx, region, &entries, &preds)
            } else {
                entries[0]
            };
            let inner: Vec<_> = component.into_iter().filter(|b| *b != header).collect();
            if !inner.is_empty() {
                work.push(inner);
            }
        }
    }
    guards
}

fn reachable(ctx: &Context, entry: Ptr<BasicBlock>) -> Vec<Ptr<BasicBlock>> {
    let mut seen = HashSet::from([entry]);
    let mut order = vec![entry];
    let mut stack = vec![entry];
    while let Some(block) = stack.pop() {
        for succ in block.deref(ctx).succs(ctx) {
            if seen.insert(succ) {
                order.push(succ);
                stack.push(succ);
            }
        }
    }
    order
}

fn predecessors(
    ctx: &Context,
    entry: Ptr<BasicBlock>,
) -> HashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>> {
    let mut preds: HashMap<_, Vec<_>> = HashMap::new();
    for block in reachable(ctx, entry) {
        for succ in block.deref(ctx).succs(ctx) {
            preds.entry(succ).or_default().push(block);
        }
    }
    preds
}

fn is_cycle(ctx: &Context, component: &[Ptr<BasicBlock>]) -> bool {
    component.len() > 1 || component[0].deref(ctx).succs(ctx).contains(&component[0])
}

/// Strongly connected components of the subgraph induced by `nodes` (Tarjan), each in the
/// order of `nodes`.
fn components(
    ctx: &Context,
    nodes: &[Ptr<BasicBlock>],
    set: &HashSet<Ptr<BasicBlock>>,
) -> Vec<Vec<Ptr<BasicBlock>>> {
    struct Tarjan<'a> {
        ctx: &'a Context,
        set: &'a HashSet<Ptr<BasicBlock>>,
        index: HashMap<Ptr<BasicBlock>, usize>,
        low: HashMap<Ptr<BasicBlock>, usize>,
        stack: Vec<Ptr<BasicBlock>>,
        on_stack: HashSet<Ptr<BasicBlock>>,
        out: Vec<Vec<Ptr<BasicBlock>>>,
    }
    impl Tarjan<'_> {
        fn visit(&mut self, block: Ptr<BasicBlock>) {
            let next = self.index.len();
            self.index.insert(block, next);
            self.low.insert(block, next);
            self.stack.push(block);
            self.on_stack.insert(block);
            for succ in block.deref(self.ctx).succs(self.ctx) {
                if !self.set.contains(&succ) {
                    continue;
                }
                if !self.index.contains_key(&succ) {
                    self.visit(succ);
                    let low = self.low[&block].min(self.low[&succ]);
                    self.low.insert(block, low);
                } else if self.on_stack.contains(&succ) {
                    let low = self.low[&block].min(self.index[&succ]);
                    self.low.insert(block, low);
                }
            }
            if self.low[&block] == self.index[&block] {
                // `block` is on the stack, so this pops exactly its component.
                let mut component = Vec::new();
                while let Some(member) = self.stack.pop() {
                    self.on_stack.remove(&member);
                    component.push(member);
                    if member == block {
                        break;
                    }
                }
                self.out.push(component);
            }
        }
    }
    let mut tarjan = Tarjan {
        ctx,
        set,
        index: HashMap::new(),
        low: HashMap::new(),
        stack: Vec::new(),
        on_stack: HashSet::new(),
        out: Vec::new(),
    };
    for &node in nodes {
        if !tarjan.index.contains_key(&node) {
            tarjan.visit(node);
        }
    }
    let position: HashMap<_, _> = nodes.iter().enumerate().map(|(i, b)| (*b, i)).collect();
    for component in &mut tarjan.out {
        component.sort_by_key(|b| position[b]);
    }
    tarjan.out
}

fn i32_type(ctx: &mut Context) -> TypeHandle {
    IntegerType::get(ctx, 32, Signedness::Signless).into()
}

/// A zero constant of `ty`, inserted before `mark`.
fn zero_before(ctx: &mut Context, ty: TypeHandle, mark: Ptr<Operation>) -> Value {
    let attr: Box<dyn TypedAttrInterface> = match val_type(ctx, ty) {
        Some(ValType::I64) => Box::new(integer_attr(ctx, I64_BITS, 0)),
        Some(ValType::F32) => Box::new(FPSingleAttr::from(0.0f32)),
        Some(ValType::F64) => Box::new(FPDoubleAttr::from(0.0f64)),
        _ => Box::new(integer_attr(ctx, I32_BITS, 0)),
    };
    let op = ConstantOp::new(ctx, attr).get_operation();
    op.insert_before(ctx, mark);
    op.deref(ctx).get_result(0)
}

fn i32_const(ctx: &mut Context, value: u32) -> Ptr<Operation> {
    let attr = integer_attr(ctx, I32_BITS, value.into());
    ConstantOp::new(ctx, Box::new(attr)).get_operation()
}

fn i32_const_at_back(ctx: &mut Context, block: Ptr<BasicBlock>, value: u32) -> Value {
    let op = i32_const(ctx, value);
    op.insert_at_back(block, ctx);
    op.deref(ctx).get_result(0)
}

fn i32_const_before(ctx: &mut Context, value: u32, mark: Ptr<Operation>) -> Value {
    let op = i32_const(ctx, value);
    op.insert_before(ctx, mark);
    op.deref(ctx).get_result(0)
}

/// Insert the guard for `entries` and redirect every edge into them through it.
fn insert_guard(
    ctx: &mut Context,
    region: Ptr<Region>,
    entries: &[Ptr<BasicBlock>],
    preds: &HashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>>,
) -> Ptr<BasicBlock> {
    // Guard arguments: the label, then each entry's arguments.
    let mut types = vec![i32_type(ctx)];
    let mut ranges = Vec::new();
    for entry in entries {
        let start = types.len();
        let args: Vec<_> = entry
            .deref(ctx)
            .arguments()
            .map(|a| a.get_type(ctx))
            .collect();
        types.extend(args);
        ranges.push(start..types.len());
    }
    let guard = BasicBlock::new(ctx, None, types.clone());
    guard.insert_at_back(region, ctx);

    // Dispatch: entry i when label == i, the last entry otherwise.
    let label = guard.deref(ctx).get_argument(0);
    let mut block = guard;
    for (index, entry) in entries.iter().enumerate() {
        let args: Vec<_> = ranges[index]
            .clone()
            .map(|i| guard.deref(ctx).get_argument(i))
            .collect();
        if index + 1 == entries.len() {
            BrOp::new(ctx, *entry, args)
                .get_operation()
                .insert_at_back(block, ctx);
            break;
        }
        let matches = if index == 0 {
            let op = NumOp::new(ctx, NumOpcode::I32Eqz, vec![label]).get_operation();
            op.insert_at_back(block, ctx);
            op.deref(ctx).get_result(0)
        } else {
            let expected = i32_const_at_back(ctx, block, index as u32);
            let op = NumOp::new(ctx, NumOpcode::I32Eq, vec![label, expected]).get_operation();
            op.insert_at_back(block, ctx);
            op.deref(ctx).get_result(0)
        };
        let next = BasicBlock::new(ctx, None, vec![]);
        next.insert_at_back(region, ctx);
        CondBrOp::new(ctx, matches, *entry, args, next, vec![])
            .get_operation()
            .insert_at_back(block, ctx);
        block = next;
    }

    // Redirect every edge into an entry.
    let mut sources: Vec<Ptr<BasicBlock>> = Vec::new();
    for entry in entries {
        for pred in preds.get(entry).into_iter().flatten() {
            if !sources.contains(pred) {
                sources.push(*pred);
            }
        }
    }
    // The edge to `succ` with `args`, through the guard if `succ` is an entry.
    let redirect =
        |ctx: &mut Context, succ: Ptr<BasicBlock>, args: Vec<Value>, terminator: Ptr<Operation>| {
            let Some(index) = entries.iter().position(|e| *e == succ) else {
                return (succ, args);
            };
            let mut guard_args = vec![i32_const_before(ctx, index as u32, terminator)];
            let mut supplied = args.into_iter();
            for (other, range) in ranges.iter().enumerate() {
                for position in range.clone() {
                    guard_args.push(if other == index {
                        supplied
                            .next()
                            .expect("a verified branch passes one operand per block argument")
                    } else {
                        zero_before(ctx, types[position], terminator)
                    });
                }
            }
            (guard, guard_args)
        };
    for source in sources {
        let terminator = source
            .deref(ctx)
            .get_terminator(ctx)
            .expect("a predecessor block ends in a terminator");
        let replacement = if let Some(cond_br) = Operation::get_op::<CondBrOp>(terminator, ctx) {
            let condition = cond_br.get_operand_condition(ctx);
            let [(then_dest, then_args), (else_dest, else_args)] = [0, 1].map(|succ| {
                let dest = terminator.deref(ctx).get_successor(succ);
                let args = cond_br.successor_operands(ctx, succ);
                redirect(ctx, dest, args, terminator)
            });
            CondBrOp::new(ctx, condition, then_dest, then_args, else_dest, else_args)
                .get_operation()
        } else if Operation::get_op::<BrOp>(terminator, ctx).is_some() {
            let dest = terminator.deref(ctx).get_successor(0);
            let args = terminator.deref(ctx).operands().collect();
            let (dest, args) = redirect(ctx, dest, args, terminator);
            BrOp::new(ctx, dest, args).get_operation()
        } else {
            panic!(
                "unexpected branch kind (a wasm.br_table must not target an irreducible loop \
                 entry; branch to a block that does)"
            )
        };
        replacement.insert_before(ctx, terminator);
        Operation::erase(terminator, ctx);
    }
    guard
}
