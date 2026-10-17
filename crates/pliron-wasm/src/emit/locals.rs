//! Local allocation: liveness over emitted statements, an interference graph, and greedy
//! coloring that coalesces branch arguments with the block parameters they move into.
use super::{
    EmitResult,
    function::{FunctionEmitter, successors},
};
use crate::{
    control::{BrOp, CondBrOp},
    types::val_type,
};
use pliron::{
    basic_block::BasicBlock, builtin::op_interfaces::BranchOpInterface, context::Ptr,
    linked_list::ContainsLinkedList, operation::Operation, r#type::Typed, value::Value,
};
use std::collections::{HashMap, HashSet};
use wasm_encoder::ValType;

/// Values with locals that a statement reads, and the values it defines.
type Statement = (Vec<Value>, Vec<Value>);

impl FunctionEmitter<'_> {
    /// Give every value that needs a local one, sharing locals between values of one type
    /// whose live ranges do not overlap. Liveness is computed over statements as they are
    /// emitted: a folded tree reads its leaves at its user, block parameters are defined at
    /// their block's start, and branch arguments are read at the branch.
    pub(super) fn assign_locals(&mut self, blocks: &[Ptr<BasicBlock>]) -> EmitResult<()> {
        let ctx = self.ctx;
        // Values that need a local, in definition order, and their types.
        let mut values = Vec::new();
        // Per block: its parameters with locals, and its statements in order.
        let mut params: HashMap<Ptr<BasicBlock>, Vec<Value>> = HashMap::new();
        let mut statements: HashMap<Ptr<BasicBlock>, Vec<Statement>> = HashMap::new();
        for &block in blocks.iter().rev() {
            let defined: Vec<Value> = block
                .deref(ctx)
                .arguments()
                .filter(|v| v.is_used(ctx))
                .collect();
            if block != self.entry {
                values.extend(defined.iter().copied());
            }
            params.insert(block, defined);
            let mut list = Vec::new();
            for op in block.deref(ctx).iter(ctx) {
                if self.in_place(op) {
                    continue;
                }
                let defs: Vec<Value> = op.deref(ctx).results().filter(|v| v.is_used(ctx)).collect();
                values.extend(defs.iter().copied());
                let mut reads = Vec::new();
                self.reads(op, &mut reads);
                list.push((reads, defs));
            }
            statements.insert(block, list);
        }
        let mut types: HashMap<Value, ValType> = HashMap::new();
        for &value in values.iter().chain(self.locals.keys()) {
            let ty = val_type(ctx, value.get_type(ctx))
                .ok_or_else(|| self.malformed("a value has a non-Wasm type"))?;
            types.insert(value, ty);
        }
        let tracked = |value: &Value| types.contains_key(value);
        let live_out = |live_in: &HashMap<Ptr<BasicBlock>, HashSet<Value>>, block| {
            successors(ctx, block)
                .iter()
                .filter_map(|succ| live_in.get(succ))
                .flatten()
                .copied()
                .collect::<HashSet<Value>>()
        };

        // Live-in sets to a fixpoint (postorder converges fastest for backward flow).
        let mut live_in: HashMap<Ptr<BasicBlock>, HashSet<Value>> = HashMap::new();
        loop {
            let mut changed = false;
            for &block in blocks {
                let mut live = live_out(&live_in, block);
                for (reads, defs) in statements[&block].iter().rev() {
                    for def in defs {
                        live.remove(def);
                    }
                    live.extend(reads.iter().copied().filter(tracked));
                }
                for param in &params[&block] {
                    live.remove(param);
                }
                if live_in.get(&block) != Some(&live) {
                    live_in.insert(block, live);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        // Interference: a definition overlaps everything live after it.
        let mut interferes: HashMap<Value, HashSet<Value>> = HashMap::new();
        let mut edge = |a: Value, b: Value| {
            if a != b && types.get(&a) == types.get(&b) {
                interferes.entry(a).or_default().insert(b);
                interferes.entry(b).or_default().insert(a);
            }
        };
        for &block in blocks {
            let mut live = live_out(&live_in, block);
            for (reads, defs) in statements[&block].iter().rev() {
                for (index, def) in defs.iter().enumerate() {
                    for other in live.iter().chain(&defs[index + 1..]) {
                        edge(*def, *other);
                    }
                }
                for def in defs {
                    live.remove(def);
                }
                live.extend(reads.iter().copied().filter(tracked));
            }
            let defined = &params[&block];
            for (index, param) in defined.iter().enumerate() {
                for other in live.iter().chain(&defined[index + 1..]) {
                    edge(*param, *other);
                }
            }
        }

        // Branch arguments and the parameters they move into prefer one local (the move is
        // then dropped).
        let mut hints: HashMap<Value, Vec<Value>> = HashMap::new();
        for &block in blocks {
            let Some(terminator) = block.deref(ctx).get_terminator(ctx) else {
                continue;
            };
            let edges: Vec<(Ptr<BasicBlock>, Vec<Value>)> =
                if let Some(cond_br) = Operation::get_op::<CondBrOp>(terminator, ctx) {
                    (0..2)
                        .map(|succ| {
                            (
                                terminator.deref(ctx).get_successor(succ),
                                cond_br.successor_operands(ctx, succ),
                            )
                        })
                        .collect()
                } else if Operation::get_op::<BrOp>(terminator, ctx).is_some() {
                    vec![(
                        terminator.deref(ctx).get_successor(0),
                        terminator.deref(ctx).operands().collect(),
                    )]
                } else {
                    Vec::new()
                };
            for (dest, args) in edges {
                for (param, arg) in dest.deref(ctx).arguments().zip(args) {
                    if tracked(&param) && tracked(&arg) {
                        hints.entry(param).or_default().push(arg);
                        hints.entry(arg).or_default().push(param);
                    }
                }
            }
        }

        // Greedy coloring: function parameters keep locals 0..n; other values take the first
        // local of their type no neighbor holds, preferring a hinted one.
        let mut color: HashMap<Value, u32> = std::mem::take(&mut self.locals);
        let mut by_type: HashMap<ValType, Vec<u32>> = HashMap::new();
        for (index, ty) in self.local_types.iter().enumerate() {
            by_type.entry(*ty).or_default().push(index as u32);
        }
        let mut next = self.local_types.len() as u32;
        let mut new_types: Vec<(u32, ValType)> = Vec::new();
        for value in values {
            if color.contains_key(&value) {
                continue;
            }
            let ty = types[&value];
            let taken: HashSet<u32> = interferes
                .get(&value)
                .into_iter()
                .flatten()
                .filter_map(|other| color.get(other).copied())
                .collect();
            let of_type = by_type.entry(ty).or_default();
            let hinted = hints
                .get(&value)
                .into_iter()
                .flatten()
                .filter_map(|other| color.get(other).copied())
                .find(|local| !taken.contains(local) && of_type.contains(local));
            let local = match hinted
                .or_else(|| of_type.iter().copied().find(|local| !taken.contains(local)))
            {
                Some(local) => local,
                None => {
                    let local = next;
                    next += 1;
                    of_type.push(local);
                    new_types.push((local, ty));
                    local
                }
            };
            color.insert(value, local);
        }
        // Renumber the new locals grouped by type, so the declaration run-length encodes
        // compactly.
        let rank = |ty: &ValType| match ty {
            ValType::I32 => 0,
            ValType::I64 => 1,
            ValType::F32 => 2,
            ValType::F64 => 3,
            _ => 4,
        };
        new_types.sort_by_key(|(local, ty)| (rank(ty), *local));
        let mut renumber: HashMap<u32, u32> = HashMap::new();
        for (local, ty) in new_types {
            renumber.insert(local, self.local_types.len() as u32);
            self.local_types.push(ty);
        }
        self.locals = color
            .into_iter()
            .map(|(value, local)| (value, renumber.get(&local).copied().unwrap_or(local)))
            .collect();
        Ok(())
    }

    /// Values with locals that evaluating `op` reads: its operands, through folded and
    /// constant definitions, which are evaluated in place.
    fn reads(&self, op: Ptr<Operation>, out: &mut Vec<Value>) {
        let mut previous = None;
        for operand in op.deref(self.ctx).operands() {
            match operand.defining_op() {
                Some(def) if self.in_place(def) => {
                    if previous != Some(def) {
                        self.reads(def, out);
                        previous = Some(def);
                    }
                }
                _ => {
                    out.push(operand);
                    previous = None;
                }
            }
        }
    }
}
