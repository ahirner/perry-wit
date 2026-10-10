//! `wasm` dialect → core Wasm binary.
//!
//! Control flow is structured with Ramsey's dominator-tree translation ("Beyond Relooper",
//! ICFP 2022): every reducible CFG becomes nested `block`/`loop`/`if` with no code duplication
//! and no helper variables. Loop headers get a `loop`, blocks with several forward
//! predecessors ("merge nodes") get an enclosing `block` that forward branches exit, and every
//! other block is emitted inline at its single forward predecessor.
//!
//! Values: a single-use result defined in the same block as its user is evaluated in place on
//! the operand stack (tree folding), and constants are rematerialized at each use. Effects and traps keep their order; pure ops move freely.
//! Every other value, and every block argument, lives in a local. Block arguments are written
//! at each edge by pushing all incoming values before setting any parameter, so cyclic moves
//! (`^loop(b, a)`) need no temporaries.
use crate::{
    attrs::Effect,
    control::{BrOp, BrTableOp, CondBrOp, ReturnOp, UnreachableOp},
    ops::{
        CallOp, ImportFuncOp, LoadOp, MemoryGrowOp, MemorySizeOp, NumOp, SelectOp, StoreOp,
        export_name,
    },
    types::val_type,
};
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        attributes::{FPDoubleAttr, FPSingleAttr, IntegerAttr},
        op_interfaces::{OneRegionInterface, OneResultInterface, SymbolOpInterface},
        ops::{ConstantOp, FuncOp, ModuleOp},
        type_interfaces::FunctionTypeInterface,
        types::FunctionType,
    },
    common_traits::Named,
    context::{Context, Ptr},
    graph::dominance::{DomTree, compute_dominator_tree},
    identifier::Identifier,
    linked_list::ContainsLinkedList,
    op::Op,
    operation::Operation,
    region::Region,
    r#type::{TypeHandle, Typed, TypedHandle},
    utils::apint::APInt,
    value::Value,
};
use std::collections::{HashMap, HashSet};
use thiserror::Error;
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection,
    Function, FunctionSection, ImportSection, IndirectNameMap, Instruction, MemArg, MemorySection,
    MemoryType, Module, NameMap, NameSection, TypeSection, ValType,
};

#[derive(Debug, Error)]
pub enum EmitError {
    #[error("function {function}: control flow is irreducible at block {block}")]
    Irreducible { function: String, block: String },
    #[error("function {function}: {op} is not part of the wasm dialect")]
    UnsupportedOp { function: String, op: String },
    #[error("{0} has a type that is not a Wasm value type")]
    ValueType(String),
    #[error("function {function}: {message}")]
    Malformed { function: String, message: String },
}

type EmitResult<T> = Result<T, EmitError>;

/// Linear memory 0 of the emitted module.
#[derive(Clone, Debug, Default)]
pub enum Memory {
    /// No memory; loads and stores fail validation.
    #[default]
    None,
    /// Import memory 0, e.g. from a runtime module that owns the allocator.
    Import {
        module: String,
        name: String,
        min_pages: u64,
    },
    /// Define memory 0, optionally exporting it.
    Define {
        min_pages: u64,
        export: Option<String>,
    },
}

#[derive(Clone, Debug, Default)]
pub struct EmitOptions {
    pub memory: Memory,
    /// Active data segments of memory 0.
    pub data: Vec<DataSegment>,
    /// Emit a `name` section with function and local names.
    pub names: bool,
}

/// Bytes placed at a fixed address of memory 0 at instantiation.
#[derive(Clone, Debug, Default)]
pub struct DataSegment {
    pub offset: u32,
    pub bytes: Vec<u8>,
}

fn signature(
    ctx: &Context,
    ty: TypeHandle,
    what: &str,
) -> EmitResult<(Vec<ValType>, Vec<ValType>)> {
    let ty = TypedHandle::<FunctionType>::from_handle(ty, ctx)
        .map_err(|_| EmitError::ValueType(what.to_string()))?;
    let ty = ty.deref(ctx);
    let map = |types: Vec<TypeHandle>| -> EmitResult<Vec<ValType>> {
        types
            .into_iter()
            .map(|t| val_type(ctx, t).ok_or_else(|| EmitError::ValueType(what.to_string())))
            .collect()
    };
    Ok((map(ty.arg_types())?, map(ty.res_types())?))
}

/// Encode `module` as a core Wasm module. Imports get the lowest function indices, then
/// functions follow in module order.
pub fn emit_module(ctx: &Context, module: &ModuleOp, options: &EmitOptions) -> EmitResult<Vec<u8>> {
    let block = module
        .get_region(ctx)
        .deref(ctx)
        .get_head()
        .expect("module body");
    let mut imports = Vec::new();
    let mut functions = Vec::new();
    for op in block.deref(ctx).iter(ctx) {
        if let Some(import) = Operation::get_op::<ImportFuncOp>(op, ctx) {
            imports.push(import);
        } else if let Some(function) = Operation::get_op::<FuncOp>(op, ctx) {
            functions.push(function);
        }
    }

    let mut types = TypeSection::new();
    let mut type_index: HashMap<(Vec<ValType>, Vec<ValType>), u32> = HashMap::new();
    let mut intern = |sig: (Vec<ValType>, Vec<ValType>)| -> u32 {
        let next = type_index.len() as u32;
        *type_index.entry(sig.clone()).or_insert_with(|| {
            types.ty().function(sig.0, sig.1);
            next
        })
    };

    let mut function_index: HashMap<Identifier, u32> = HashMap::new();
    let mut names = NameMap::new();
    let mut import_section = ImportSection::new();
    for (index, import) in imports.iter().enumerate() {
        let symbol = import.get_symbol_name(ctx);
        let sig = signature(ctx, import.get_type(ctx), symbol.as_ref())?;
        let ty = intern(sig);
        import_section.import(
            &import.module(ctx),
            &import.name(ctx),
            EntityType::Function(ty),
        );
        function_index.insert(symbol.clone(), index as u32);
        names.append(index as u32, symbol.as_ref());
    }
    if let Memory::Import {
        module,
        name,
        min_pages,
    } = &options.memory
    {
        import_section.import(
            module,
            name,
            EntityType::Memory(MemoryType {
                minimum: *min_pages,
                maximum: None,
                memory64: false,
                shared: false,
                page_size_log2: None,
            }),
        );
    }

    let mut function_section = FunctionSection::new();
    let mut signatures = Vec::new();
    for (offset, function) in functions.iter().enumerate() {
        let symbol = function.get_symbol_name(ctx);
        let sig = signature(ctx, function.get_type(ctx), symbol.as_ref())?;
        function_section.function(intern(sig.clone()));
        signatures.push(sig);
        let index = (imports.len() + offset) as u32;
        function_index.insert(symbol.clone(), index);
        names.append(index, symbol.as_ref());
    }

    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();
    let mut local_names = IndirectNameMap::new();
    for (offset, (function, (params, results))) in functions.iter().zip(signatures).enumerate() {
        let index = (imports.len() + offset) as u32;
        if let Some(name) = export_name(ctx, function) {
            exports.export(&name, ExportKind::Func, index);
        }
        let emitted =
            FunctionEmitter::new(ctx, function, &function_index, params, results)?.emit()?;
        code.function(&emitted.body);
        local_names.append(index, &emitted.names);
    }

    let mut wasm = Module::new();
    wasm.section(&types);
    if !import_section.is_empty() {
        wasm.section(&import_section);
    }
    wasm.section(&function_section);
    if let Memory::Define { min_pages, export } = &options.memory {
        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: *min_pages,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        wasm.section(&memories);
        if let Some(name) = export {
            exports.export(name, ExportKind::Memory, 0);
        }
    }
    if !exports.is_empty() {
        wasm.section(&exports);
    }
    wasm.section(&code);
    if !options.data.is_empty() {
        let mut data = DataSection::new();
        for segment in &options.data {
            data.active(
                0,
                &ConstExpr::i32_const(segment.offset as i32),
                segment.bytes.iter().copied(),
            );
        }
        wasm.section(&data);
    }
    if options.names {
        let mut name_section = NameSection::new();
        name_section.functions(&names);
        name_section.locals(&local_names);
        wasm.section(&name_section);
    }
    Ok(wasm.finish())
}

/// The enclosing constructs of the code being emitted, innermost last; a branch target's
/// label index is its distance from the end.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Frame {
    IfThenElse,
    LoopHeadedBy(Ptr<BasicBlock>),
    BlockFollowedBy(Ptr<BasicBlock>),
}

struct Emitted {
    body: Function,
    names: NameMap,
}

struct FunctionEmitter<'a> {
    ctx: &'a Context,
    name: String,
    function_index: &'a HashMap<Identifier, u32>,
    results: Vec<ValType>,
    entry: Ptr<BasicBlock>,
    order: HashMap<Ptr<BasicBlock>, usize>,
    dom: DomTree<Ptr<Region>, Context>,
    loop_headers: HashSet<Ptr<BasicBlock>>,
    merge_nodes: HashSet<Ptr<BasicBlock>>,
    folded: HashSet<Ptr<Operation>>,
    locals: HashMap<Value, u32>,
    local_types: Vec<ValType>,
    params: usize,
    frames: Vec<Frame>,
    code: Vec<Instruction<'static>>,
}

fn successors(ctx: &Context, block: Ptr<BasicBlock>) -> Vec<Ptr<BasicBlock>> {
    block
        .deref(ctx)
        .get_terminator(ctx)
        .map(|t| t.deref(ctx).successors().collect())
        .unwrap_or_default()
}

/// Constants are rematerialized at every use: `i32.const 0` is as short as a `local.get`.
fn is_constant(ctx: &Context, op: Ptr<Operation>) -> bool {
    Operation::get_op::<ConstantOp>(op, ctx).is_some()
}

/// Whether evaluating `op` later than its position is unobservable: no effects, no traps,
/// no memory reads.
fn movable(ctx: &Context, op: Ptr<Operation>) -> bool {
    if is_constant(ctx, op) {
        return true;
    }
    if Operation::get_op::<SelectOp>(op, ctx).is_some() {
        return true;
    }
    Operation::get_op::<NumOp>(op, ctx).is_some_and(|num| num.opcode(ctx).effect() == Effect::Pure)
}

impl<'a> FunctionEmitter<'a> {
    fn new(
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
        for &block in &postorder {
            for succ in successors(ctx, block) {
                if order[&succ] > order[&block] {
                    *forward_preds.entry(succ).or_default() += 1;
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
        let mut merge_nodes: HashSet<Ptr<BasicBlock>> = forward_preds
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(block, _)| block)
            .collect();
        // A `br_table` reaches its targets by label, so its forward targets are merge nodes
        // even with a single predecessor.
        for &block in &postorder {
            let is_table = block
                .deref(ctx)
                .get_terminator(ctx)
                .is_some_and(|t| Operation::get_op::<BrTableOp>(t, ctx).is_some());
            if is_table {
                for succ in successors(ctx, block) {
                    if order[&succ] > order[&block] {
                        merge_nodes.insert(succ);
                    }
                }
            }
        }

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
            locals: HashMap::new(),
            local_types: Vec::new(),
            params: params.len(),
            frames: Vec::new(),
            code: Vec::new(),
        };
        for (index, arg) in entry.deref(ctx).arguments().enumerate() {
            emitter.locals.insert(arg, index as u32);
        }
        emitter.local_types = params;
        for &block in &postorder {
            emitter.schedule(block);
        }
        emitter.assign_locals(&postorder)?;
        Ok(emitter)
    }

    fn malformed(&self, message: impl Into<String>) -> EmitError {
        EmitError::Malformed {
            function: self.name.clone(),
            message: message.into(),
        }
    }

    /// Decide which single-use values are evaluated in place at their user.
    fn schedule(&mut self, block: Ptr<BasicBlock>) {
        let ctx = self.ctx;
        let ops: Vec<_> = block.deref(ctx).iter(ctx).collect();
        let index: HashMap<_, _> = ops.iter().enumerate().map(|(i, op)| (*op, i)).collect();
        // Whether an op's evaluation tree has an effect, and the earliest op in that tree.
        let mut anchored: Vec<bool> = ops.iter().map(|op| !movable(ctx, *op)).collect();
        let mut start: Vec<usize> = (0..ops.len()).collect();
        for (user_index, &user) in ops.iter().enumerate() {
            let user_op = user.deref(ctx);
            // Branch arguments are evaluated on the edge, after the condition and only on the
            // taken edge, so only effect-free trees fold into them.
            let is_branch = Operation::get_op::<BrOp>(user, ctx).is_some()
                || Operation::get_op::<CondBrOp>(user, ctx).is_some();
            let operands: Vec<Value> = user_op.operands().collect();
            drop(user_op);
            let mut barrier = user_index;
            for (position, operand) in operands.into_iter().enumerate().rev() {
                let edge_argument = is_branch
                    && !(position == 0 && Operation::get_op::<CondBrOp>(user, ctx).is_some());
                let Some(def) = operand.defining_op() else {
                    continue;
                };
                let Some(&def_index) = index.get(&def) else {
                    continue;
                };
                if def_index >= user_index
                    || def.deref(ctx).get_num_results() != 1
                    || operand.num_uses(ctx) != 1
                    || self.folded.contains(&def)
                    || is_constant(ctx, def)
                    || (edge_argument && anchored[def_index])
                {
                    continue;
                }
                if anchored[def_index] {
                    // An effectful tree may only move past pure statements.
                    let crosses_effect = (def_index + 1..barrier)
                        .any(|between| anchored[between] && !self.folded.contains(&ops[between]));
                    if crosses_effect {
                        continue;
                    }
                    barrier = start[def_index];
                }
                self.folded.insert(def);
                anchored[user_index] |= anchored[def_index];
                start[user_index] = start[user_index].min(start[def_index]);
            }
        }
    }

    /// Values with locals that evaluating `op` reads: its operands, through folded and
    /// constant definitions, which are evaluated in place.
    fn reads(&self, op: Ptr<Operation>, out: &mut Vec<Value>) {
        let ctx = self.ctx;
        let operands: Vec<Value> = op.deref(ctx).operands().collect();
        for operand in operands {
            match operand.defining_op() {
                Some(def) if self.folded.contains(&def) || is_constant(ctx, def) => {
                    self.reads(def, out)
                }
                _ => out.push(operand),
            }
        }
    }

    /// Give every value that needs a local one, sharing locals between values of one type
    /// whose live ranges do not overlap. Liveness is computed over statements as they are
    /// emitted: a folded tree reads its leaves at its user, block parameters are defined at
    /// their block's start, and branch arguments are read at the branch.
    fn assign_locals(&mut self, blocks: &[Ptr<BasicBlock>]) -> EmitResult<()> {
        let ctx = self.ctx;
        // Values that need a local, in definition order, and their types.
        let mut values = Vec::new();
        // Per block: its parameters with locals, and (reads, defs) per statement in order.
        type Statement = (Vec<Value>, Vec<Value>);
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
                if self.folded.contains(&op) || is_constant(ctx, op) {
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

        // Live-in sets to a fixpoint (postorder converges fastest for backward flow).
        let mut live_in: HashMap<Ptr<BasicBlock>, HashSet<Value>> = HashMap::new();
        loop {
            let mut changed = false;
            for &block in blocks {
                let mut live: HashSet<Value> = successors(ctx, block)
                    .iter()
                    .filter_map(|succ| live_in.get(succ))
                    .flatten()
                    .copied()
                    .collect();
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
            let mut live: HashSet<Value> = successors(ctx, block)
                .iter()
                .filter_map(|succ| live_in.get(succ))
                .flatten()
                .copied()
                .collect();
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
                    use pliron::builtin::op_interfaces::BranchOpInterface;
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
        let mut color: HashMap<Value, u32> = self.locals.clone();
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
            let hinted = hints
                .get(&value)
                .into_iter()
                .flatten()
                .filter_map(|other| color.get(other).copied())
                .find(|local| !taken.contains(local) && by_type[&ty].contains(local));
            let local = match hinted.or_else(|| {
                by_type
                    .get(&ty)
                    .into_iter()
                    .flatten()
                    .copied()
                    .find(|local| !taken.contains(local))
            }) {
                Some(local) => local,
                None => {
                    let local = next;
                    next += 1;
                    by_type.entry(ty).or_default().push(local);
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
        for (value, local) in color {
            self.locals
                .insert(value, renumber.get(&local).copied().unwrap_or(local));
        }
        Ok(())
    }

    fn emit(mut self) -> EmitResult<Emitted> {
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
        self.code.push(Instruction::End);

        let mut declared: Vec<(u32, ValType)> = Vec::new();
        for ty in &self.local_types[self.params..] {
            match declared.last_mut() {
                Some((count, last)) if last == ty => *count += 1,
                _ => declared.push((1, *ty)),
            }
        }
        let mut body = Function::new(declared);
        for instruction in &peephole(std::mem::take(&mut self.code)) {
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

    fn rpo(&self, block: Ptr<BasicBlock>) -> usize {
        self.order[&block]
    }

    fn do_tree(&mut self, block: Ptr<BasicBlock>) -> EmitResult<()> {
        let mut merges: Vec<_> = self
            .dom
            .children(&block)
            .filter(|child| self.merge_nodes.contains(child) && self.order.contains_key(child))
            .collect();
        // The merge child emitted last (highest reverse postorder) is the outermost block.
        merges.sort_by_key(|child| std::cmp::Reverse(self.rpo(*child)));
        if self.loop_headers.contains(&block) {
            self.code.push(Instruction::Loop(BlockType::Empty));
            self.frames.push(Frame::LoopHeadedBy(block));
            self.node_within(block, &merges)?;
            self.frames.pop();
            self.code.push(Instruction::End);
            Ok(())
        } else {
            self.node_within(block, &merges)
        }
    }

    fn node_within(
        &mut self,
        block: Ptr<BasicBlock>,
        merges: &[Ptr<BasicBlock>],
    ) -> EmitResult<()> {
        if let Some((&follower, inner)) = merges.split_first() {
            self.code.push(Instruction::Block(BlockType::Empty));
            self.frames.push(Frame::BlockFollowedBy(follower));
            self.node_within(block, inner)?;
            self.frames.pop();
            self.code.push(Instruction::End);
            return self.do_tree(follower);
        }
        let ctx = self.ctx;
        let ops: Vec<_> = block.deref(ctx).iter(ctx).collect();
        let Some((&terminator, body)) = ops.split_last() else {
            return Err(self.malformed("empty block"));
        };
        for &op in body {
            self.statement(op)?;
        }
        if let Some(br) = Operation::get_op::<BrOp>(terminator, ctx) {
            let dest = terminator.deref(ctx).get_successor(0);
            let args: Vec<_> = br.get_operation().deref(ctx).operands().collect();
            self.edge(block, dest, &args)
        } else if let Some(cond_br) = Operation::get_op::<CondBrOp>(terminator, ctx) {
            use pliron::builtin::op_interfaces::BranchOpInterface;
            let condition = terminator.deref(ctx).get_operand(0);
            self.value(condition)?;
            self.code.push(Instruction::If(BlockType::Empty));
            self.frames.push(Frame::IfThenElse);
            for succ in 0..2 {
                if succ == 1 {
                    self.code.push(Instruction::Else);
                }
                let dest = terminator.deref(ctx).get_successor(succ);
                let args = cond_br.successor_operands(ctx, succ);
                self.edge(block, dest, &args)?;
            }
            self.frames.pop();
            self.code.push(Instruction::End);
            Ok(())
        } else if Operation::get_op::<BrTableOp>(terminator, ctx).is_some() {
            let index = terminator.deref(ctx).get_operand(0);
            self.value(index)?;
            let mut labels = Vec::new();
            for dest in successors(ctx, block) {
                labels.push(self.label(block, dest)?);
            }
            let default = labels
                .pop()
                .ok_or_else(|| self.malformed("br_table without default"))?;
            self.code.push(Instruction::BrTable(labels.into(), default));
            Ok(())
        } else if Operation::get_op::<ReturnOp>(terminator, ctx).is_some() {
            let values: Vec<_> = terminator.deref(ctx).operands().collect();
            for value in values {
                self.value(value)?;
            }
            self.code.push(Instruction::Return);
            Ok(())
        } else if Operation::get_op::<UnreachableOp>(terminator, ctx).is_some() {
            self.code.push(Instruction::Unreachable);
            Ok(())
        } else {
            Err(EmitError::UnsupportedOp {
                function: self.name.clone(),
                op: Operation::get_op_dyn(terminator, ctx)
                    .get_opid()
                    .to_string(),
            })
        }
    }

    /// Move `args` into `dest`'s parameters, then transfer control.
    fn edge(
        &mut self,
        from: Ptr<BasicBlock>,
        dest: Ptr<BasicBlock>,
        args: &[Value],
    ) -> EmitResult<()> {
        let ctx = self.ctx;
        let moves: Vec<(Value, u32)> = dest
            .deref(ctx)
            .arguments()
            .zip(args)
            .filter_map(|(param, arg)| {
                let local = *self.locals.get(&param)?;
                // Coalescing may give the argument the parameter's local already.
                let in_place = arg
                    .defining_op()
                    .is_none_or(|def| !self.folded.contains(&def) && !is_constant(ctx, def))
                    && self.locals.get(arg) == Some(&local);
                (!in_place).then_some((*arg, local))
            })
            .collect();
        for (arg, _) in &moves {
            self.value(*arg)?;
        }
        for (_, local) in moves.iter().rev() {
            self.code.push(Instruction::LocalSet(*local));
        }
        self.do_branch(from, dest)
    }

    fn do_branch(&mut self, from: Ptr<BasicBlock>, dest: Ptr<BasicBlock>) -> EmitResult<()> {
        let target = if self.rpo(dest) <= self.rpo(from) {
            Frame::LoopHeadedBy(dest)
        } else if self.merge_nodes.contains(&dest) {
            Frame::BlockFollowedBy(dest)
        } else {
            return self.do_tree(dest);
        };
        let depth = self
            .frames
            .iter()
            .rev()
            .position(|frame| *frame == target)
            .ok_or_else(|| self.malformed("branch target is not an enclosing label"))?;
        // A branch is the last code of its construct, and an `if` the last code of its
        // own: when only `if`s lie between it and the block it ends, falling off the arms
        // reaches the same `end`.
        let falls_through = matches!(target, Frame::BlockFollowedBy(_))
            && self
                .frames
                .iter()
                .rev()
                .take(depth)
                .all(|frame| *frame == Frame::IfThenElse);
        if !falls_through {
            self.code.push(Instruction::Br(depth as u32));
        }
        Ok(())
    }

    /// The label depth of a branch from `from` to `dest`, which must be an enclosing loop or
    /// block (a `br_table` target).
    fn label(&self, from: Ptr<BasicBlock>, dest: Ptr<BasicBlock>) -> EmitResult<u32> {
        let target = if self.rpo(dest) <= self.rpo(from) {
            Frame::LoopHeadedBy(dest)
        } else {
            Frame::BlockFollowedBy(dest)
        };
        self.frames
            .iter()
            .rev()
            .position(|frame| *frame == target)
            .map(|depth| depth as u32)
            .ok_or_else(|| self.malformed("br_table target is not an enclosing label"))
    }

    fn statement(&mut self, op: Ptr<Operation>) -> EmitResult<()> {
        let ctx = self.ctx;
        if self.folded.contains(&op) || is_constant(ctx, op) {
            return Ok(());
        }
        let results: Vec<_> = op.deref(ctx).results().collect();
        if movable(ctx, op) && results.iter().all(|v| !v.is_used(ctx)) {
            return Ok(());
        }
        self.tree(op)?;
        for result in results.iter().rev() {
            match self.locals.get(result) {
                Some(local) => self.code.push(Instruction::LocalSet(*local)),
                None => self.code.push(Instruction::Drop),
            }
        }
        Ok(())
    }

    fn value(&mut self, value: Value) -> EmitResult<()> {
        if let Some(def) = value.defining_op()
            && (self.folded.contains(&def) || is_constant(self.ctx, def))
        {
            return self.tree(def);
        }
        let local = *self
            .locals
            .get(&value)
            .ok_or_else(|| self.malformed("a used value has no local"))?;
        self.code.push(Instruction::LocalGet(local));
        Ok(())
    }

    fn tree(&mut self, op: Ptr<Operation>) -> EmitResult<()> {
        let ctx = self.ctx;
        let operands: Vec<_> = op.deref(ctx).operands().collect();
        for operand in operands {
            self.value(operand)?;
        }
        let instruction = if let Some(constant) = Operation::get_op::<ConstantOp>(op, ctx) {
            let attr = constant
                .get_attr_builtin_constant_value(ctx)
                .ok_or_else(|| self.malformed("constant without value"))?;
            if let Some(int) = attr.downcast_ref::<IntegerAttr>() {
                let bits: APInt = int.clone().into();
                match val_type(ctx, constant.get_result(ctx).get_type(ctx)) {
                    Some(ValType::I32) => Instruction::I32Const(bits.to_u64() as u32 as i32),
                    Some(ValType::I64) => Instruction::I64Const(bits.to_u64() as i64),
                    _ => return Err(self.malformed("integer constant of a non-Wasm width")),
                }
            } else if let Some(double) = attr.downcast_ref::<FPDoubleAttr>() {
                Instruction::F64Const(f64::from(double.clone()).into())
            } else if let Some(single) = attr.downcast_ref::<FPSingleAttr>() {
                Instruction::F32Const(f32::from(single.clone()).into())
            } else {
                return Err(self.malformed("unsupported constant attribute"));
            }
        } else if let Some(num) = Operation::get_op::<NumOp>(op, ctx) {
            num.opcode(ctx).instruction()
        } else if Operation::get_op::<SelectOp>(op, ctx).is_some() {
            Instruction::Select
        } else if let Some(load) = Operation::get_op::<LoadOp>(op, ctx) {
            let kind = load.kind(ctx);
            kind.load(memarg(load.offset(ctx), kind.bytes()))
        } else if let Some(store) = Operation::get_op::<StoreOp>(op, ctx) {
            let kind = store.kind(ctx);
            kind.store(memarg(store.offset(ctx), kind.bytes()))
        } else if let Some(call) = Operation::get_op::<CallOp>(op, ctx) {
            let callee = call.callee(ctx);
            let index = *self
                .function_index
                .get(&callee)
                .ok_or_else(|| self.malformed(format!("unknown callee {callee}")))?;
            Instruction::Call(index)
        } else if Operation::get_op::<MemorySizeOp>(op, ctx).is_some() {
            Instruction::MemorySize(0)
        } else if Operation::get_op::<MemoryGrowOp>(op, ctx).is_some() {
            Instruction::MemoryGrow(0)
        } else {
            return Err(EmitError::UnsupportedOp {
                function: self.name.clone(),
                op: Operation::get_op_dyn(op, ctx).get_opid().to_string(),
            });
        };
        self.code.push(instruction);
        Ok(())
    }
}

fn memarg(offset: u32, bytes: u32) -> MemArg {
    MemArg {
        offset: offset.into(),
        align: bytes.trailing_zeros(),
        memory_index: 0,
    }
}

/// Local rewrites of the emitted code: `local.set n; local.get n` is `local.tee n`, an empty
/// `else` arm goes, and an `if` with no arms only drops its condition.
fn peephole(code: Vec<Instruction<'static>>) -> Vec<Instruction<'static>> {
    let mut out: Vec<Instruction<'static>> = Vec::with_capacity(code.len());
    for instruction in code {
        match (&instruction, out.last()) {
            (Instruction::LocalGet(get), Some(Instruction::LocalSet(set))) if get == set => {
                let local = *get;
                out.pop();
                out.push(Instruction::LocalTee(local));
            }
            (Instruction::End, Some(Instruction::Else)) => {
                out.pop();
                if matches!(out.last(), Some(Instruction::If(BlockType::Empty))) {
                    out.pop();
                    out.push(Instruction::Drop);
                } else {
                    out.push(Instruction::End);
                }
            }
            (Instruction::End, Some(Instruction::If(BlockType::Empty))) => {
                out.pop();
                out.push(Instruction::Drop);
            }
            _ => out.push(instruction),
        }
    }
    out
}
