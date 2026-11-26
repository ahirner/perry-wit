//! Worklist monomorphization and storage-join solver over dense indices.
use crate::source::SourceModule;
mod array_layouts;
mod callbacks;
mod calls;
mod escape;
mod eval;
mod expr;
mod instances;
mod ir;
mod locations;
mod members;
mod methods;
mod promises;
use super::bitset::BitSet;
use super::boundary::{LocalFlags, ModuleIndex, TemplateKind};
use super::builtins::{self, MethodResultArgs};
use super::types::{
    CaptureOwner, CellKind, ClassId, ClassInstId, DispatchArmSpan, FieldLocSpan, FuncIdentity,
    HelperId, Idx, IdxVec, InstanceId, LocId, LocalIdx, ObjectProp, ParamLocSpan, Pool, TemplateId,
    Ty, TyKind, TypeParamSpan, TypeTable,
};
use crate::{
    Absence, hir_compat,
    narrowing::{DefinedGuard, Guards},
    shapes::{self, Declarations},
};
use array_layouts::{ArgumentKey, ArrayLayouts};
use eval::first_type_var_name;
pub(crate) use ir::*;
use locations::{Locations, Subscriptions};
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

struct ResultContract {
    inst: InstanceId,
    expected: Ty,
    param_name: String,
    byte_offset: u32,
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
    slots: IdxVec<InstanceId, Vec<InstanceLocalSlot>>,
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
        if let Some(loc) = index.locals[local].module_loc() {
            return Some(loc);
        }
        loop {
            match owner {
                CaptureOwner::Instance(id) => {
                    if let Some(slot) = self.slots[id].iter().find(|s| s.local == local) {
                        return Some(slot.loc);
                    }
                    owner = instances[id].capture_owner;
                }
                CaptureOwner::Template(_) => return self.collapsed[local],
                CaptureOwner::None => return None,
            }
        }
    }
}

struct InstanceLocalSlot {
    local: LocalIdx,
    loc: LocId,
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
    locations: Locations,
    arrays: ArrayLayouts,
    subscriptions: Subscriptions,
    worklist: Vec<InstanceId>,
    queued: BitSet,
    instances: IdxVec<InstanceId, Instance>,
    witness_instances: Vec<Option<ExportRoot>>,
    instance_index: HashMap<InstanceKey, InstanceId>,
    template_counts: IdxVec<TemplateId, u16>,
    param_locs: Pool<ParamLocSpan, LocId>,
    field_locs: Pool<FieldLocSpan, LocId>,
    dispatch_arms: Pool<DispatchArmSpan, (FuncIdentity, InstanceId)>,
    dispatch_arm_spans: Vec<DispatchArmSpan>,
    helpers: IdxVec<HelperId, HelperEntry>,
    helper_index: HashMap<HelperSpec, HelperId>,
    capture_locs: CaptureLocations,
    inst_frame_locals: IdxVec<InstanceId, Vec<Ty>>,
    inst_written: IdxVec<InstanceId, BitSet>,
    inst_sites: IdxVec<InstanceId, Vec<SiteEntry>>,
    inst_superseded: BitSet,
    escaping_env_templates: BitSet,
    template_escape_locs: IdxVec<TemplateId, LocId>,
    collapsed_closures: BitSet,

    boxed_locals: BitSet,
    dict_reads: BitSet,
    self_nested_locals: BitSet,
    static_field_locs: Vec<(ClassId, usize, LocId)>,
    class_insts: IdxVec<ClassInstId, ClassInst>,
    class_index: HashMap<(ClassId, ArgumentKey), ClassInstId>,
    type_param_pool: Pool<TypeParamSpan, shapes::TypeName>,
    template_type_params: IdxVec<TemplateId, TypeParamSpan>,
    class_type_params: IdxVec<ClassId, TypeParamSpan>,
    result_contracts: Vec<ResultContract>,
    contract_insts: BitSet,
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
        let template_escape_locs = index.views.map(|_| values.push(Ty::BOTTOM));
        let static_field_locs = index
            .classes
            .iter_enumerated()
            .flat_map(|(cid, c)| {
                c.static_field_locs
                    .iter()
                    .enumerate()
                    .map(move |(f_idx, &(_, loc))| (cid, f_idx, loc))
            })
            .collect();
        let mut type_param_pool = Pool::default();
        let mut tp_buf = Vec::new();
        let mut intern_tps = |tps: &[perry_hir::types::TypeParam]| -> TypeParamSpan {
            if tps.is_empty() {
                return TypeParamSpan::default();
            }
            tp_buf.clear();
            tp_buf.extend(
                tps.iter()
                    .map(|tp| shapes::TypeName::new(ctx, tp.name.clone())),
            );
            type_param_pool.extend_from_slice(&tp_buf)
        };
        let template_type_params = index.views.map(|v| intern_tps(v.type_params));
        let class_type_params = index
            .classes
            .map(|class| intern_tps(&class.declaration.type_params));
        Self {
            resource_instances: HashMap::new(),
            ctx,
            hir,
            index,
            decls,
            types,
            locations: Locations::new(values),
            arrays: Default::default(),
            subscriptions: Subscriptions::default(),
            worklist: Vec::new(),
            queued: BitSet::default(),
            instances: IdxVec::default(),
            witness_instances: Vec::with_capacity(hir.exports.functions.len()),
            instance_index: HashMap::with_capacity(64),
            template_counts: index.views.filled(0),
            param_locs: Pool::default(),
            field_locs: Pool::default(),
            dispatch_arms: Pool::default(),
            dispatch_arm_spans: Vec::new(),
            helpers: IdxVec::default(),
            helper_index: HashMap::with_capacity(16),
            capture_locs: CaptureLocations {
                slots: IdxVec::default(),
                collapsed: index.locals.filled(None),
            },
            inst_frame_locals: IdxVec::default(),
            inst_written: IdxVec::default(),
            inst_sites: IdxVec::default(),
            inst_superseded: BitSet::default(),
            escaping_env_templates: BitSet::default(),
            template_escape_locs,
            collapsed_closures: BitSet::default(),

            boxed_locals: BitSet::default(),
            dict_reads: BitSet::default(),
            self_nested_locals: BitSet::default(),
            static_field_locs,
            class_insts: IdxVec::default(),
            class_index: HashMap::with_capacity(16),
            type_param_pool,
            template_type_params,
            class_type_params,
            result_contracts: Vec::new(),
            contract_insts: BitSet::default(),
            next_func_id: index.max_func_id.saturating_add(1),
            deletes_array_elements: false,
        }
    }

    fn expr_cell_loc(&mut self, inst: InstanceId, expr: &Expr) -> Result<LocId> {
        let entry = self
            .index
            .site_of(self.instances[inst].template, expr)
            .and_then(|site| self.inst_sites[inst].get_mut(site))
            .ok_or_else(|| Diagnostic::internal("missing storage site for expression"))?;
        match entry.resolution {
            Some(
                SiteResolution::Cell(loc) | SiteResolution::PromiseReaction { result: loc, .. },
            ) => Ok(loc),
            Some(SiteResolution::TypedArray(ty))
                if let TyKind::Cell { loc, .. } = self.types.kind(ty) =>
            {
                Ok(loc)
            }
            _ => {
                let loc = self.locations.alloc(Ty::BOTTOM);
                self.record_site(inst, expr, SiteResolution::Cell(loc));
                Ok(loc)
            }
        }
    }

    fn alloc_expr_cell(&mut self, inst: InstanceId, expr: &Expr, kind: CellKind) -> Result<Ty> {
        let loc = self.expr_cell_loc(inst, expr)?;
        Ok(self.types.cell(loc, kind))
    }

    fn read_loc(&mut self, loc: LocId, reader: InstanceId) -> Ty {
        let (loc, ty) = self.locations.resolve(loc);
        self.subscriptions.read(reader, loc);
        ty
    }

    fn write_loc(&mut self, loc: LocId, value: Ty) {
        self.locations.write(self.types, loc, value);
    }

    fn is_non_site_root(&self, inst_id: InstanceId) -> bool {
        let inst = &self.instances[inst_id];
        inst.caller.is_none()
            || matches!(
                self.index.view(inst.template).kind,
                TemplateKind::ClassConstructor
                    | TemplateKind::ClassGetter(_)
                    | TemplateKind::ClassSetter(_)
            )
            || self.contract_insts.contains(inst_id.index())
    }

    fn for_each_callback_target(&self, cb: CallTarget, mut f: impl FnMut(InstanceId)) {
        match cb {
            CallTarget::Direct { target, .. } => f(target),
            CallTarget::Dispatch { helper, .. } => {
                if let HelperSpec::Dispatcher { arms, .. } = self.helpers[helper].spec {
                    for &(_, target) in &self.dispatch_arms[arms] {
                        f(target);
                    }
                }
            }
            CallTarget::ThrowTypeError => {}
        }
    }

    fn for_each_site_target(&self, res: &SiteResolution, mut f: impl FnMut(InstanceId)) {
        match *res {
            SiteResolution::Call { target, .. } => self.for_each_callback_target(target, f),
            SiteResolution::MemberCall(MemberTarget::Class(target))
            | SiteResolution::Callback { target } => f(target),
            SiteResolution::ClassNew(class_inst) => {
                let instance = &self.class_insts[class_inst];
                for target in instance.field_init.into_iter().chain(instance.ctor_inst) {
                    f(target);
                }
            }
            SiteResolution::PromiseNew {
                executor: Some(executor),
                ..
            } => {
                self.for_each_callback_target(executor.call, f);
            }
            SiteResolution::PromiseReaction { helper, .. } => {
                if let HelperSpec::PromiseReaction {
                    on_fulfilled,
                    on_rejected,
                    ..
                } = self.helpers[helper].spec
                {
                    if let Some((cb, _)) = on_fulfilled {
                        self.for_each_callback_target(cb, &mut f);
                    }
                    if let Some((cb, _)) = on_rejected {
                        self.for_each_callback_target(cb, &mut f);
                    }
                }
            }
            SiteResolution::TimerStart { helper, .. } => {
                if let HelperSpec::TimerRunner { cb_call, .. } = self.helpers[helper].spec {
                    self.for_each_callback_target(cb_call, f);
                }
            }
            _ => {}
        }
    }

    fn compute_live_instances(&self) -> BitSet {
        let mut live = BitSet::default();
        let mut queue = Vec::new();
        for (id, _) in self.instances.iter_enumerated() {
            let idx = id.index();
            if !self.inst_superseded.contains(idx) && self.is_non_site_root(id) && live.insert(idx)
            {
                queue.push(id);
            }
        }
        let mut head = 0usize;
        while let Some(&caller) = queue.get(head) {
            head += 1;
            for entry in &self.inst_sites[caller] {
                if let Some(res) = &entry.resolution {
                    self.for_each_site_target(res, |t| {
                        if live.insert(t.index()) {
                            queue.push(t);
                        }
                    });
                }
                if let Some(FuncCoercion::Callback { target, .. }) = entry.coercion {
                    self.for_each_callback_target(target, |t| {
                        if live.insert(t.index()) {
                            queue.push(t);
                        }
                    });
                }
            }
        }
        live
    }

    fn prune_unreachable(&mut self) {
        let live = self.compute_live_instances();
        self.template_counts.as_mut_slice().fill(0);
        for (id, inst) in self.instances.iter_enumerated() {
            let idx = id.index();
            if live.contains(idx) {
                bump_count(&mut self.template_counts, inst.template);
            } else if self.inst_superseded.insert(idx) {
                self.inst_sites[id].fill(SiteEntry::default());
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
            for &loc in &self.locations.dirty {
                for &reader in self.subscriptions.readers(loc) {
                    if !self.inst_superseded.contains(reader.index())
                        && self.queued.insert(reader.index())
                    {
                        self.worklist.push(reader);
                    }
                }
            }
            self.locations.dirty.clear();
        }
        Ok(())
    }

    fn verify_fixed_point(&mut self) -> Result<()> {
        if !cfg!(any(test, debug_assertions)) {
            return Ok(());
        }
        let live = self.compute_live_instances();
        let live_ids: Vec<InstanceId> = self
            .instances
            .iter_enumerated()
            .filter_map(|(id, _)| live.contains(id.index()).then_some(id))
            .collect();
        let prev_inst_count = self.instances.len();
        let prev_loc_count = self.locations.len();
        let prev_frames = self.inst_frame_locals.clone();
        let prev_escaping = self.escaping_env_templates.clone();
        let prev_collapsed = self.collapsed_closures.clone();
        self.locations.dirty.clear();
        for id in live_ids {
            self.evaluate_instance(id)?;
        }
        if self.escaping_env_templates != prev_escaping {
            return Err(Diagnostic::internal(
                "fixed-point check marked a new escaping closure",
            ));
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
        if self.locations.len() != prev_loc_count {
            return Err(Diagnostic::internal(
                "fixed-point check allocated a storage location",
            ));
        }
        if self.inst_frame_locals != prev_frames {
            return Err(Diagnostic::internal(
                "fixed-point check mutated a local frame slot",
            ));
        }
        if !self.locations.dirty.is_empty() {
            return Err(Diagnostic::internal(
                "fixed-point check dirtied a storage location",
            ));
        }
        Ok(())
    }

    pub(super) fn intern_dispatch_arms(
        &mut self,
        arms: &[(FuncIdentity, InstanceId)],
    ) -> DispatchArmSpan {
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
        let contracts = std::mem::take(&mut self.result_contracts);
        for contract in contracts {
            let inst = &self.instances[contract.inst];
            let raw_actual = self.locations[inst.return_loc];
            let actual = match self.types.kind(raw_actual) {
                TyKind::Cell {
                    loc,
                    kind: CellKind::Promise,
                } => {
                    let inner = self.locations[loc];
                    self.types
                        .promise(if inner == Ty::BOTTOM { Ty::VOID } else { inner })
                }
                _ => raw_actual,
            };
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
