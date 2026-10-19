//! The lowering of one function: its builder state, the mapping of MIR blocks and values,
//! and the translation of its ops in reverse post-order.
use crate::{
    analysis::value_order,
    async_split,
    module::ModuleState,
    repr::i32_type,
    values::{self, Carriers},
};
use anyhow::{Context as _, Result};
use perry_mir::CaughtOp;
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        op_interfaces::{OneRegionInterface, SymbolOpInterface},
        ops::FuncOp,
        types::FunctionType,
    },
    context::{Context, Ptr},
    linked_list::ContainsLinkedList,
    operation::Operation,
    region::Region,
    r#type::{TypeHandle, Typed},
    value::Value,
};
use std::collections::HashMap;

/// Builds the Wasm body of one function (or of generated glue) into the module state.
/// Methods are spread over the topical modules; the construction helpers are in `builder`.
pub(crate) struct FunctionLowering<'s, 'e> {
    pub(crate) state: &'s mut ModuleState<'e>,
    pub(crate) region: Ptr<Region>,
    /// The entry block, where values that must dominate every use are computed.
    pub(crate) entry: Ptr<BasicBlock>,
    /// The block new ops are appended to.
    pub(crate) block: Ptr<BasicBlock>,
    /// Wasm value of each MIR value defined so far.
    pub(crate) values: HashMap<Value, Carriers<Value>>,
    /// Wasm block of each MIR block.
    pub(crate) blocks: HashMap<Ptr<BasicBlock>, Ptr<BasicBlock>>,
    /// Whether the function returns an exception status after its results.
    pub(crate) throws: bool,
    pub(crate) atomic_construction: bool,
    /// Wasm result types, without the status.
    pub(crate) results: Vec<TypeHandle>,
    pub(crate) throw_block: Option<Ptr<BasicBlock>>,
    /// While lowering an op followed by `perry.caught`: where its exception goes instead of
    /// leaving the function. The block takes the status, then the op's results (zero on the
    /// exceptional edge), so the results dominate the code after it.
    pub(crate) landing: Option<(Ptr<BasicBlock>, Vec<TypeHandle>)>,
    /// The frame, when lowering the step function of an async function.
    pub(crate) frame: Option<async_split::Frame>,
    /// The runtime's table of one-character ASCII strings, once loaded.
    pub(crate) ascii_strings: Option<Value>,
    /// Root stack length at entry, restored on every exit.
    pub(crate) root_base: Option<Value>,
    /// The root stack descriptor, once loaded.
    pub(crate) root_stack: Option<Value>,
    /// Bounded activation storage of the frame-tier records, if any.
    pub(crate) local_frame: Option<Value>,
    /// Offset of each frame-tier allocation in [Self::local_frame].
    pub(crate) frame_slots: HashMap<Ptr<Operation>, u32>,
    /// While lowering canonical arguments: allocations are roots until the call returns.
    pub(crate) canonical_borrows: bool,
    /// The canonical resource scope and the root mark to restore when it fails.
    pub(crate) resource_scope: Option<(Value, Option<Value>)>,
}

impl<'s, 'e> FunctionLowering<'s, 'e> {
    /// A builder appending to the entry block of `function`, which returns `results` and does
    /// not throw. Used for component glue.
    pub(crate) fn glue(state: &'s mut ModuleState<'e>, ctx: &Context, function: &FuncOp) -> Self {
        let entry = function.get_entry_block(ctx);
        FunctionLowering {
            state,
            region: function.get_region(ctx),
            entry,
            block: entry,
            values: HashMap::new(),
            blocks: HashMap::new(),
            throws: false,
            atomic_construction: false,
            results: vec![],
            throw_block: None,
            landing: None,
            frame: None,
            ascii_strings: None,
            root_base: None,
            root_stack: None,
            local_frame: None,
            frame_slots: HashMap::new(),
            canonical_borrows: false,
            resource_scope: None,
        }
    }

    /// Lower MIR function `mir` (not async) to a Wasm function with the same symbol.
    pub(crate) fn lower(
        ctx: &mut Context,
        state: &'s mut ModuleState<'e>,
        mir: &FuncOp,
    ) -> Result<FuncOp> {
        let symbol = mir.get_symbol_name(ctx);
        let throws = state.may_throw(&symbol);
        let effects = state.program_facts.function_summary(ctx, mir)?.effects;
        let (mir_params, mir_results) = crate::analysis::function_signature(ctx, mir)?;
        let params = values::signature(ctx, mir_params)?;
        let results = values::signature(ctx, mir_results)?;
        let mut signature_results = results.clone();
        if throws {
            signature_results.push(i32_type(ctx));
        }
        let fn_ty = FunctionType::get(ctx, params, signature_results);
        let function = FuncOp::new(ctx, symbol, fn_ty);
        let entry = function.get_entry_block(ctx);
        let mut lowering = FunctionLowering::glue(state, ctx, &function);
        lowering.throws = throws;
        lowering.atomic_construction = effects.atomic_construction;
        lowering.results = results;
        lowering.map_blocks(ctx, mir, Some(entry))?;
        if effects.may_collect {
            lowering.enter_roots(ctx);
        }
        lowering.prepare_frame_storage(ctx, mir)?;
        lowering
            .blocks
            .insert(mir.get_entry_block(ctx), lowering.block);
        lowering.translate(ctx, mir)?;
        Ok(function)
    }

    /// Create a Wasm block for every block of `mir` (the MIR entry maps to `entry` if given)
    /// and map block arguments. Returns the block of the MIR entry.
    pub(crate) fn map_blocks(
        &mut self,
        ctx: &mut Context,
        mir: &FuncOp,
        entry: Option<Ptr<BasicBlock>>,
    ) -> Result<Ptr<BasicBlock>> {
        let mir_blocks: Vec<_> = mir.get_region(ctx).deref(ctx).iter(ctx).collect();
        for (index, &mir_block) in mir_blocks.iter().enumerate() {
            let block = match entry {
                Some(entry) if index == 0 => entry,
                _ => {
                    let mir_types: Vec<_> = mir_block
                        .deref(ctx)
                        .arguments()
                        .map(|arg| arg.get_type(ctx))
                        .collect();
                    let types = values::signature(ctx, mir_types)?;
                    self.new_block(ctx, types)
                }
            };
            let logical: Vec<_> = mir_block.deref(ctx).arguments().collect();
            let physical: Vec<_> = block.deref(ctx).arguments().collect();
            self.bind_values(ctx, logical, physical)?;
            self.blocks.insert(mir_block, block);
        }
        Ok(self.blocks[&mir_blocks[0]])
    }

    /// Translate the ops of every block of `mir` (after [Self::map_blocks]). In a step
    /// function, values kept in the frame are stored where they are defined.
    pub(crate) fn translate(&mut self, ctx: &mut Context, mir: &FuncOp) -> Result<()> {
        let mir_blocks: Vec<_> =
            pliron::graph::traversals::region::DFSTraversal::new(ctx, &mir.get_region(ctx))
                .reverse_post_order()
                .collect();
        let live = self.state.live;
        let order = value_order(ctx, mir);
        for mir_block in mir_blocks {
            self.block = self.blocks[&mir_block];
            let args: Vec<_> = mir_block.deref(ctx).arguments().collect();
            self.spill(ctx, &args);
            let ops: Vec<_> = mir_block.deref(ctx).iter(ctx).collect();
            let mut index = 0;
            while index < ops.len() {
                let op = ops[index];
                if Operation::get_op::<perry_mir::AwaitOp>(op, ctx).is_some() {
                    self.clear_dead_frame_slots(ctx, op, live.after(op));
                }
                let root_mark = self.safe_point(ctx, op, live.after(op), &order)?;
                // `op; status = perry.caught`: route the exception of `op` to `status`.
                let caught = ops
                    .get(index + 1)
                    .copied()
                    .filter(|next| Operation::get_op::<CaughtOp>(*next, ctx).is_some());
                if caught.is_some() {
                    let mir_types: Vec<_> =
                        op.deref(ctx).results().map(|r| r.get_type(ctx)).collect();
                    let types = values::signature(ctx, mir_types)?;
                    let mut args = vec![i32_type(ctx)];
                    args.extend(types.iter().copied());
                    self.landing = Some((self.new_block(ctx, args), types));
                }
                let lowered = self.op(ctx, op).with_context(|| {
                    format!("lowering {}", Operation::get_op_dyn(op, ctx).get_opid())
                });
                let landing = self.landing.take();
                lowered?;
                let results: Vec<_> = op.deref(ctx).results().collect();
                self.spill(ctx, &results);
                if let (Some(caught), Some((landing, _))) = (caught, landing) {
                    let zero = self.i32_const(ctx, 0);
                    let mut args = vec![zero];
                    for result in &results {
                        args.extend(self.values[result].iter());
                    }
                    self.br(ctx, landing, args);
                    self.block = landing;
                    let landing_args: Vec<_> = landing.deref(ctx).arguments().collect();
                    let status = landing_args[0];
                    self.bind_values(
                        ctx,
                        results.iter().copied(),
                        landing_args[1..].iter().copied(),
                    )?;
                    self.spill(ctx, &results);
                    self.define(ctx, caught, status);
                    let results: Vec<_> = caught.deref(ctx).results().collect();
                    self.spill(ctx, &results);
                    index += 1;
                }
                self.end_safe_point(ctx, root_mark);
                index += 1;
            }
        }
        Ok(())
    }

    /// Store the frame-resident values among `values` into their slots.
    pub(crate) fn spill(&mut self, ctx: &mut Context, values: &[Value]) {
        let Some(frame) = &self.frame else {
            return;
        };
        let frame_value = frame.frame;
        let stores: Vec<_> = values
            .iter()
            .filter_map(|v| frame.slots.get(v).map(|slot| (*v, *slot)))
            .collect();
        for (value, slot) in stores {
            if let Some(&wasm) = self.values.get(&value) {
                for ((offset, kind), carrier) in slot.iter().zip(wasm.iter()) {
                    self.store(ctx, kind, frame_value, offset, carrier);
                }
            }
        }
    }
}
