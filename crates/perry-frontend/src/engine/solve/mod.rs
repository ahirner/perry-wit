//! Worklist monomorphization and storage-join solver over dense indices.
mod callbacks;
mod calls;
mod escape;
mod eval;
mod expr;
mod instances;
mod ir;
mod members;
mod methods;
mod promises;
use super::bitset::BitSet;
use super::boundary::{ClassInfo, ModuleIndex, SourceFacts, TemplateKind};
use super::builtins::{self, MethodResultArgs};
use super::types::{
    CaptureOwner, CellKind, ClassId, ClassInstId, DispatchArmSpan, FieldLocSpan, FuncIdentity,
    HelperId, IdxVec, InstanceId, LocId, LocalIdx, ObjectProp, ParamLocSpan, Pool, TemplateId, Ty,
    TyKind, TypeTable,
};
use crate::{
    Absence,
    hir::Foreign,
    hir_compat::{self, Metadata},
    narrowing::{DefinedGuard, Guards},
    shapes::{self, Declarations},
};
use eval::first_type_var_name;
pub(crate) use ir::*;
use perry_hir::{
    ir::{CompareOp, Expr, LogicalOp, Module as HirModule, Stmt},
    types::{LocalId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};
use pliron::context::Context;
use std::collections::HashMap;

const INSTANCE_BUDGET: u16 = 256;

type InstanceKey = (
    TemplateId,
    Option<Receiver>,
    CaptureOwner,
    Ty,
    Ty,
    Option<Ty>,
);

struct ResultContract {
    inst: InstanceId,
    expected: Ty,
    param_name: String,
    byte_offset: u32,
}

struct InstanceReq<'a> {
    witness: Option<&'a hir_compat::Witness<perry_hir::types::FuncId>>,
    template: TemplateId,
    receiver: Option<Receiver>,
    capture_owner: CaptureOwner,
    args: &'a [Ty],
    explicit_type_args: &'a [Ty],
    call_byte_offset: u32,
    caller: Option<InstanceId>,
    is_inline_callback: bool,
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
            is_inline_callback: false,
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
    fn inline_cb(mut self) -> Self {
        self.is_inline_callback = true;
        self
    }
}

struct EvalState<'h> {
    inst: InstanceId,
    receiver: Option<Receiver>,
    returned: Ty,
    has_return_value: bool,
    guards: Guards<'h>,
    iteration: hir_compat::Iteration,
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
            iteration: hir_compat::Iteration::default(),
            local_changed: false,
            current_span: None,
        }
    }
}

struct InstanceLocalSlot {
    local: LocalIdx,
    loc: LocId,
}

pub(crate) fn solve<'h>(
    ctx: &Context,
    hir: &'h HirModule,
    index: &ModuleIndex<'h>,
    metadata: &Metadata<'_>,
    facts: &SourceFacts,
    foreign: &HashMap<String, Foreign>,
    types: &mut TypeTable,
) -> Result<Solved> {
    let decls = Declarations::new(
        &hir.interfaces,
        &hir.type_aliases,
        &hir.imports,
        &hir.classes,
        foreign,
    );
    let mut solver = Solver::new(ctx, hir, index, metadata, facts, &decls, types);
    let init_inst = solver.seed_roots()?;
    solver.run_worklist(init_inst)?;
    solver.verify_fixed_point()?;
    solver.verify_contracts()?;
    Ok(solver.finish(init_inst))
}

struct Solver<'h, 'a> {
    ctx: &'a Context,
    hir: &'h HirModule,
    index: &'a ModuleIndex<'h>,
    metadata: &'a Metadata<'a>,
    facts: &'a SourceFacts,
    decls: &'a Declarations<'a>,
    types: &'a mut TypeTable,
    locations: IdxVec<LocId, Ty>,
    /// Locations whose type an annotation fixes: writes are checked by lowering, never joined.
    declared_locs: BitSet,
    dirty_locs: BitSet,
    inst_reads: IdxVec<InstanceId, Vec<LocId>>,
    reader_ends: IdxVec<LocId, usize>,
    reader_instances: Vec<InstanceId>,
    csr_dirty: bool,
    evaluating_inst: Option<InstanceId>,
    current_reads: BitSet,
    current_read_list: Vec<LocId>,
    worklist: Vec<InstanceId>,
    worklist_head: usize,
    queued: BitSet,
    instances: IdxVec<InstanceId, Instance>,
    witness_instances: Vec<Option<InstanceId>>,
    instance_index: HashMap<InstanceKey, InstanceId>,
    template_counts: IdxVec<TemplateId, u16>,
    param_locs: Pool<ParamLocSpan, LocId>,
    field_locs: Pool<FieldLocSpan, LocId>,
    dispatch_arms: Pool<DispatchArmSpan, (FuncIdentity, InstanceId)>,
    dispatch_arm_spans: Vec<DispatchArmSpan>,
    helpers: IdxVec<HelperId, HelperSpec>,
    helper_index: HashMap<HelperSpec, HelperId>,
    helper_byte_offsets: IdxVec<HelperId, u32>,
    inst_locals: IdxVec<InstanceId, Vec<InstanceLocalSlot>>,
    inst_frame_locals: IdxVec<InstanceId, Vec<Ty>>,
    inst_written: IdxVec<InstanceId, BitSet>,
    inst_captures: IdxVec<InstanceId, Vec<LocalId>>,
    inst_sites: IdxVec<InstanceId, Vec<SiteEntry>>,
    inst_superseded: BitSet,
    escaping_env_templates: BitSet,
    template_escape_locs: IdxVec<TemplateId, LocId>,
    collapsed_closures: BitSet,
    collapsed_local_locs: IdxVec<LocalIdx, Option<LocId>>,
    boxed_locals: BitSet,
    dict_reads: BitSet,
    self_nested_locs: BitSet,
    self_nested_locals: BitSet,
    static_field_locs: Vec<(ClassId, usize, LocId)>,
    class_insts: IdxVec<ClassInstId, ClassInst>,
    class_index: HashMap<(ClassId, Ty), ClassInstId>,
    template_type_params: IdxVec<TemplateId, Vec<shapes::TypeId>>,
    class_type_params: IdxVec<ClassId, Vec<shapes::TypeId>>,
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
                index.view(inst.template).display_name()
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
        hir: &'h HirModule,
        index: &'a ModuleIndex<'h>,
        metadata: &'a Metadata<'a>,
        facts: &'a SourceFacts,
        decls: &'a Declarations<'a>,
        types: &'a mut TypeTable,
    ) -> Self {
        let mut locations = index.location_types.clone();
        if locations.is_empty() {
            locations.push(Ty::VOID);
        }
        let template_escape_locs = index.views.map(|_| locations.push(Ty::BOTTOM));
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
        let intern_tps = |tps: &[perry_hir::types::TypeParam]| -> Vec<shapes::TypeId> {
            tps.iter()
                .map(|tp| shapes::TypeId::from_owned(ctx, HirType::Named(tp.name.clone())))
                .collect()
        };
        let template_type_params = index.views.map(|v| intern_tps(v.type_params));
        let class_type_params = index.classes.map(|c| {
            hir.classes
                .get(c.hir_idx)
                .map_or_else(Vec::new, |hc| intern_tps(&hc.type_params))
        });
        Self {
            ctx,
            hir,
            index,
            metadata,
            facts,
            decls,
            types,
            reader_ends: locations.filled(0),
            locations,
            declared_locs: BitSet::default(),
            dirty_locs: BitSet::default(),
            inst_reads: IdxVec::default(),
            reader_instances: Vec::new(),
            csr_dirty: true,
            evaluating_inst: None,
            current_reads: BitSet::default(),
            current_read_list: Vec::new(),
            worklist: Vec::new(),
            worklist_head: 0,
            queued: BitSet::default(),
            instances: IdxVec::default(),
            witness_instances: Vec::with_capacity(metadata.witnesses.len()),
            instance_index: HashMap::with_capacity(64),
            template_counts: index.views.filled(0),
            param_locs: Pool::default(),
            field_locs: Pool::default(),
            dispatch_arms: Pool::default(),
            dispatch_arm_spans: Vec::new(),
            helpers: IdxVec::default(),
            helper_index: HashMap::with_capacity(16),
            helper_byte_offsets: IdxVec::default(),
            inst_locals: IdxVec::default(),
            inst_frame_locals: IdxVec::default(),
            inst_written: IdxVec::default(),
            inst_captures: IdxVec::default(),
            inst_sites: IdxVec::default(),
            inst_superseded: BitSet::default(),
            escaping_env_templates: BitSet::default(),
            template_escape_locs,
            collapsed_closures: BitSet::default(),
            collapsed_local_locs: index.local_ids.filled(None),
            boxed_locals: BitSet::default(),
            dict_reads: BitSet::default(),
            self_nested_locs: BitSet::default(),
            self_nested_locals: BitSet::default(),
            static_field_locs,
            class_insts: IdxVec::default(),
            class_index: HashMap::with_capacity(16),
            template_type_params,
            class_type_params,
            result_contracts: Vec::new(),
            contract_insts: BitSet::default(),
            next_func_id: index.max_func_id.saturating_add(1),
            deletes_array_elements: false,
        }
    }

    fn alloc_loc(&mut self, initial: Ty) -> LocId {
        self.reader_ends.push(self.reader_instances.len());
        self.locations.push(initial)
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
            _ => {
                let loc = self.alloc_loc(Ty::BOTTOM);
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
        if self.evaluating_inst == Some(reader) {
            if self.current_reads.insert(loc.index()) {
                self.current_read_list.push(loc);
            }
        } else {
            let reads = &mut self.inst_reads[reader];
            if let Err(pos) = reads.binary_search(&loc) {
                reads.insert(pos, loc);
                self.csr_dirty = true;
            }
        }
        self.locations[loc]
    }

    pub(super) fn commit_reads(&mut self, inst_id: InstanceId) {
        self.current_read_list.sort_unstable();
        let prev = &mut self.inst_reads[inst_id];
        if *prev != self.current_read_list {
            prev.clear();
            prev.extend_from_slice(&self.current_read_list);
            self.csr_dirty = true;
        }
    }

    fn write_loc(&mut self, loc: LocId, value: Ty) -> bool {
        if self.declared_locs.contains(loc.index()) {
            return false;
        }
        let Some(&old) = self.locations.get(loc) else {
            return false;
        };
        let (next, self_nested) = self.types.join_bounded(old, value);
        if self_nested {
            self.self_nested_locs.insert(loc.index());
        }
        if next == old {
            return false;
        }
        self.locations[loc] = next;
        self.dirty_locs.insert(loc.index());
        true
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

    fn for_each_callback_target(&self, cb: CallbackCall, mut f: impl FnMut(InstanceId)) {
        match cb {
            CallbackCall::Direct { target, .. } => f(target),
            CallbackCall::Dispatch { helper, .. } => {
                if let HelperSpec::Dispatcher { arms, .. } = self.helpers[helper] {
                    for &(_, target) in &self.dispatch_arms[arms] {
                        f(target);
                    }
                }
            }
            CallbackCall::ThrowTypeError(_) => {}
        }
    }

    fn for_each_site_target(&self, res: &SiteResolution, mut f: impl FnMut(InstanceId)) {
        match *res {
            SiteResolution::DirectCall { target, .. }
            | SiteResolution::MethodCall { target, .. }
            | SiteResolution::Callback { target, .. } => f(target),
            SiteResolution::DispatchCall { helper, .. } => {
                if let HelperSpec::Dispatcher { arms, .. } = self.helpers[helper] {
                    for &(_, target) in &self.dispatch_arms[arms] {
                        f(target);
                    }
                }
            }
            SiteResolution::ClassNew { class_inst, .. } => {
                let instance = &self.class_insts[class_inst];
                for target in instance.field_init.into_iter().chain(instance.ctor_inst) {
                    f(target);
                }
            }
            SiteResolution::PromiseNew {
                executor: Some((executor, _, _)),
                ..
            } => {
                self.for_each_callback_target(executor, f);
            }
            SiteResolution::PromiseReaction { helper, .. } => {
                if let Some(HelperSpec::PromiseReaction {
                    on_fulfilled,
                    on_rejected,
                    ..
                }) = self.helpers.get(helper)
                {
                    if let Some((cb, _)) = *on_fulfilled {
                        self.for_each_callback_target(cb, &mut f);
                    }
                    if let Some((cb, _)) = *on_rejected {
                        self.for_each_callback_target(cb, &mut f);
                    }
                }
            }
            SiteResolution::TimerStart { helper, .. } => {
                if let Some(HelperSpec::TimerRunner { cb_call, .. }) = self.helpers.get(helper) {
                    self.for_each_callback_target(*cb_call, f);
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
                match entry.coercion {
                    Some(FuncCoercion::EnvCallback { target, .. }) => {
                        if live.insert(target.index()) {
                            queue.push(target);
                        }
                    }
                    Some(FuncCoercion::DispatchCallback { arms, .. }) => {
                        for &(_, target) in &self.dispatch_arms[arms] {
                            if live.insert(target.index()) {
                                queue.push(target);
                            }
                        }
                    }
                    _ => {}
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
                let reads = &mut self.inst_reads[id];
                if !reads.is_empty() {
                    reads.clear();
                    self.csr_dirty = true;
                }
            }
        }
    }

    fn enqueue(&mut self, inst: InstanceId) {
        if !self.inst_superseded.contains(inst.index()) && self.queued.insert(inst.index()) {
            self.worklist.push(inst);
        }
    }

    fn rebuild_csr(&mut self, init: InstanceId) -> Result<()> {
        self.reader_ends.as_mut_slice().fill(0);
        for reads in self.inst_reads.iter() {
            for &loc in reads {
                self.reader_ends[loc] += 1;
            }
        }
        let mut total = 0;
        for count in self.reader_ends.as_mut_slice() {
            total += std::mem::replace(count, total);
        }
        self.reader_instances.resize(total, init);
        for (reader, reads) in self.inst_reads.iter_enumerated() {
            for &loc in reads {
                let end = &mut self.reader_ends[loc];
                let slot = self.reader_instances.get_mut(*end).ok_or_else(|| {
                    Diagnostic::internal("location reader exceeds the allocated range")
                })?;
                *slot = reader;
                *end += 1;
            }
        }
        self.csr_dirty = false;
        Ok(())
    }

    fn run_worklist(&mut self, init: InstanceId) -> Result<()> {
        let mut waves = 0usize;
        loop {
            let wave_limit = 1024usize.max(self.instances.len().saturating_mul(8));
            if waves >= wave_limit {
                return Err(Diagnostic::internal(
                    "type inference solver did not reach a fixed point within the wave limit",
                ));
            }
            waves += 1;
            let wave_end = self.worklist.len();
            while self.worklist_head < wave_end {
                let Some(&inst_id) = self.worklist.get(self.worklist_head) else {
                    break;
                };
                self.worklist_head += 1;
                self.queued.remove(inst_id.index());
                if !self.inst_superseded.contains(inst_id.index()) {
                    self.evaluate_instance(inst_id)?;
                }
            }
            if self.dirty_locs.is_empty() && self.worklist_head >= self.worklist.len() {
                return Ok(());
            }
            if self.worklist_head > 0 {
                self.worklist.drain(..self.worklist_head);
                self.worklist_head = 0;
            }
            if !self.dirty_locs.is_empty() {
                if self.csr_dirty {
                    self.rebuild_csr(init)?;
                }
                for loc in self.dirty_locs.iter() {
                    let range = match self.reader_ends.as_slice().get(..=loc) {
                        Some([.., start, end]) => *start..*end,
                        Some([end]) => 0..*end,
                        _ => {
                            return Err(Diagnostic::internal(
                                "missing reader offset for dirty location",
                            ));
                        }
                    };
                    let readers = self.reader_instances.get(range).ok_or_else(|| {
                        Diagnostic::internal("invalid reader range for dirty location")
                    })?;
                    for &r in readers {
                        if !self.inst_superseded.contains(r.index())
                            && self.queued.insert(r.index())
                        {
                            self.worklist.push(r);
                        }
                    }
                }
                self.dirty_locs.clear();
            }
            if self.worklist.is_empty() {
                return Ok(());
            }
        }
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
        let prev_locs = self.locations.as_slice().to_vec();
        let prev_frames = self.inst_frame_locals.clone();
        let prev_escaping = self.escaping_env_templates.clone();
        let prev_collapsed = self.collapsed_closures.clone();
        self.dirty_locs.clear();
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
        if self.locations.as_slice() != prev_locs.as_slice() {
            return Err(Diagnostic::internal(
                "fixed-point check mutated a storage location",
            ));
        }
        if self.inst_frame_locals != prev_frames {
            return Err(Diagnostic::internal(
                "fixed-point check mutated a local frame slot",
            ));
        }
        if !self.dirty_locs.is_empty() {
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
            let offset = &mut self.helper_byte_offsets[id];
            if !hir_compat::has_source_offset(*offset) {
                *offset = byte_offset;
            }
            return id;
        }
        let id = self.helpers.push(spec);
        self.helper_byte_offsets.push(byte_offset);
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
            if !self.types.accepts(self.ctx, contract.expected, actual) {
                return Err(self.metadata.locations.annotate_call(
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
