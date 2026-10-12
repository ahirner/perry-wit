//! Per-function emission state: the CFG analyses the structuring needs (reverse postorder,
//! dominator tree, loop headers, merge nodes), and assembling the finished body.
use super::{EmitError, EmitResult, peephole::peephole};
use crate::control::BrTableOp;
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        op_interfaces::{OneRegionInterface, SymbolOpInterface},
        ops::FuncOp,
    },
    common_traits::Named,
    context::{Context, Ptr},
    graph::dominance::{DomTree, compute_dominator_tree},
    identifier::Identifier,
    operation::Operation,
    region::Region,
    value::Value,
};
use std::collections::{HashMap, HashSet};
use wasm_encoder::{Function, Instruction, NameMap, ValType};

/// The enclosing constructs of the code being emitted, innermost last; a branch target's
/// label index is its distance from the end.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Frame {
    IfThenElse,
    LoopHeadedBy(Ptr<BasicBlock>),
    BlockFollowedBy(Ptr<BasicBlock>),
}

/// A function's code-section body and its local names.
pub(super) struct Emitted {
    pub(super) body: Function,
    pub(super) names: NameMap,
}

/// Emits one `builtin.func`: [FunctionEmitter::new] analyses the CFG, schedules values onto
/// the stack and allocates locals; [FunctionEmitter::emit] then structures and encodes it.
pub(super) struct FunctionEmitter<'a> {
    pub(super) ctx: &'a Context,
    pub(super) name: String,
    pub(super) function_index: &'a HashMap<Identifier, u32>,
    results: Vec<ValType>,
    pub(super) entry: Ptr<BasicBlock>,
    /// Reverse-postorder position of every reachable block.
    pub(super) order: HashMap<Ptr<BasicBlock>, usize>,
    pub(super) dom: DomTree<Ptr<Region>, Context>,
    pub(super) loop_headers: HashSet<Ptr<BasicBlock>>,
    pub(super) merge_nodes: HashSet<Ptr<BasicBlock>>,
    /// Single-use definitions evaluated in place at their user.
    pub(super) folded: HashSet<Ptr<Operation>>,
    pub(super) locals: HashMap<Value, u32>,
    pub(super) local_types: Vec<ValType>,
    params: usize,
    pub(super) frames: Vec<Frame>,
    pub(super) code: Vec<Instruction<'static>>,
}

pub(super) fn successors(ctx: &Context, block: Ptr<BasicBlock>) -> Vec<Ptr<BasicBlock>> {
    block
        .deref(ctx)
        .get_terminator(ctx)
        .map(|t| t.deref(ctx).successors().collect())
        .unwrap_or_default()
}

impl<'a> FunctionEmitter<'a> {
    pub(super) fn new(
        ctx: &'a Context,
        function: &FuncOp,
        function_index: &'a HashMap<Identifier, u32>,
        params: Vec<ValType>,
        results: Vec<ValType>,
    ) -> EmitResult<Self> {
        let name = function.get_symbol_name(ctx).to_string();
        let region = function.get_region(ctx);
        let entry = function.get_entry_block(ctx);

        // Reverse postorder over reachable blocks.
        let mut postorder = Vec::new();
        let mut seen = HashSet::from([entry]);
        let mut stack = vec![(entry, successors(ctx, entry), 0usize)];
        while let Some((block, succs, next)) = stack.last_mut() {
            if let Some(&succ) = succs.get(*next) {
                *next += 1;
                if seen.insert(succ) {
                    let succs = successors(ctx, succ);
                    stack.push((succ, succs, 0));
                }
            } else {
                postorder.push(*block);
                stack.pop();
            }
        }
        let order: HashMap<_, _> = postorder
            .iter()
            .rev()
            .enumerate()
            .map(|(index, block)| (*block, index))
            .collect();
        let dom = compute_dominator_tree(ctx, &region);

        let mut loop_headers = HashSet::new();
        let mut forward_preds: HashMap<Ptr<BasicBlock>, usize> = HashMap::new();
        // A `br_table` reaches its targets by label, so its forward targets are merge nodes
        // even with a single predecessor.
        let mut table_targets = HashSet::new();
        for &block in &postorder {
            let is_table = block
                .deref(ctx)
                .get_terminator(ctx)
                .is_some_and(|t| Operation::get_op::<BrTableOp>(t, ctx).is_some());
            for succ in successors(ctx, block) {
                if order[&succ] > order[&block] {
                    *forward_preds.entry(succ).or_default() += 1;
                    if is_table {
                        table_targets.insert(succ);
                    }
                } else if dom.dominates(&succ, &block) {
                    loop_headers.insert(succ);
                } else {
                    return Err(EmitError::Irreducible {
                        function: name,
                        block: succ.deref(ctx).unique_name(ctx).to_string(),
                    });
                }
            }
        }
        let mut merge_nodes = table_targets;
        merge_nodes.extend(
            forward_preds
                .into_iter()
                .filter(|(_, count)| *count > 1)
                .map(|(block, _)| block),
        );

        let mut emitter = FunctionEmitter {
            ctx,
            name,
            function_index,
            results,
            entry,
            order,
            dom,
            loop_headers,
            merge_nodes,
            folded: HashSet::new(),
            locals: entry
                .deref(ctx)
                .arguments()
                .enumerate()
                .map(|(index, arg)| (arg, index as u32))
                .collect(),
            params: params.len(),
            local_types: params,
            frames: Vec::new(),
            code: Vec::new(),
        };
        for &block in &postorder {
            emitter.schedule(block);
        }
        emitter.assign_locals(&postorder)?;
        Ok(emitter)
    }

    pub(super) fn malformed(&self, message: impl Into<String>) -> EmitError {
        EmitError::Malformed {
            function: self.name.clone(),
            message: message.into(),
        }
    }

    /// Structure and encode the function body.
    pub(super) fn emit(mut self) -> EmitResult<Emitted> {
        self.do_tree(self.entry)?;
        let falls_off = !matches!(
            self.code.last(),
            Some(
                Instruction::Return
                    | Instruction::Unreachable
                    | Instruction::Br(_)
                    | Instruction::BrTable(..)
            )
        );
        if falls_off && !self.results.is_empty() {
            self.code.push(Instruction::Unreachable);
        }
        if matches!(self.code.last(), Some(Instruction::Return)) {
            self.code.pop();
        }
        self.code.push(Instruction::End);

        let mut declared: Vec<(u32, ValType)> = Vec::new();
        for ty in &self.local_types[self.params..] {
            match declared.last_mut() {
                Some((count, last)) if last == ty => *count += 1,
                _ => declared.push((1, *ty)),
            }
        }
        let mut body = Function::new(declared);
        for instruction in &peephole(self.code) {
            body.instruction(instruction);
        }
        let mut names = NameMap::new();
        let mut named: Vec<_> = self
            .locals
            .iter()
            .map(|(value, index)| (*index, value.unique_name(self.ctx).to_string()))
            .collect();
        named.sort();
        named.dedup_by_key(|(index, _)| *index);
        for (index, name) in named {
            names.append(index, &name);
        }
        Ok(Emitted { body, names })
    }

    /// The reverse-postorder position of a reachable block.
    pub(super) fn rpo(&self, block: Ptr<BasicBlock>) -> usize {
        self.order[&block]
    }
}
