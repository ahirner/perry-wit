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
    pub(crate) is_witness: bool,
    pub(crate) list_result: Option<Ty>,
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
    pub(crate) field_init: Option<InstanceId>,
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
    ResourceCall(usize),
    BranchResult(Ty),
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
        executor: Option<(CallbackCall, bool, bool)>,
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

impl SiteEntry {
    pub(crate) fn promise(
        sites: &IdxVec<InstanceId, Vec<Self>>,
        owner: CaptureOwner,
        site: usize,
    ) -> Result<LocId> {
        let entry = owner.instance().and_then(|inst| sites[inst].get(site));
        match entry.and_then(|e| e.resolution.as_ref()) {
            Some(SiteResolution::PromiseNew { value_loc, .. }) => Ok(*value_loc),
            _ => Err(Diagnostic::internal(
                "missing promise constructor for settlement handle",
            )),
        }
    }
}

pub(crate) struct Solved {
    pub(crate) witness_instances: Vec<Option<InstanceId>>,
    pub(crate) init_inst: InstanceId,
    pub(crate) instances: IdxVec<InstanceId, Instance>,
    pub(crate) class_insts: IdxVec<ClassInstId, ClassInst>,
    pub(crate) locations: IdxVec<LocId, Ty>,
    pub(crate) param_locs: Vec<LocId>,
    pub(crate) field_locs: Vec<LocId>,
    pub(crate) dispatch_arms: Vec<(FuncIdentity, InstanceId)>,
    pub(crate) helpers: IdxVec<HelperId, HelperSpec>,
    pub(crate) helper_byte_offsets: IdxVec<HelperId, u32>,
    pub(crate) inst_local_tys: IdxVec<InstanceId, Vec<(LocalId, Ty)>>,
    pub(crate) inst_frame_locals: IdxVec<InstanceId, Vec<Ty>>,
    pub(crate) inst_sites: IdxVec<InstanceId, Vec<SiteEntry>>,
    pub(crate) inst_captures: IdxVec<InstanceId, Vec<LocalId>>,
    pub(crate) live_instances: BitSet,
    pub(crate) escaping_env_templates: BitSet,
    pub(crate) collapsed_closures: BitSet,
    pub(crate) boxed_locals: BitSet,
    pub(crate) module_local_tys: IdxVec<LocalIdx, Option<Ty>>,
    pub(crate) collapsed_local_tys: IdxVec<LocalIdx, Option<Ty>>,
    pub(crate) class_static_field_tys: IdxVec<ClassId, Vec<Ty>>,
    pub(crate) class_inst_methods: IdxVec<ClassInstId, Vec<InstanceId>>,
    pub(crate) class_inst_getters: IdxVec<ClassInstId, Vec<(usize, InstanceId)>>,
    pub(crate) class_inst_setters: IdxVec<ClassInstId, Vec<(usize, InstanceId)>>,
    pub(crate) class_static_methods: IdxVec<ClassId, Vec<InstanceId>>,
    pub(crate) dict_reads: BitSet,
    pub(crate) self_nested_locs: BitSet,
    pub(crate) self_nested_locals: BitSet,
    pub(crate) deletes_array_elements: bool,
}

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
    param_locs: Vec<LocId>,
    field_locs: Vec<LocId>,
    dispatch_arms: Vec<(FuncIdentity, InstanceId)>,
    dispatch_arm_spans: Vec<TySpan>,
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
            param_locs: Vec::new(),
            field_locs: Vec::new(),
            dispatch_arms: Vec::new(),
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
            let cloc = match self.collapsed_local_locs[lidx] {
                Some(loc) => loc,
                None => {
                    let loc = self.alloc_loc(Ty::BOTTOM);
                    self.collapsed_local_locs[lidx] = Some(loc);
                    loc
                }
            };
            let cap_owner_tid = self.index.local_owner[lidx];
            let seed_tys: Vec<Ty> = self
                .instances
                .iter_enumerated()
                .filter_map(|(id, inst)| {
                    if self.inst_superseded.contains(id.index()) || inst.template != cap_owner_tid {
                        return None;
                    }
                    if let Some(slot) = self.local_frame_slot_in(id, lidx) {
                        self.inst_frame_locals[id].get(slot).copied()
                    } else {
                        self.inst_locals[id]
                            .iter()
                            .find(|s| s.local == lidx)
                            .map(|s| self.locations[s.loc])
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
        let loc = self.template_escape_locs[tid];
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
            let loc = self.template_escape_locs[tid];
            self.write_loc(loc, Ty::BOOLEAN);
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
        let template = self.instances[inst_id].template;
        let Some(local_site) = self.index.site_of(template, expr) else {
            debug_assert!(
                false,
                "unindexed coercion site in template {}",
                template.index()
            );
            return;
        };
        if let Some(slot) = self.inst_sites[inst_id].get_mut(local_site) {
            slot.coercion = Some(c);
        }
    }

    pub(super) fn clear_site(&mut self, inst_id: InstanceId, expr: &Expr) {
        let template = self.instances[inst_id].template;
        let Some(local_site) = self.index.site_of(template, expr) else {
            return;
        };
        if let Some(slot) = self.inst_sites[inst_id].get_mut(local_site) {
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

    fn annotate_inst_error(
        &self,
        inst_id: InstanceId,
        mut err: Diagnostic,
        byte_offset: u32,
        span: Option<swc_common::Span>,
    ) -> Diagnostic {
        if hir_compat::has_source_offset(byte_offset) {
            err = self.metadata.locations.annotate_call(err, byte_offset);
        } else if let Some(sp) = span {
            err = self.metadata.locations.annotate(err, sp);
        } else if let Some(sp) = self
            .metadata
            .locations
            .function_span(self.index.view(self.instances[inst_id].template).name)
        {
            err = err.with_span(sp);
        }
        attach_inst_chain(
            &self.instances,
            self.index,
            &self.metadata.locations,
            err,
            inst_id,
        )
    }

    pub(super) fn annotate_expr_error(
        &self,
        s: &EvalState<'h>,
        err: Diagnostic,
        byte_offset: u32,
    ) -> Diagnostic {
        self.annotate_inst_error(s.inst, err, byte_offset, s.current_span)
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
            let err = Diagnostic::unsupported(
                "joining function values with non-function values is not supported yet",
            );
            return Err(self.annotate_inst_error(inst_id, err, byte_offset, span));
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
                    let inner = self.locations[loc];
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
