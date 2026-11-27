//! Worklist monomorphization and storage-join solver over dense indices.
use crate::source::SourceModule;
mod array_layouts;
mod callbacks;
mod calls;
mod escape;
mod eval;
mod expr;
mod facts;
mod instances;
mod ir;
mod members;
mod methods;
mod promises;
mod writes;
use super::bitset::BitSet;
use super::boundary::{GenericOwner, LocalFlags, ModuleIndex, TemplateKind};
use super::builtins::{self, MethodResultArgs};
use super::types::{
    ActivationSpan, CaptureOwner, CellKind, ClassId, ClassInstId, DispatchArmSpan, FieldLocSpan,
    FuncIdentity, HelperId, Idx, IdxVec, InstanceId, LocId, LocalIdx, LocalStorage, ObjectProp,
    Pool, TemplateId, Ty, TyKind, TypeTable,
};
use crate::{
    Absence, hir_compat,
    narrowing::{DefinedGuard, Guards},
    shapes::{self, Declarations},
};
use array_layouts::{ArgumentKey, ArrayLayouts};
use eval::first_type_var_name;
use facts::{Fact, Facts, Subscriptions};
pub(crate) use ir::*;
use perry_hir::{
    ir::{CompareOp, Expr, LogicalOp, Stmt},
    types::{LocalId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};
use pliron::context::Context;
use std::collections::HashMap;

const INSTANCE_BUDGET: u16 = 256;

type InstanceKey = (
    TemplateId,
    Option<Receiver<ArgumentKey>>,
    CaptureOwner,
    ArgumentKey,
    Ty,
    Option<Ty>,
    bool,
);

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct ResultContract<'h> {
    byte_offset: u32,
    caller: InstanceId,
    parameter: LocalId,
    inst: InstanceId,
    expected: Ty,
    param_name: &'h str,
}

#[derive(Clone, Copy)]
struct TypeScope {
    declaration: GenericOwner,
    arguments: Ty,
}

impl ModuleIndex<'_> {
    fn substitute(&self, types: &mut TypeTable, ty: Ty, scopes: &[TypeScope]) -> Ty {
        if scopes.is_empty() {
            return ty;
        }
        types.substitute_with(ty, |types, name| {
            scopes.iter().find_map(|scope| {
                let &ordinal = self.type_slots.get(&(scope.declaration, name))?;
                Some(
                    types
                        .tuple_slice(scope.arguments)
                        .get(ordinal)
                        .copied()
                        .unwrap_or(Ty::OPEN),
                )
            })
        })
    }
}

struct InstanceReq<'a> {
    witness: Option<&'a hir_compat::Witness<'a, hir_compat::Target<perry_hir::types::FuncId>>>,
    template: TemplateId,
    receiver: Option<Receiver>,
    capture_owner: CaptureOwner,
    args: &'a [Ty],
    explicit_type_args: &'a [Ty],
    call_byte_offset: u32,
    caller: Option<InstanceId>,
    invocation: Invocation,
}

impl<'a> InstanceReq<'a> {
    fn new(template: TemplateId, args: &'a [Ty], caller: Option<InstanceId>) -> Self {
        Self {
            template,
            witness: None,
            receiver: None,
            capture_owner: CaptureOwner::None,
            args,
            explicit_type_args: &[],
            call_byte_offset: 0,
            caller,
            invocation: Invocation::Function,
        }
    }
    fn receiver(mut self, r: Option<Receiver>) -> Self {
        self.receiver = r;
        self
    }
    fn owner(mut self, o: CaptureOwner) -> Self {
        self.capture_owner = o;
        self
    }
    fn type_args(mut self, t: &'a [Ty]) -> Self {
        self.explicit_type_args = t;
        self
    }
    fn at(mut self, offset: u32) -> Self {
        self.call_byte_offset = offset;
        self
    }
    fn callback(mut self, lexical: bool) -> Self {
        self.invocation = if lexical {
            Invocation::Lexical
        } else {
            Invocation::Inline
        };
        self
    }
}

struct EvalState<'h> {
    inst: InstanceId,
    receiver: Option<Receiver>,
    returned: Ty,
    has_return_value: bool,
    guards: Guards<'h>,
    local_changed: bool,
    current_span: Option<swc_common::Span>,
}

impl EvalState<'_> {
    fn new(inst: InstanceId, receiver: Option<Receiver>) -> Self {
        Self {
            inst,
            receiver,
            returned: Ty::BOTTOM,
            has_return_value: false,
            guards: Guards::default(),
            local_changed: false,
            current_span: None,
        }
    }
}

pub(super) struct CaptureLocations {
    pub(super) slots: Pool<ActivationSpan, LocId>,
    collapsed: IdxVec<LocalIdx, Option<LocId>>,
}

impl CaptureLocations {
    pub(super) fn get(
        &self,
        index: &ModuleIndex,
        instances: &IdxVec<InstanceId, Instance>,
        mut owner: CaptureOwner,
        local: LocalIdx,
    ) -> Option<LocId> {
        let info = index.locals[local];
        if let Some(loc) = info.module_loc() {
            return Some(loc);
        }
        loop {
            match owner {
                CaptureOwner::Instance(id) => {
                    if instances[id].template == info.owner
                        && let LocalStorage::Activation(slot) = info.storage
                    {
                        return self.slots[instances[id].activation]
                            .get(slot as usize)
                            .copied();
                    }
                    owner = instances[id].capture_owner;
                }
                CaptureOwner::Template(_) => return self.collapsed[local],
                CaptureOwner::None => return None,
            }
        }
    }
}

pub(crate) fn solve<'h>(
    ctx: &Context,
    hir: &'h SourceModule<'h>,
    index: &ModuleIndex<'h>,
    decls: &Declarations,
    types: &mut TypeTable,
) -> Result<Solved> {
    let mut solver = Solver::new(ctx, hir, index, decls, types);
    let init_inst = solver.seed_roots()?;
    solver.run_worklist()?;
    let live = solver.live_instances(|_| {});
    if !solver.instances.iter_enumerated().any(|(id, inst)| {
        live.contains(id.index())
            && matches!(
                index.view(inst.template).kind,
                TemplateKind::Function | TemplateKind::Closure(_)
            )
    }) {
        for (tid, view) in index.views.iter_enumerated() {
            if view.kind == TemplateKind::Function
                && !view.is_generator
                && view.type_params.is_empty()
                && view.params.iter().all(|p| !matches!(p.ty, HirType::Any))
                && !matches!(view.return_type, HirType::Any)
            {
                let args = solver.declared_arguments(tid)?;
                solver.request_instance(InstanceReq::new(tid, &args, None))?;
            }
        }
        solver.run_worklist()?;
    }
    solver.verify_fixed_point()?;
    solver.verify_contracts()?;
    Ok(solver.finish(init_inst))
}

struct Solver<'h, 'a> {
    resource_instances: HashMap<u32, ClassInstId>,
    ctx: &'a Context,
    hir: &'h SourceModule<'h>,
    index: &'a ModuleIndex<'h>,

    decls: &'a Declarations<'a>,
    types: &'a mut TypeTable,
    facts: Facts,
    arrays: ArrayLayouts,
    subscriptions: Subscriptions,
    worklist: Vec<InstanceId>,
    queued: BitSet,
    instances: IdxVec<InstanceId, Instance>,
    witness_instances: Vec<Option<ExportRoot>>,
    instance_index: HashMap<InstanceKey, InstanceId>,
    template_counts: IdxVec<TemplateId, u16>,
    field_locs: Pool<FieldLocSpan, LocId>,
    dispatch_arms: Pool<DispatchArmSpan, DispatchArm>,
    dispatch_arm_spans: Vec<DispatchArmSpan>,
    helpers: IdxVec<HelperId, HelperEntry>,
    helper_index: HashMap<HelperSpec, HelperId>,
    capture_locs: CaptureLocations,
    inst_frame_locals: IdxVec<InstanceId, Vec<Ty>>,
    inst_written: IdxVec<InstanceId, BitSet>,
    inst_sites: IdxVec<InstanceId, InstanceSites>,
    inst_superseded: BitSet,
    collapsed_closures: BitSet,

    boxed_locals: BitSet,
    dict_reads: BitSet,
    self_nested_locals: BitSet,
    class_insts: IdxVec<ClassInstId, ClassInst>,
    class_index: HashMap<(ClassId, ArgumentKey), ClassInstId>,
    type_bindings: IdxVec<InstanceId, Box<[TypeScope]>>,
    result_contracts: std::collections::BTreeSet<ResultContract<'h>>,
    next_func_id: u32,
    deletes_array_elements: bool,
}

pub(crate) fn attach_inst_chain(
    instances: &IdxVec<InstanceId, Instance>,
    index: &ModuleIndex<'_>,
    locations: &crate::hir_compat::Locations<'_>,
    mut err: Diagnostic,
    mut cursor: InstanceId,
) -> Diagnostic {
    for _ in 0..16 {
        let Some(inst) = instances.get(cursor) else {
            break;
        };
        if inst.template == TemplateId::INIT {
            break;
        }
        if hir_compat::has_source_offset(inst.call_byte_offset) {
            let msg = format!(
                "while instantiating '{}'",
                index.view(inst.template).display_name(locations)
            );
            err = match locations.call_span(inst.call_byte_offset) {
                Some(span) => err.with_spanned_note(span, msg),
                None => err.with_note(msg),
            };
        }
        let Some(next) = inst.caller else { break };
        cursor = next;
    }
    err
}

fn bump_count(counts: &mut IdxVec<TemplateId, u16>, template: TemplateId) {
    let count = &mut counts[template];
    *count = count.saturating_add(1);
}

impl<'h, 'a> Solver<'h, 'a> {
    fn new(
        ctx: &'a Context,
        hir: &'h SourceModule<'h>,
        index: &'a ModuleIndex<'h>,

        decls: &'a Declarations<'a>,
        types: &'a mut TypeTable,
    ) -> Self {
        let mut values = index.location_types.clone();
        if values.is_empty() {
            values.push(Ty::VOID);
        }
        Self {
            resource_instances: HashMap::new(),
            ctx,
            hir,
            index,
            decls,
            types,
            facts: Facts::new(values),
            arrays: Default::default(),
            subscriptions: Subscriptions::default(),
            worklist: Vec::new(),
            queued: BitSet::default(),
            instances: IdxVec::default(),
            witness_instances: Vec::with_capacity(hir.exports.functions.len()),
            instance_index: HashMap::with_capacity(64),
            template_counts: index.views.filled(0),
            field_locs: Pool::default(),
            dispatch_arms: Pool::default(),
            dispatch_arm_spans: Vec::new(),
            helpers: IdxVec::default(),
            helper_index: HashMap::with_capacity(16),
            capture_locs: CaptureLocations {
                slots: Pool::default(),
                collapsed: index.locals.filled(None),
            },
            inst_frame_locals: IdxVec::default(),
            inst_written: IdxVec::default(),
            inst_sites: IdxVec::default(),
            inst_superseded: BitSet::default(),
            collapsed_closures: BitSet::default(),

            boxed_locals: BitSet::default(),
            dict_reads: BitSet::default(),
            self_nested_locals: BitSet::default(),
            class_insts: IdxVec::default(),
            class_index: HashMap::with_capacity(16),
            type_bindings: IdxVec::default(),
            result_contracts: Default::default(),
            next_func_id: index.max_func_id.saturating_add(1),
            deletes_array_elements: false,
        }
    }

    fn storage_site(
        &mut self,
        inst: InstanceId,
        expr: &Expr,
        kind: CellKind,
    ) -> Result<(&mut SiteEntry, LocId, Ty)> {
        let index = self
            .index
            .expressions
            .index(expr)
            .ok_or_else(|| Diagnostic::internal("missing storage site for expression"))?;
        let entry = self.inst_sites[inst].entry(index).or_default();
        let loc = match entry.resolution {
            Some(
                SiteResolution::Cell(loc) | SiteResolution::PromiseReaction { result: loc, .. },
            ) => loc,
            Some(SiteResolution::TypedArray(ty))
                if let TyKind::Cell { loc, .. } = self.types.kind(ty) =>
            {
                loc
            }
            _ => self.facts.alloc(Ty::BOTTOM),
        };
        Ok((entry, loc, self.types.cell(loc, kind)))
    }

    fn alloc_expr_cell(&mut self, inst: InstanceId, expr: &Expr, kind: CellKind) -> Result<Ty> {
        let (site, loc, cell) = self.storage_site(inst, expr, kind)?;
        site.resolution = Some(SiteResolution::Cell(loc));
        Ok(cell)
    }

    fn read_loc(&mut self, loc: LocId, reader: InstanceId) -> Ty {
        let (loc, ty) = self.facts.resolve(loc);
        self.subscriptions.read(reader, Fact::Storage(loc));
        ty
    }

    fn write_loc(&mut self, loc: LocId, value: Ty) {
        self.facts.write(self.types, loc, value);
    }

    fn is_non_site_root(&self, inst_id: InstanceId) -> bool {
        let inst = &self.instances[inst_id];
        inst.caller.is_none()
            || matches!(
                self.index.view(inst.template).kind,
                TemplateKind::ClassConstructor
            )
    }

    fn for_each_site_target(
        &self,
        entry: &SiteEntry,
        mut instance: impl FnMut(InstanceId),
        mut helper: impl FnMut(HelperId),
    ) {
        let mut callback = |target| match target {
            CallTarget::Direct { target, .. } => instance(target),
            CallTarget::Dispatch { helper: id, .. } => {
                helper(id);
                if let HelperSpec::Dispatcher { arms, .. } = self.helpers[id].spec {
                    for arm in &self.dispatch_arms[arms] {
                        instance(arm.target);
                    }
                }
            }
            CallTarget::ThrowTypeError => {}
        };
        if let Some(FuncCoercion::Callback { target, .. }) = entry.coercion {
            callback(target);
        }
        let Some(resolution) = entry.resolution else {
            return;
        };
        match resolution {
            SiteResolution::Call { target, .. } => callback(target),
            SiteResolution::ReadMember(target)
            | SiteResolution::WriteMember(target)
            | SiteResolution::Callback { target } => instance(target),
            SiteResolution::UpdateMember { get, set } => {
                get.into_iter().chain(set).for_each(instance);
            }
            SiteResolution::ClassNew(class_inst, _) => {
                let class = &self.class_insts[class_inst];
                class
                    .field_init
                    .into_iter()
                    .chain(class.ctor_inst)
                    .for_each(instance);
            }
            SiteResolution::PromiseNew {
                executor: Some(executor),
                ..
            } => {
                callback(executor.call);
            }
            SiteResolution::PromiseReaction { helper: id, .. } => {
                if let HelperSpec::PromiseReaction(reaction) = self.helpers[id].spec {
                    for (call, _) in reaction.callbacks() {
                        callback(call);
                    }
                }
                helper(id);
            }
            SiteResolution::TimerStart { helper: id } => {
                if let HelperSpec::TimerRunner { cb_call, .. } = self.helpers[id].spec {
                    callback(cb_call);
                }
                helper(id);
            }
            _ => {}
        }
    }

    fn live_instances(&self, mut helper: impl FnMut(HelperId)) -> BitSet {
        let mut live = BitSet::default();
        let mut queue = Vec::new();
        let roots = self
            .instances
            .ids()
            .filter(|&id| self.is_non_site_root(id))
            .chain(self.result_contracts.iter().map(|contract| contract.inst));
        for id in roots {
            let idx = id.index();
            if !self.inst_superseded.contains(idx) && live.insert(idx) {
                queue.push(id);
            }
        }
        let mut head = 0usize;
        while let Some(&caller) = queue.get(head) {
            head += 1;
            for entry in self.inst_sites[caller].values() {
                self.for_each_site_target(
                    entry,
                    |target| {
                        if live.insert(target.index()) {
                            queue.push(target);
                        }
                    },
                    &mut helper,
                );
            }
        }
        live
    }

    fn prune_unreachable(&mut self) {
        let live = self.live_instances(|_| {});
        self.template_counts.as_mut_slice().fill(0);
        for (id, inst) in self.instances.iter_enumerated() {
            let idx = id.index();
            if live.contains(idx) {
                bump_count(&mut self.template_counts, inst.template);
            } else if self.inst_superseded.insert(idx) {
                self.inst_sites[id].clear();
                self.subscriptions.forget(id);
            }
        }
    }

    fn enqueue(&mut self, inst: InstanceId) {
        if !self.inst_superseded.contains(inst.index()) && self.queued.insert(inst.index()) {
            self.worklist.push(inst);
        }
    }

    fn run_worklist(&mut self) -> Result<()> {
        let mut wave = Vec::new();
        let mut waves = 0usize;
        while !self.worklist.is_empty() {
            let wave_limit = 1024usize.max(self.instances.len().saturating_mul(8));
            if waves >= wave_limit {
                return Err(Diagnostic::internal(
                    "type inference solver did not reach a fixed point within the wave limit",
                ));
            }
            waves += 1;
            std::mem::swap(&mut wave, &mut self.worklist);
            for inst_id in wave.drain(..) {
                self.queued.remove(inst_id.index());
                if !self.inst_superseded.contains(inst_id.index()) {
                    self.evaluate_instance(inst_id)?;
                }
            }
            for &fact in &self.facts.dirty {
                for &reader in self.subscriptions.readers(fact) {
                    if !self.inst_superseded.contains(reader.index())
                        && self.queued.insert(reader.index())
                    {
                        self.worklist.push(reader);
                    }
                }
            }
            self.facts.dirty.clear();
        }
        Ok(())
    }

    fn verify_fixed_point(&mut self) -> Result<()> {
        if !cfg!(any(test, debug_assertions)) {
            return Ok(());
        }
        let live = self.live_instances(|_| {});
        let live_ids: Vec<InstanceId> = self
            .instances
            .iter_enumerated()
            .filter_map(|(id, _)| live.contains(id.index()).then_some(id))
            .collect();
        let prev_inst_count = self.instances.len();
        let prev_loc_count = self.facts.len();
        let prev_frames = self.inst_frame_locals.clone();
        let prev_collapsed = self.collapsed_closures.clone();
        self.facts.dirty.clear();
        for id in live_ids {
            self.evaluate_instance(id)?;
        }
        if self.collapsed_closures != prev_collapsed {
            return Err(Diagnostic::internal(
                "fixed-point check collapsed a new closure",
            ));
        }
        if self.instances.len() != prev_inst_count {
            return Err(Diagnostic::internal(
                "fixed-point check spawned a new instance",
            ));
        }
        if self.facts.len() != prev_loc_count {
            return Err(Diagnostic::internal(
                "fixed-point check allocated a storage location",
            ));
        }
        if self.inst_frame_locals != prev_frames {
            return Err(Diagnostic::internal(
                "fixed-point check mutated a local frame slot",
            ));
        }
        if !self.facts.dirty.is_empty() {
            return Err(Diagnostic::internal(
                "fixed-point check changed a solver fact",
            ));
        }
        Ok(())
    }

    pub(super) fn intern_dispatch_arms(&mut self, arms: &[DispatchArm]) -> DispatchArmSpan {
        for &span in &self.dispatch_arm_spans {
            if &self.dispatch_arms[span] == arms {
                return span;
            }
        }
        let span = self.dispatch_arms.extend_from_slice(arms);
        self.dispatch_arm_spans.push(span);
        span
    }

    pub(super) fn intern_helper(&mut self, spec: HelperSpec, byte_offset: u32) -> HelperId {
        if let Some(&id) = self.helper_index.get(&spec) {
            let offset = &mut self.helpers[id].byte_offset;
            if !hir_compat::has_source_offset(*offset) {
                *offset = byte_offset;
            }
            return id;
        }
        let id = self.helpers.push(HelperEntry { spec, byte_offset });
        self.helper_index.insert(spec, id);
        id
    }

    fn verify_contracts(&mut self) -> Result<()> {
        for contract in &self.result_contracts {
            let inst = &self.instances[contract.inst];
            let raw_actual = self.facts[inst.return_loc];
            let actual = self
                .types
                .promise_argument(raw_actual, |loc| self.facts[loc]);
            if !self.types.accepts(contract.expected, actual) {
                return Err(self.hir.locations.annotate_call(
                    diag!(
                        codes::TYPE_MISMATCH,
                        "callback result does not match the declared result of parameter '{}'",
                        contract.param_name
                    ),
                    contract.byte_offset,
                ));
            }
        }
        Ok(())
    }
}
