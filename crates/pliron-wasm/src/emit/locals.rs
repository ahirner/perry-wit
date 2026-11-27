//! Local allocation: liveness over emitted statements, an interference graph, and greedy
//! coloring that coalesces branch arguments with the block parameters they move into.
use super::{
    EmitResult,
    function::FunctionEmitter,
    values::{TreeStep, value_tree},
};
use crate::types::val_type;
use pliron::{
    basic_block::BasicBlock, context::Ptr, linked_list::ContainsLinkedList, r#type::Typed,
    value::Value,
};
use std::collections::{HashMap, HashSet};
use wasm_encoder::ValType;

impl FunctionEmitter<'_> {
    /// Give every value that needs a local one, sharing locals between values of one type
    /// whose live ranges do not overlap. Liveness is computed over statements as they are
    /// emitted: a folded tree reads its leaves at its user, block parameters are defined at
    /// their block's start, and branch arguments are read at the branch.
    pub(super) fn assign_locals(&mut self, blocks: &[Ptr<BasicBlock>]) -> EmitResult<()> {
        let ctx = self.ctx;
        // Values that need a local, in definition order, and their types.
        let mut values = Vec::new();
        for &block in blocks.iter().rev() {
            let body = block.deref(ctx);
            if block != self.entry {
                values.extend(body.arguments().filter(|v| v.is_used(ctx)));
            }
            for op in body.iter(ctx).filter(|&op| !self.in_place(op)) {
                values.extend(op.deref(ctx).results().filter(|v| v.is_used(ctx)));
            }
        }
        let mut types: HashMap<Value, ValType> = HashMap::new();
        for &value in values.iter().chain(self.locals.keys()) {
            let ty = val_type(ctx, value.get_type(ctx))
                .ok_or_else(|| self.malformed("a value has a non-Wasm type"))?;
            types.insert(value, ty);
        }
        let tracked = |value: &Value| types.contains_key(value);
        // Scheduling moves reads within a block. SSA live-out is unchanged at every edge.
        let intervals = pliron_control::liveness::LiveValues::for_blocks(ctx, blocks);

        // Interference: a definition overlaps everything live after it.
        let mut interferes: HashMap<Value, HashSet<Value>> = HashMap::new();
        let mut edge = |a: Value, b: Value| {
            if a != b && types.get(&a) == types.get(&b) {
                interferes.entry(a).or_default().insert(b);
                interferes.entry(b).or_default().insert(a);
            }
        };
        for &block in blocks {
            let body = block.deref(ctx);
            let terminator = body
                .get_terminator(ctx)
                .ok_or_else(|| self.malformed("missing terminator"))?;
            let mut live: HashSet<_> = intervals
                .after(terminator)
                .values()
                .filter(tracked)
                .collect();
            let mut define = |defs: Vec<Value>, live: &mut HashSet<Value>| {
                for (index, &def) in defs.iter().enumerate() {
                    for &other in live.iter().chain(&defs[index + 1..]) {
                        edge(def, other);
                    }
                }
                for def in defs {
                    live.remove(&def);
                }
            };
            for op in body.iter(ctx).rev().filter(|&op| !self.in_place(op)) {
                define(op.deref(ctx).results().filter(tracked).collect(), &mut live);
                live.extend(
                    value_tree(ctx, &self.folded, op).filter_map(|step| match step {
                        TreeStep::Local(value) => Some(value),
                        TreeStep::Operation(_) => None,
                    }),
                );
            }
            define(body.arguments().filter(tracked).collect(), &mut live);
        }

        // Branch arguments and the parameters they move into prefer one local (the move is
        // then dropped).
        let mut hints: HashMap<Value, Vec<Value>> = HashMap::new();
        for &block in blocks {
            let Some(terminator) = block.deref(ctx).get_terminator(ctx) else {
                continue;
            };
            for (position, param) in pliron_control::forwarded_arguments(ctx, terminator) {
                let arg = terminator.deref(ctx).get_operand(position);
                if tracked(&param) && tracked(&arg) {
                    hints.entry(param).or_default().push(arg);
                    hints.entry(arg).or_default().push(param);
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
}
