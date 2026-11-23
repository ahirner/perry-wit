//! Worklist monomorphization and storage-join solver over dense indices.
mod callbacks;
mod calls;
mod eval;
mod expr;
mod instances;
mod promises;
use super::bitset::BitSet;
use super::boundary::{ClassInfo, ModuleIndex, SourceFacts, TemplateKind};
use super::builtins::{self, MethodResultArgs};
use super::types::{
    CaptureOwner, CellKind, ClassId, ClassInstId, FuncIdentity, HelperId, IdxVec, InstanceId,
    LocId, LocalIdx, ObjectProp, TemplateId, Ty, TyKind, TySpan, TypeTable,
};
use crate::{
    Absence,
    hir::Foreign,
    hir_compat::{self, Metadata},
    narrowing::{DefinedGuard, Guards},
    shapes::{self, Declarations},
};
use eval::first_type_var_name;
use perry_hir::{
    ir::{CompareOp, Expr, LogicalOp, Module as HirModule, Stmt},
    types::{LocalId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};
use pliron::context::Context;
use std::collections::HashMap;

const INSTANCE_BUDGET: u16 = 256;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Receiver {
    Class(ClassInstId),
    Object(Ty),
}

impl Receiver {
    pub(crate) fn class(self) -> Option<ClassInstId> {
        match self {
            Self::Class(id) => Some(id),
            Self::Object(_) => None,
        }
    }
}

pub(crate) struct Instance {
    pub(crate) template: TemplateId,
    pub(crate) receiver: Option<Receiver>,
    pub(crate) capture_owner: CaptureOwner,
    pub(crate) args: Ty,
    pub(crate) explicit_type_args: Ty,
    pub(crate) return_loc: LocId,
    pub(crate) has_explicit_return: bool,
    pub(crate) param_locs: TySpan,
    pub(crate) caller: Option<InstanceId>,
    pub(crate) call_byte_offset: u32,
    pub(crate) is_inline_callback: bool,
    pub(crate) captures_non_global: bool,
    pub(crate) emitted_func_id: u32,
    pub(crate) emitted_name: String,
    pub(crate) emitted_method_name: String,
}

pub(crate) struct ClassInst {
    pub(crate) class: ClassId,
    pub(crate) args: Ty,
    pub(crate) field_locs: TySpan,
    pub(crate) ctor_inst: Option<InstanceId>,
    pub(crate) emitted_name: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum ReactionKind {
    Then,
    Catch,
    Finally,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum CallbackCall {
    Direct {
        target: InstanceId,
        pass_env: bool,
        pass_receiver: bool,
    },
    Dispatch {
        helper: HelperId,
        needs_recv: bool,
    },
    ThrowTypeError(HelperId),
}

impl CallbackCall {
    pub(crate) fn needs_value(self) -> bool {
        matches!(
            self,
            Self::Direct { pass_env: true, .. } | Self::Dispatch { .. }
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum HelperSpec {
    Dispatcher {
        callee_ty: Ty,
        arms: TySpan,
        arg_tys: Ty,
        recv_ty: Option<Ty>,
    },
    Widener {
        from: Ty,
        to: Ty,
    },
    Comparer {
        left_ty: Ty,
        right_ty: Ty,
    },
    TypeOfFn(Ty),
    ThrowTypeError,
    PromiseReaction {
        kind: ReactionKind,
        promise_ty: Ty,
        inner_ty: Ty,
        on_fulfilled: Option<(CallbackCall, Ty)>,
        on_rejected: Option<(CallbackCall, Ty)>,
        ret_ty: Ty,
    },
    TimerRunner {
        repeating: bool,
        is_immediate: bool,
        cb_call: CallbackCall,
        cb_ty: Ty,
        extra_arg_tys: Ty,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FuncCoercion {
    Zero,
    Tag(FuncIdentity),
    BoxEnv(FuncIdentity),
    Widen(HelperId),
    DispatchCallback {
        helper: HelperId,
        arms: TySpan,
        input_tys: Ty,
    },
    EnvCallback {
        target: InstanceId,
        input_tys: Ty,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum SiteResolution {
    DirectCall {
        target: InstanceId,
        argument_types: Ty,
        pass_receiver: bool,
        pass_env: bool,
    },
    DispatchCall {
        helper: HelperId,
        argument_types: Ty,
        needs_recv: bool,
    },
    ThrowTypeError(HelperId),
    FuncTypeOf(Option<HelperId>),
    FuncCompare {
        left_ty: Ty,
        right_ty: Ty,
        op: CompareOp,
        helper: Option<HelperId>,
    },
    MethodCall {
        target: InstanceId,
        argument_types: Ty,
    },
    Callback {
        target: InstanceId,
        keep_inline_closure: bool,
    },
    ClassNew {
        class_inst: ClassInstId,
        argument_types: Ty,
    },
    ObjectLiteral(Ty),
    ZeroSizedArray,
    TypedArray(Ty),
    ZeroSizedFieldSet,
    CollectionNew {
        is_map: bool,
        type_args: Vec<Ty>,
    },
    Cell(LocId),
    FoldBool(bool),
    PromiseNew {
        value_loc: LocId,
        resolve_tid: TemplateId,
        reject_tid: TemplateId,
        executor: CallbackCall,
        pass_resolve: bool,
        pass_reject: bool,
    },
    PromiseReaction {
        result: LocId,
        helper: HelperId,
        pass_first: bool,
        pass_second: bool,
    },
    TimerStart {
        helper: HelperId,
        has_delay: bool,
        is_immediate: bool,
        pass_cb: bool,
    },
    TimerClear {
        arg_ty: Option<Ty>,
    },
}

#[derive(Clone, Default, Debug)]
pub(crate) struct SiteEntry {
    pub(crate) resolution: Option<SiteResolution>,
    pub(crate) coercion: Option<FuncCoercion>,
}

pub(crate) struct Solved {
    pub(crate) init_inst: InstanceId,
    pub(crate) instances: IdxVec<InstanceId, Instance>,
    pub(crate) class_insts: IdxVec<ClassInstId, ClassInst>,
    pub(crate) locations: IdxVec<LocId, Ty>,
    pub(crate) param_locs: Vec<LocId>,
    pub(crate) field_locs: Vec<LocId>,
    pub(crate) dispatch_arms: Vec<(FuncIdentity, InstanceId)>,
    pub(crate) helpers: IdxVec<HelperId, HelperSpec>,
    pub(crate) helper_byte_offsets: Vec<u32>,
    pub(crate) inst_local_tys: Vec<Vec<(LocalId, Ty)>>,
    pub(crate) inst_frame_locals: Vec<Vec<Ty>>,
    pub(crate) inst_sites: Vec<Vec<SiteEntry>>,
    pub(crate) inst_captures: Vec<Vec<LocalId>>,
    pub(crate) live_instances: BitSet,
    pub(crate) escaping_env_templates: BitSet,
    pub(crate) collapsed_closures: BitSet,
    pub(crate) boxed_locals: BitSet,
    pub(crate) module_local_tys: Vec<Option<Ty>>,
    pub(crate) collapsed_local_tys: Vec<Option<Ty>>,
    pub(crate) class_static_field_tys: Vec<Vec<Ty>>,
    pub(crate) class_inst_methods: Vec<Vec<InstanceId>>,
    pub(crate) class_inst_getters: Vec<Vec<(usize, InstanceId)>>,
    pub(crate) class_inst_setters: Vec<Vec<(usize, InstanceId)>>,
    pub(crate) class_static_methods: Vec<Vec<InstanceId>>,
    pub(crate) dict_reads: BitSet,
    pub(crate) self_nested_locs: BitSet,
    pub(crate) self_nested_locals: BitSet,
    pub(crate) promise_handle_locs: HashMap<(TemplateId, CaptureOwner), LocId>,
    pub(crate) deletes_array_elements: bool,
}

type InstanceKey = (TemplateId, Option<Receiver>, CaptureOwner, Ty, Ty);

struct ResultContract {
    inst: InstanceId,
    expected: Ty,
    param_name: String,
    byte_offset: u32,
}

struct InstanceReq<'a> {
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
    solver.run_worklist()?;
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
    init_inst: Option<InstanceId>,
    locations: IdxVec<LocId, Ty>,
    /// Locations whose type an annotation fixes: writes are checked by lowering, never joined.
    declared_locs: BitSet,
    dirty_locs: BitSet,
    inst_reads: Vec<Vec<LocId>>,
    reader_offsets: Vec<u32>,
    reader_instances: Vec<InstanceId>,
    csr_dirty: bool,
    evaluating_inst: Option<InstanceId>,
    current_reads: BitSet,
    current_read_list: Vec<LocId>,
    worklist: Vec<InstanceId>,
    worklist_head: usize,
    queued: BitSet,
    instances: IdxVec<InstanceId, Instance>,
    instance_index: HashMap<InstanceKey, InstanceId>,
    template_counts: Vec<u16>,
    param_locs: Vec<LocId>,
    field_locs: Vec<LocId>,
    dispatch_arms: Vec<(FuncIdentity, InstanceId)>,
    dispatch_arm_spans: Vec<TySpan>,
    helpers: IdxVec<HelperId, HelperSpec>,
    helper_index: HashMap<HelperSpec, HelperId>,
    helper_byte_offsets: Vec<u32>,
    inst_locals: Vec<Vec<InstanceLocalSlot>>,
    inst_frame_locals: Vec<Vec<Ty>>,
    inst_written: Vec<BitSet>,
    inst_captures: Vec<Vec<LocalId>>,
    inst_sites: Vec<Vec<SiteEntry>>,
    inst_superseded: BitSet,
    escaping_env_templates: BitSet,
    template_escape_locs: Vec<LocId>,
    collapsed_closures: BitSet,
    collapsed_local_locs: Vec<Option<LocId>>,
    boxed_locals: BitSet,
    dict_reads: BitSet,
    self_nested_locs: BitSet,
    self_nested_locals: BitSet,
    static_field_locs: Vec<(ClassId, usize, LocId)>,
    class_insts: IdxVec<ClassInstId, ClassInst>,
    class_index: HashMap<(ClassId, Ty), ClassInstId>,
    template_type_params: Vec<Vec<shapes::TypeId>>,
    class_type_params: Vec<Vec<shapes::TypeId>>,
    result_contracts: Vec<ResultContract>,
    contract_insts: BitSet,
    promise_handle_locs: HashMap<(TemplateId, CaptureOwner), LocId>,
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
        if inst.call_byte_offset != 0
            && let Some(view) = index.view(inst.template)
        {
            let msg = format!("while instantiating '{}'", view.display_name());
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

fn bump_count(counts: &mut [u16], template: TemplateId) {
    if let Some(slot) = counts.get_mut(template.index()) {
        *slot = slot.saturating_add(1);
    }
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
        let template_escape_locs: Vec<LocId> = (0..index.views.len())
            .map(|_| locations.push(Ty::BOTTOM))
            .collect();
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
        let template_type_params = index
            .views
            .iter()
            .map(|v| intern_tps(v.type_params))
            .collect();
        let class_type_params = index
            .classes
            .iter()
            .map(|c| {
                hir.classes
                    .get(c.hir_idx)
                    .map_or_else(Vec::new, |hc| intern_tps(&hc.type_params))
            })
            .collect();
        Self {
            ctx,
            hir,
            index,
            metadata,
            facts,
            decls,
            types,
            init_inst: None,
            locations,
            declared_locs: BitSet::default(),
            dirty_locs: BitSet::default(),
            inst_reads: Vec::new(),
            reader_offsets: Vec::new(),
            reader_instances: Vec::new(),
            csr_dirty: true,
            evaluating_inst: None,
            current_reads: BitSet::default(),
            current_read_list: Vec::new(),
            worklist: Vec::new(),
            worklist_head: 0,
            queued: BitSet::default(),
            instances: IdxVec::default(),
            instance_index: HashMap::with_capacity(64),
            template_counts: vec![0; index.views.len()],
            param_locs: Vec::new(),
            field_locs: Vec::new(),
            dispatch_arms: Vec::new(),
            dispatch_arm_spans: Vec::new(),
            helpers: IdxVec::default(),
            helper_index: HashMap::with_capacity(16),
            helper_byte_offsets: Vec::new(),
            inst_locals: Vec::new(),
            inst_frame_locals: Vec::new(),
            inst_written: Vec::new(),
            inst_captures: Vec::new(),
            inst_sites: Vec::new(),
            inst_superseded: BitSet::default(),
            escaping_env_templates: BitSet::default(),
            template_escape_locs,
            collapsed_closures: BitSet::default(),
            collapsed_local_locs: vec![None; index.local_count()],
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
            promise_handle_locs: HashMap::new(),
            next_func_id: index.max_func_id.saturating_add(1),
            deletes_array_elements: false,
        }
    }

    fn alloc_loc(&mut self, initial: Ty) -> LocId {
        self.locations.push(initial)
    }

    fn expr_cell_loc(&mut self, inst: InstanceId, expr: &Expr) -> Result<LocId> {
        let entry = self
            .index
            .site_of(self.instances[inst].template, expr)
            .and_then(|site| self.inst_sites.get_mut(inst.index())?.get_mut(site))
            .ok_or_else(|| Diagnostic::internal("missing storage site for expression"))?;
        match entry.resolution {
            Some(
                SiteResolution::Cell(loc) | SiteResolution::PromiseReaction { result: loc, .. },
            ) => Ok(loc),
            _ => {
                let loc = self.locations.push(Ty::BOTTOM);
                entry.resolution = Some(SiteResolution::Cell(loc));
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
        } else if let Some(reads) = self.inst_reads.get_mut(reader.index())
            && let Err(pos) = reads.binary_search(&loc)
        {
            reads.insert(pos, loc);
            self.csr_dirty = true;
        }
        self.locations[loc]
    }

    pub(super) fn commit_reads(&mut self, inst_id: InstanceId) {
        self.current_read_list.sort_unstable();
        if let Some(prev) = self.inst_reads.get_mut(inst_id.index())
            && *prev != self.current_read_list
        {
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
        let Some(inst) = self.instances.get(inst_id) else {
            return false;
        };
        inst.caller.is_none()
            || self.index.view(inst.template).is_some_and(|v| {
                matches!(
                    v.kind,
                    TemplateKind::ClassConstructor
                        | TemplateKind::ClassGetter(_)
                        | TemplateKind::ClassSetter(_)
                )
            })
            || self.contract_insts.contains(inst_id.index())
    }

    fn for_each_callback_target(&self, cb: CallbackCall, mut f: impl FnMut(InstanceId)) {
        match cb {
            CallbackCall::Direct { target, .. } => f(target),
            CallbackCall::Dispatch { helper, .. } => {
                if let Some(HelperSpec::Dispatcher { arms, .. }) = self.helpers.get(helper)
                    && let Some(slice) = self.dispatch_arms.get(arms.range())
                {
                    for &(_, target) in slice {
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
                if let Some(HelperSpec::Dispatcher { arms, .. }) = self.helpers.get(helper)
                    && let Some(slice) = self.dispatch_arms.get(arms.range())
                {
                    for &(_, target) in slice {
                        f(target);
                    }
                }
            }
            SiteResolution::ClassNew { class_inst, .. } => {
                if let Some(ctor) = self.class_insts.get(class_inst).and_then(|c| c.ctor_inst) {
                    f(ctor);
                }
            }
            SiteResolution::PromiseNew { executor, .. } => {
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
            for entry in self.inst_sites.get(caller.index()).into_iter().flatten() {
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
                        if let Some(slice) = self.dispatch_arms.get(arms.range()) {
                            for &(_, target) in slice {
                                if live.insert(target.index()) {
                                    queue.push(target);
                                }
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
        self.template_counts.fill(0);
        for (id, inst) in self.instances.iter_enumerated() {
            let idx = id.index();
            if live.contains(idx) {
                bump_count(&mut self.template_counts, inst.template);
            } else if self.inst_superseded.insert(idx) {
                if let Some(sites) = self.inst_sites.get_mut(idx) {
                    sites.fill(SiteEntry::default());
                }
                if let Some(reads) = self.inst_reads.get_mut(idx)
                    && !reads.is_empty()
                {
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

    fn rebuild_csr(&mut self) {
        let Some(init_inst) = self.init_inst else {
            return;
        };
        self.reader_offsets.clear();
        self.reader_offsets.resize(self.locations.len() + 1, 0);
        for (inst_i, reads) in self.inst_reads.iter().enumerate() {
            if self.inst_superseded.contains(inst_i) {
                continue;
            }
            for &loc in reads {
                if let Some(slot) = self.reader_offsets.get_mut(loc.index() + 1) {
                    *slot = slot.saturating_add(1);
                }
            }
        }
        let mut acc = 0u32;
        for off in &mut self.reader_offsets {
            acc = acc.saturating_add(*off);
            *off = acc;
        }
        let total = self.reader_offsets.last().copied().unwrap_or(0) as usize;
        self.reader_instances.clear();
        self.reader_instances.resize(total, init_inst);
        let mut cursor = self.reader_offsets.clone();
        for (reader, reads) in self
            .instances
            .iter_enumerated()
            .map(|(id, _)| id)
            .zip(&self.inst_reads)
        {
            if self.inst_superseded.contains(reader.index()) {
                continue;
            }
            for &loc in reads {
                if let Some(pos) = cursor.get_mut(loc.index()) {
                    let idx = *pos as usize;
                    *pos = pos.saturating_add(1);
                    if let Some(slot) = self.reader_instances.get_mut(idx) {
                        *slot = reader;
                    }
                }
            }
        }
        self.csr_dirty = false;
    }

    fn run_worklist(&mut self) -> Result<()> {
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
                    self.rebuild_csr();
                }
                let dirty: Vec<usize> = self.dirty_locs.iter().collect();
                self.dirty_locs.clear();
                for loc_idx in dirty {
                    let start = self.reader_offsets.get(loc_idx).copied().unwrap_or(0) as usize;
                    let end = self
                        .reader_offsets
                        .get(loc_idx + 1)
                        .copied()
                        .unwrap_or(start as u32) as usize;
                    for &r in self.reader_instances.get(start..end).unwrap_or(&[]) {
                        if !self.inst_superseded.contains(r.index())
                            && self.queued.insert(r.index())
                        {
                            self.worklist.push(r);
                        }
                    }
                }
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

    pub(super) fn intern_dispatch_arms(&mut self, arms: &[(FuncIdentity, InstanceId)]) -> TySpan {
        for &span in &self.dispatch_arm_spans {
            if self.dispatch_arms.get(span.range()) == Some(arms) {
                return span;
            }
        }
        let span = TySpan {
            start: self.dispatch_arms.len() as u32,
            len: arms.len() as u32,
        };
        self.dispatch_arms.extend_from_slice(arms);
        self.dispatch_arm_spans.push(span);
        span
    }

    pub(super) fn intern_helper(&mut self, spec: HelperSpec, byte_offset: u32) -> HelperId {
        if let Some(&id) = self.helper_index.get(&spec) {
            if byte_offset != 0
                && let Some(slot) = self.helper_byte_offsets.get_mut(id.index())
                && *slot == 0
            {
                *slot = byte_offset;
            }
            return id;
        }
        let id = self.helpers.push(spec);
        self.helper_byte_offsets.push(byte_offset);
        self.helper_index.insert(spec, id);
        id
    }

    pub(super) fn collapse_closure(&mut self, tid: TemplateId, owner_tid: TemplateId) {
        if !self.collapsed_closures.insert(tid.index()) {
            return;
        }
        let caps: Vec<LocalId> = self.index.template_captures(tid).to_vec();
        for cap_id in caps {
            let Some(lidx) = self.index.local_idx(cap_id) else {
                continue;
            };
            if self.index.local_module.contains(lidx.index()) {
                continue;
            }
            let cloc = match self
                .collapsed_local_locs
                .get(lidx.index())
                .copied()
                .flatten()
            {
                Some(loc) => loc,
                None => {
                    let loc = self.alloc_loc(Ty::BOTTOM);
                    if let Some(slot) = self.collapsed_local_locs.get_mut(lidx.index()) {
                        *slot = Some(loc);
                    }
                    loc
                }
            };
            let cap_owner_tid = self.index.local_owner.get(lidx.index()).copied();
            let seed_tys: Vec<Ty> = self
                .instances
                .iter_enumerated()
                .filter_map(|(id, inst)| {
                    if self.inst_superseded.contains(id.index())
                        || Some(inst.template) != cap_owner_tid
                    {
                        return None;
                    }
                    if let Some(slot) = self.local_frame_slot_in(id, lidx) {
                        self.inst_frame_locals
                            .get(id.index())
                            .and_then(|f| f.get(slot))
                            .copied()
                    } else {
                        self.inst_locals
                            .get(id.index())
                            .and_then(|sl| sl.iter().find(|s| s.local == lidx))
                            .and_then(|s| self.locations.get(s.loc).copied())
                    }
                })
                .collect();
            for ty in seed_tys {
                self.write_loc(cloc, ty);
            }
        }
        let owners: Vec<InstanceId> = self
            .instances
            .iter_enumerated()
            .filter_map(|(id, inst)| {
                (!self.inst_superseded.contains(id.index()) && inst.template == owner_tid)
                    .then_some(id)
            })
            .collect();
        for id in owners {
            self.enqueue(id);
        }
    }

    pub(super) fn is_escaping_env(&mut self, tid: TemplateId, reader: InstanceId) -> bool {
        let Some(&loc) = self.template_escape_locs.get(tid.index()) else {
            return false;
        };
        self.read_loc(loc, reader) != Ty::BOTTOM
    }

    pub(super) fn is_tagged_func_union_in(&mut self, ty: Ty, reader: InstanceId) -> bool {
        if let TyKind::Union(span) = self.types.kind(ty) {
            let members: Vec<Ty> = self.types.ty_slice(span).to_vec();
            for m in members {
                if let TyKind::Func(ident) = self.types.kind(m) {
                    let _ = self.is_escaping_env(ident.template, reader);
                }
            }
        }
        self.types.is_tagged_func_union(
            ty,
            &self.index.capture_free_templates,
            &self.escaping_env_templates,
        )
    }

    pub(super) fn mark_escaping_env(&mut self, _caller: InstanceId, ty: Ty) {
        let members: Vec<_> = self
            .types
            .func_members(&ty)
            .filter(|ident| {
                !ident.capture_owner.is_none()
                    || !self
                        .index
                        .capture_free_templates
                        .contains(ident.template.index())
            })
            .collect();
        for ident in members {
            let tid = ident.template;
            if !self.escaping_env_templates.insert(tid.index()) {
                continue;
            }
            if let Some(&loc) = self.template_escape_locs.get(tid.index()) {
                self.write_loc(loc, Ty::BOOLEAN);
            }
            for &cap_id in self.index.template_captures(tid) {
                if let Some(lidx) = self.index.local_idx(cap_id) {
                    let li = lidx.index();
                    if !self.index.local_module.contains(li)
                        && (self.index.local_reassigned.contains(li)
                            || self.index.local_written_from_closure.contains(li)
                            || self.index.local_deferred.contains(li)
                            || self.index.local_for_head.contains(li)
                            || self.index.local_forward_captured.contains(li))
                    {
                        self.boxed_locals.insert(li);
                    }
                }
            }
        }
    }

    pub(super) fn record_site_coercion(
        &mut self,
        inst_id: InstanceId,
        expr: &Expr,
        c: FuncCoercion,
    ) {
        let Some(template) = self.instances.get(inst_id).map(|i| i.template) else {
            return;
        };
        let Some(local_site) = self.index.site_of(template, expr) else {
            debug_assert!(
                false,
                "unindexed coercion site in template {}",
                template.index()
            );
            return;
        };
        if let Some(slot) = self
            .inst_sites
            .get_mut(inst_id.index())
            .and_then(|s| s.get_mut(local_site))
        {
            slot.coercion = Some(c);
        }
    }

    pub(super) fn clear_site(&mut self, inst_id: InstanceId, expr: &Expr) {
        let Some(template) = self.instances.get(inst_id).map(|i| i.template) else {
            return;
        };
        let Some(local_site) = self.index.site_of(template, expr) else {
            return;
        };
        if let Some(slot) = self
            .inst_sites
            .get_mut(inst_id.index())
            .and_then(|s| s.get_mut(local_site))
        {
            slot.resolution = None;
        }
    }

    pub(super) fn coerce_array_write(
        &mut self,
        inst_id: InstanceId,
        arr_def: Ty,
        args: &[Expr],
        arg_tys: &[Ty],
    ) {
        for &at in arg_tys {
            self.mark_escaping_env(inst_id, at);
        }
        let elem_ty = match self.types.kind(arr_def) {
            TyKind::Cell { loc, kind } if kind.is_array() => Some(self.read_loc(loc, inst_id)),
            TyKind::Array(elem) => Some(elem),
            _ => None,
        };
        let Some(elem_ty) = elem_ty else { return };
        if self
            .types
            .is_zero_sized_func(elem_ty, &self.index.capture_free_templates)
        {
            for a in args {
                self.record_site_coercion(inst_id, a, FuncCoercion::Zero);
            }
        } else if self.is_tagged_func_union_in(elem_ty, inst_id) {
            for (a, &at) in args.iter().zip(arg_tys) {
                self.record_coercion(inst_id, a, at, elem_ty);
            }
        }
    }

    pub(super) fn record_coercion(
        &mut self,
        inst_id: InstanceId,
        expr: &Expr,
        src_ty: Ty,
        dst_ty: Ty,
    ) {
        if src_ty == Ty::BOTTOM || dst_ty == Ty::BOTTOM {
            return;
        }
        if !self.is_tagged_func_union_in(dst_ty, inst_id) {
            return;
        }
        let inner = match expr {
            Expr::Sequence(parts) if self.metadata.locations.operator_span(parts).is_some() => {
                parts.last().unwrap_or(expr)
            }
            _ => expr,
        };
        if matches!(inner, Expr::Conditional { .. } | Expr::Logical { .. }) {
            return;
        }
        if self
            .types
            .is_zero_sized_func(src_ty, &self.index.capture_free_templates)
            && let TyKind::Func(ident) = self.types.kind(src_ty)
        {
            self.record_site_coercion(inst_id, expr, FuncCoercion::Tag(ident));
            return;
        }
        if let TyKind::Func(ident) = self.types.kind(src_ty)
            && self.is_escaping_env(ident.template, inst_id)
        {
            if !matches!(expr, Expr::Closure { .. }) {
                self.record_site_coercion(inst_id, expr, FuncCoercion::BoxEnv(ident));
            }
            return;
        }
        if self.is_tagged_func_union_in(src_ty, inst_id)
            && self.types.strip_nullish(src_ty).0 != self.types.strip_nullish(dst_ty).0
        {
            let helper = self.intern_helper(
                HelperSpec::Widener {
                    from: src_ty,
                    to: dst_ty,
                },
                0,
            );
            self.record_site_coercion(inst_id, expr, FuncCoercion::Widen(helper));
        }
    }

    pub(super) fn annotate_expr_error(
        &self,
        s: &EvalState<'h>,
        mut err: Diagnostic,
        byte_offset: u32,
    ) -> Diagnostic {
        if byte_offset != 0 {
            err = self.metadata.locations.annotate_call(err, byte_offset);
        } else if let Some(sp) = s.current_span {
            err = self.metadata.locations.annotate(err, sp);
        } else if let Some(sp) = self
            .instances
            .get(s.inst)
            .and_then(|i| self.index.view(i.template))
            .and_then(|v| self.metadata.locations.function_span(v.name))
        {
            err = err.with_span(sp);
        }
        attach_inst_chain(
            &self.instances,
            self.index,
            &self.metadata.locations,
            err,
            s.inst,
        )
    }

    pub(super) fn check_escaping_func(
        &mut self,
        inst_id: InstanceId,
        ty: Ty,
        byte_offset: u32,
        span: Option<swc_common::Span>,
    ) -> Result<()> {
        self.mark_escaping_env(inst_id, ty);
        if let TyKind::Union(u_span) = self.types.kind(ty)
            && self
                .types
                .ty_slice(u_span)
                .iter()
                .any(|&m| matches!(self.types.kind(m), TyKind::Func(_)))
            && !self.is_tagged_func_union_in(ty, inst_id)
        {
            let mut err = Diagnostic::unsupported(
                "joining function values with non-function values is not supported yet",
            );
            if byte_offset != 0 {
                err = self.metadata.locations.annotate_call(err, byte_offset);
            } else if let Some(sp) = span {
                err = self.metadata.locations.annotate(err, sp);
            } else if let Some(sp) = self
                .instances
                .get(inst_id)
                .and_then(|i| self.index.view(i.template))
                .and_then(|v| self.metadata.locations.function_span(v.name))
            {
                err = err.with_span(sp);
            }
            return Err(attach_inst_chain(
                &self.instances,
                self.index,
                &self.metadata.locations,
                err,
                inst_id,
            ));
        }
        Ok(())
    }

    fn verify_contracts(&mut self) -> Result<()> {
        let contracts: Vec<(InstanceId, Ty, String, u32)> = self
            .result_contracts
            .iter()
            .map(|c| (c.inst, c.expected, c.param_name.clone(), c.byte_offset))
            .collect();
        for (inst_id, expected, param_name, byte_offset) in contracts {
            let inst = self.instances.get(inst_id).ok_or_else(|| {
                Diagnostic::internal("callback contract references missing instance")
            })?;
            let raw_actual = *self.locations.get(inst.return_loc).ok_or_else(|| {
                Diagnostic::internal("callback contract references missing return location")
            })?;
            let actual = match self.types.kind(raw_actual) {
                TyKind::Cell {
                    loc,
                    kind: CellKind::Promise,
                } => {
                    let inner = self.locations.get(loc).copied().unwrap_or(Ty::BOTTOM);
                    self.types
                        .promise(if inner == Ty::BOTTOM { Ty::VOID } else { inner })
                }
                _ => raw_actual,
            };
            if !self.types.accepts(self.ctx, expected, actual) {
                return Err(self.metadata.locations.annotate_call(
                    diag!(
                        codes::TYPE_MISMATCH,
                        "callback result does not match the declared result of parameter '{}'",
                        param_name
                    ),
                    byte_offset,
                ));
            }
        }
        Ok(())
    }
}
