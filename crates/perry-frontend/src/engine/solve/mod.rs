//! Worklist monomorphization and storage-join solver over dense indices.
mod calls;
mod eval;
mod expr;
use super::bitset::BitSet;
use super::boundary::{ClassInfo, ModuleIndex, SourceFacts, TemplateKind};
use super::builtins::{self, MethodResultArgs};
use super::types::{
    CaptureOwner, CellKind, ClassId, ClassInstId, FuncIdentity, IdxVec, InstanceId, LocId,
    LocalIdx, ObjectProp, TemplateId, Ty, TyKind, TySpan, TypeTable,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FuncCoercion {
    Zero,
    Tag(FuncIdentity),
    BoxEnv(FuncIdentity),
    Widen {
        from: Ty,
        to: Ty,
    },
    DispatchCallback {
        callee_ty: Ty,
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
        strip_arg_mask: u64,
        pass_receiver: bool,
        pass_env: bool,
    },
    DispatchCall {
        callee_ty: Ty,
        arms: TySpan,
        strip_arg_mask: u64,
        arg_tys: Ty,
        recv_ty: Option<Ty>,
        call_byte_offset: u32,
    },
    ThrowTypeError,
    FuncTypeOf(Ty),
    FuncCompare {
        left_ty: Ty,
        right_ty: Ty,
        op: CompareOp,
    },
    MethodCall {
        target: InstanceId,
        strip_arg_mask: u64,
    },
    Callback {
        target: InstanceId,
        keep_inline_closure: bool,
    },
    ClassNew {
        class_inst: ClassInstId,
        strip_arg_mask: u64,
    },
    ObjectLiteral(Ty),
    ZeroSizedArray,
    TaggedArray(Ty),
    ZeroSizedFieldSet,
    CollectionNew {
        is_map: bool,
        type_args: Vec<Ty>,
    },
    Cell(LocId),
    FoldBool(bool),
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
            next_func_id: index.max_func_id.saturating_add(1),
            deletes_array_elements: false,
        }
    }

    fn alloc_loc(&mut self, initial: Ty) -> LocId {
        self.locations.push(initial)
    }

    pub(super) fn alloc_expr_cell(
        &mut self,
        inst_id: InstanceId,
        expr: &Expr,
        kind: CellKind,
    ) -> Ty {
        let Some(template) = self.instances.get(inst_id).map(|i| i.template) else {
            return Ty::OPEN;
        };
        let Some(local_site) = self.index.site_of(template, expr) else {
            return Ty::OPEN;
        };
        if let Some(Some(SiteResolution::Cell(loc))) = self
            .inst_sites
            .get(inst_id.index())
            .and_then(|s| s.get(local_site))
            .map(|e| &e.resolution)
        {
            return self.types.cell(*loc, kind);
        }
        let loc = self.alloc_loc(Ty::BOTTOM);
        if let Some(slot) = self
            .inst_sites
            .get_mut(inst_id.index())
            .and_then(|s| s.get_mut(local_site))
        {
            slot.resolution = Some(SiteResolution::Cell(loc));
        }
        self.types.cell(loc, kind)
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

    fn for_each_site_target(&self, res: &SiteResolution, mut f: impl FnMut(InstanceId)) {
        match *res {
            SiteResolution::DirectCall { target, .. }
            | SiteResolution::MethodCall { target, .. }
            | SiteResolution::Callback { target, .. } => f(target),
            SiteResolution::DispatchCall { arms, .. } => {
                if let Some(slice) = self.dispatch_arms.get(arms.range()) {
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

    fn seed_roots(&mut self) -> Result<InstanceId> {
        let init_inst = self.request_instance(InstanceReq::new(TemplateId::INIT, &[], None))?;
        self.init_inst = Some(init_inst);
        self.enqueue(init_inst);
        let roots: Vec<TemplateId> = self
            .index
            .views
            .iter_enumerated()
            .skip(1)
            .filter_map(|(tid, view)| {
                (view.is_exported || self.index.exported_obj_methods.contains(tid.index()))
                    .then_some(tid)
            })
            .collect();
        for tid in roots {
            let Some(view) = self.index.view(tid) else {
                continue;
            };
            let bindings = self.default_type_param_bindings(tid);
            let mut arg_tys = Vec::with_capacity(view.params.len());
            for param in view.params {
                let ty = self.types.intern_hir(
                    self.ctx,
                    &param.ty,
                    self.index.is_param_annotated(param.id),
                );
                let ty = self.types.substitute(self.ctx, ty, &bindings);
                arg_tys.push(self.instantiate_annotated_class(ty, 0, None)?);
            }
            let inst = self.request_instance(InstanceReq::new(tid, &arg_tys, None))?;
            self.enqueue(inst);
        }
        Ok(init_inst)
    }

    fn default_type_param_bindings(&mut self, tid: TemplateId) -> Vec<(shapes::TypeId, Ty)> {
        let Some(view) = self.index.view(tid) else {
            return Vec::new();
        };
        let Some(tps) = self.template_type_params.get(tid.index()) else {
            return Vec::new();
        };
        view.type_params
            .iter()
            .zip(tps)
            .filter_map(|(tp, &tp_id)| {
                Some((
                    tp_id,
                    self.types.intern_hir(
                        self.ctx,
                        tp.constraint.as_deref().or(tp.default.as_deref())?,
                        true,
                    ),
                ))
            })
            .collect()
    }

    fn instantiate_annotated_class(
        &mut self,
        ty: Ty,
        byte_offset: u32,
        caller: Option<InstanceId>,
    ) -> Result<Ty> {
        match self.types.kind(ty) {
            TyKind::Leaf(base) | TyKind::Generic { base, .. } => {
                let args = match self.types.kind(ty) {
                    TyKind::Generic { args, .. } => self.types.ty_slice(args).to_vec(),
                    _ => Vec::new(),
                };
                if let HirType::Named(name) = base.hir(self.ctx) {
                    let nid = self.types.intern_name(name);
                    if let Some(cid) = self.index.class_by_name_id(nid) {
                        let resolved: Vec<Ty> = args
                            .into_iter()
                            .map(|a| self.instantiate_annotated_class(a, byte_offset, caller))
                            .collect::<Result<_>>()?;
                        let cinst = self.request_class_inst(cid, &resolved, byte_offset, caller)?;
                        return Ok(self.types.class_inst(cinst));
                    }
                }
                Ok(ty)
            }
            TyKind::Array(i) => {
                let n = self.instantiate_annotated_class(i, byte_offset, caller)?;
                Ok(if n == i { ty } else { self.types.array(n) })
            }
            TyKind::Promise(i) => {
                let n = self.instantiate_annotated_class(i, byte_offset, caller)?;
                Ok(if n == i { ty } else { self.types.promise(n) })
            }
            TyKind::Union(s) => {
                let mut changed = false;
                let mut out = Vec::with_capacity(s.len as usize);
                for m in self.types.ty_slice(s).to_vec() {
                    let n = self.instantiate_annotated_class(m, byte_offset, caller)?;
                    changed |= n != m;
                    out.push(n);
                }
                Ok(if changed { self.types.union(&out) } else { ty })
            }
            _ => Ok(ty),
        }
    }

    fn collect_non_global_caps(&self, captures: &[LocalId], out: &mut Vec<LocalId>) -> bool {
        let mut any = false;
        for &cap in captures {
            if self.index.is_non_global_local(cap) {
                any = true;
                if !out.contains(&cap) {
                    out.push(cap);
                }
            }
        }
        any
    }

    fn request_instance(&mut self, req: InstanceReq<'_>) -> Result<InstanceId> {
        let InstanceReq {
            template,
            receiver,
            capture_owner,
            args,
            explicit_type_args,
            call_byte_offset,
            caller,
            is_inline_callback,
        } = req;
        let is_monomorphic_plain = self.index.view(template).is_some_and(|v| {
            matches!(v.kind, TemplateKind::Function)
                && v.type_params.is_empty()
                && !args
                    .iter()
                    .any(|&t| matches!(self.types.kind(t), TyKind::Func(_)))
                && v.params.iter().all(|p| {
                    !matches!(p.ty, HirType::Any | HirType::Unknown | HirType::Function(_))
                        && !p.is_rest
                        && p.default.is_none()
                })
        });
        let canonical_buf: Vec<Ty>;
        let args = if is_monomorphic_plain
            && receiver.is_none()
            && capture_owner.is_none()
            && explicit_type_args.is_empty()
            && let Some(view) = self.index.view(template)
            && !view.params.is_empty()
        {
            let mut declared = Vec::with_capacity(view.params.len());
            for param in view.params {
                let ty = self.types.intern_hir(self.ctx, &param.ty, true);
                declared.push(self.instantiate_annotated_class(ty, 0, None)?);
            }
            canonical_buf = declared;
            canonical_buf.as_slice()
        } else {
            args
        };
        let args_key = self.types.tuple(args);
        let targs_key = self.types.tuple(explicit_type_args);
        let key: InstanceKey = (template, receiver, capture_owner, args_key, targs_key);
        if let Some(&id) = self.instance_index.get(&key) {
            if is_inline_callback && let Some(inst) = self.instances.get_mut(id) {
                inst.is_inline_callback = true;
            }
            if self.inst_superseded.remove(id.index()) {
                bump_count(&mut self.template_counts, template);
                self.enqueue(id);
            }
            return Ok(id);
        }

        let mut count = self
            .template_counts
            .get(template.index())
            .copied()
            .unwrap_or(0);
        if count >= INSTANCE_BUDGET {
            self.prune_unreachable();
            count = self
                .template_counts
                .get(template.index())
                .copied()
                .unwrap_or(0);
            if count >= INSTANCE_BUDGET {
                return Err(self.budget_error(template, args, caller, call_byte_offset));
            }
        }
        bump_count(&mut self.template_counts, template);

        let view = self
            .index
            .view(template)
            .ok_or_else(|| Diagnostic::internal("inference engine requested unknown template"))?;

        let mut inst_caps = Vec::new();
        let mut captures_non_global = capture_owner.is_some();
        if capture_owner.is_some() {
            captures_non_global |= self.collect_non_global_caps(view.captures, &mut inst_caps);
        }
        for &arg in args {
            if let TyKind::Func(ident) = self.types.kind(arg)
                && let Some(cb) = self.index.view(ident.template)
            {
                captures_non_global |= self.collect_non_global_caps(cb.captures, &mut inst_caps);
            }
        }

        let mut type_bindings = self.compute_type_param_bindings(
            template,
            receiver,
            capture_owner,
            args,
            explicit_type_args,
        );
        if view.is_exported && type_bindings.is_empty() {
            type_bindings = self.default_type_param_bindings(template);
        }
        if caller.is_some()
            && !view.type_params.is_empty()
            && let Some(own_tps) = self.template_type_params.get(template.index())
        {
            for (tp, &tp_id) in view.type_params.iter().zip(own_tps) {
                if !type_bindings.iter().any(|(id, _)| *id == tp_id) {
                    let mut err = diag!(
                        codes::MISSING_TYPE_ANNOTATION,
                        "cannot infer type argument '{}' for generic function '{}'",
                        tp.name,
                        hir_compat::display_name(view.name)
                    );
                    if call_byte_offset != 0 {
                        err = self.metadata.locations.annotate_call(err, call_byte_offset);
                    }
                    if let Some(cid) = caller {
                        err = attach_inst_chain(
                            &self.instances,
                            self.index,
                            &self.metadata.locations,
                            err,
                            cid,
                        );
                    }
                    return Err(err);
                }
            }
        }

        let param_loc_start = self.param_locs.len() as u32;
        let mut local_slots = Vec::with_capacity(view.params.len() + 4);
        let fixed_count = view
            .params
            .iter()
            .position(|p| p.is_rest)
            .unwrap_or(view.params.len());
        for (p_idx, param) in view.params.iter().enumerate() {
            let declared =
                self.types
                    .intern_hir(self.ctx, &param.ty, self.index.is_param_annotated(param.id));
            let declared = self.types.substitute(self.ctx, declared, &type_bindings);
            let bound_ty = if param.is_rest {
                let extra_args = args.get(fixed_count..).unwrap_or(&[]);
                if !extra_args.is_empty() {
                    let elem = extra_args.iter().fold(Ty::BOTTOM, |acc, &x| {
                        let norm = self.types.normalize_for_storage(self.ctx, self.decls, x);
                        self.types.join(acc, norm)
                    });
                    self.types.array(elem)
                } else if !self.types.is_open(declared) {
                    declared
                } else {
                    self.types.array(Ty::NEVER)
                }
            } else {
                let incoming = args.get(p_idx).copied().unwrap_or(Ty::VOID);
                if incoming == Ty::OPEN || incoming == Ty::BOTTOM {
                    declared
                } else if matches!(self.types.kind(incoming), TyKind::Func(_)) {
                    incoming
                } else if param.default.is_some() {
                    if !self.types.is_open(declared) {
                        declared
                    } else {
                        let s = self.types.exclude_absences(incoming, Absence::Undefined);
                        if s == Ty::NEVER { Ty::BOTTOM } else { s }
                    }
                } else if !self.types.is_open(declared)
                    && !matches!(self.types.kind(declared), TyKind::Leaf(id) if matches!(id.hir(self.ctx), HirType::Function(_)))
                {
                    declared
                } else {
                    incoming
                }
            };
            let loc = self.alloc_loc(bound_ty);
            self.param_locs.push(loc);
            if let Some(local) = self.index.local_idx(param.id) {
                local_slots.push(InstanceLocalSlot { local, loc });
                if let Some(cloc) = self
                    .collapsed_local_locs
                    .get(local.index())
                    .copied()
                    .flatten()
                {
                    self.write_loc(cloc, bound_ty);
                }
            }
        }
        for &cap_local in self.index.template_captured_locals(template) {
            if !local_slots.iter().any(|s| s.local == cap_local) {
                let loc = self.alloc_loc(Ty::BOTTOM);
                local_slots.push(InstanceLocalSlot {
                    local: cap_local,
                    loc,
                });
            }
        }
        let param_locs = TySpan {
            start: param_loc_start,
            len: view.params.len() as u32,
        };

        let explicit_return = view
            .explicit_return
            .then(|| {
                let ret = self.types.intern_hir(self.ctx, view.return_type, true);
                let ret = self.types.substitute(self.ctx, ret, &type_bindings);
                (!self.types.is_open(ret)).then_some(ret)
            })
            .flatten();
        let return_loc = self.alloc_loc(explicit_return.unwrap_or(Ty::BOTTOM));

        let keep_original = count == 0
            && (view.is_exported
                || self.index.exported_obj_methods.contains(template.index())
                || is_monomorphic_plain);
        let emitted_func_id = if keep_original {
            view.func_id
        } else {
            let id = self.next_func_id;
            self.next_func_id = self.next_func_id.saturating_add(1);
            id
        };
        let emitted_name = if template == TemplateId::INIT || keep_original {
            view.raw_name()
        } else {
            format!("{}_instance_{emitted_func_id}", view.raw_name())
        };
        let emitted_method_name = match view.kind {
            TemplateKind::ClassMethod
            | TemplateKind::ClassStaticMethod(_)
            | TemplateKind::ClassGetter(_)
            | TemplateKind::ClassSetter(_) => {
                if self
                    .instances
                    .iter()
                    .any(|i| i.template == template && i.receiver == receiver)
                {
                    format!("{}__inst_{emitted_func_id}", view.name)
                } else {
                    view.name.to_owned()
                }
            }
            _ => String::new(),
        };

        let inst_id = self.instances.push(Instance {
            template,
            receiver,
            capture_owner,
            args: args_key,
            explicit_type_args: targs_key,
            return_loc,
            has_explicit_return: explicit_return.is_some(),
            param_locs,
            caller,
            call_byte_offset,
            is_inline_callback,
            captures_non_global,
            emitted_func_id,
            emitted_name,
            emitted_method_name,
        });
        self.instance_index.insert(key, inst_id);
        self.inst_locals.push(local_slots);
        self.inst_frame_locals
            .push(vec![Ty::BOTTOM; self.index.template_frame_count(template)]);
        self.inst_written.push(BitSet::default());
        self.inst_captures.push(inst_caps);
        self.inst_sites
            .push(vec![SiteEntry::default(); self.index.site_count(template)]);
        self.inst_reads.push(Vec::new());
        self.enqueue(inst_id);
        Ok(inst_id)
    }

    fn compute_type_param_bindings(
        &mut self,
        template: TemplateId,
        receiver: Option<Receiver>,
        capture_owner: CaptureOwner,
        args: &[Ty],
        explicit_type_args: &[Ty],
    ) -> Vec<(shapes::TypeId, Ty)> {
        let mut bindings = capture_owner
            .instance()
            .map_or_else(Vec::new, |o| self.active_instance_type_bindings(o));
        if let Some(cinst) = receiver
            .and_then(Receiver::class)
            .and_then(|id| self.class_insts.get(id))
            && let Some(class_tps) = self.class_type_params.get(cinst.class.index())
        {
            bindings.extend(
                class_tps
                    .iter()
                    .copied()
                    .zip(self.types.tuple_slice(cinst.args).iter().copied()),
            );
        }
        let Some(view) = self.index.view(template) else {
            return bindings;
        };
        let Some(tps) = self
            .template_type_params
            .get(template.index())
            .filter(|t| !t.is_empty())
            .cloned()
        else {
            return bindings;
        };
        for (&tp_id, &et) in tps.iter().zip(explicit_type_args) {
            if !self.types.is_open(et) {
                bindings.push((tp_id, et));
            }
        }
        self.unify_params(view.params, args, &tps, &mut bindings);
        bindings
    }

    pub(super) fn unify_params(
        &mut self,
        params: &[perry_hir::ir::Param],
        args: &[Ty],
        tps: &[shapes::TypeId],
        bindings: &mut Vec<(shapes::TypeId, Ty)>,
    ) {
        for skip_open in [true, false] {
            for (param, &arg_ty) in params.iter().zip(args) {
                if skip_open
                    && self
                        .types
                        .is_open_with(arg_ty, self.locations.as_slice(), &self.dict_reads)
                {
                    continue;
                }
                let pat = self.types.intern_hir(self.ctx, &param.ty, true);
                self.types.unify(self.ctx, pat, arg_ty, tps, bindings);
            }
        }
    }

    fn budget_error(
        &self,
        template: TemplateId,
        next_args: &[Ty],
        caller: Option<InstanceId>,
        call_byte_offset: u32,
    ) -> Diagnostic {
        let name = self.index.view(template).map_or_else(
            || "function".to_owned(),
            |v| {
                let n = hir_compat::display_name(v.name);
                if n.is_empty() || n.starts_with("callback_") {
                    "anonymous function".to_owned()
                } else {
                    n.to_owned()
                }
            },
        );
        let mut err = Diagnostic::unsupported(format!(
            "generic specialization limit of {INSTANCE_BUDGET} distinct argument lists reached for {name}"
        ));
        if let Some(caller_inst) = caller
            .and_then(|c| self.instances.get(c))
            .filter(|i| i.template == template)
        {
            let prev_args = self.types.tuple_slice(caller_inst.args);
            let view = self.index.view(template);
            for (idx, (&prev, &next)) in prev_args.iter().zip(next_args).enumerate() {
                if prev != next && self.types.contains_ty(next, prev) {
                    let param_name = view
                        .and_then(|v| {
                            v.params
                                .get(idx)
                                .and_then(|p| first_type_var_name(&p.ty))
                                .or_else(|| v.type_params.get(idx).map(|tp| tp.name.as_str()))
                                .or_else(|| v.params.get(idx).map(|p| p.name.as_str()))
                        })
                        .unwrap_or("T");
                    err = err.with_note(format!("this recursive call grows type argument {param_name} by nesting its previous type"));
                    break;
                }
            }
        }
        self.metadata.locations.annotate_call(err, call_byte_offset)
    }

    fn request_class_inst(
        &mut self,
        class: ClassId,
        args: &[Ty],
        byte_offset: u32,
        caller: Option<InstanceId>,
    ) -> Result<ClassInstId> {
        let args_key = self.types.tuple(args);
        let key = (class, args_key);
        if let Some(&id) = self.class_index.get(&key) {
            return Ok(id);
        }
        let cinfo = self
            .index
            .class(class)
            .ok_or_else(|| Diagnostic::internal("unknown class index"))?;
        let hir_class = self
            .hir
            .classes
            .get(cinfo.hir_idx)
            .ok_or_else(|| Diagnostic::internal("unknown class in HIR"))?;

        let bindings: Vec<_> = self
            .class_type_params
            .get(class.index())
            .into_iter()
            .flatten()
            .copied()
            .zip(args.iter().copied())
            .collect();
        let field_loc_start = self.field_locs.len() as u32;
        for field in &hir_class.fields {
            let is_func_ty = matches!(field.ty, HirType::Function(_));
            let ty = self.types.intern_hir(
                self.ctx,
                &field.ty,
                !matches!(
                    field.ty,
                    HirType::Any | HirType::Unknown | HirType::Function(_)
                ),
            );
            let ty = self.types.substitute(self.ctx, ty, &bindings);
            let declared = !is_func_ty && !self.types.is_open(ty);
            let loc = self.alloc_loc(if declared { ty } else { Ty::BOTTOM });
            if declared {
                self.declared_locs.insert(loc.index());
            }
            self.field_locs.push(loc);
        }
        let field_locs = TySpan {
            start: field_loc_start,
            len: hir_class.fields.len() as u32,
        };
        let next_idx = self.class_insts.len();
        let emitted_name = if !self.class_insts.iter().any(|c| c.class == class) {
            hir_class.name.clone()
        } else {
            format!("{}__inst_{next_idx}", hir_class.name)
        };

        let cinst_id = self.class_insts.push(ClassInst {
            class,
            args: args_key,
            field_locs,
            ctor_inst: None,
            emitted_name,
        });
        self.class_index.insert(key, cinst_id);

        let mut has_field_inits = false;
        let field_loc_slice: Vec<LocId> = self
            .field_locs
            .get(field_locs.range())
            .unwrap_or(&[])
            .to_vec();
        for (field, field_loc) in hir_class.fields.iter().zip(field_loc_slice) {
            if let Some(init) = &field.init {
                has_field_inits = true;
                if matches!(init, Expr::Array(items, _) if items.is_empty())
                    && self.types.is_open(self.locations[field_loc])
                {
                    let elem_loc = self.alloc_loc(Ty::BOTTOM);
                    let cell = self.types.cell(elem_loc, CellKind::Array);
                    self.write_loc(field_loc, cell);
                } else if let Some(init_inst) = self.init_inst {
                    let mut state = EvalState::new(init_inst, Some(Receiver::Class(cinst_id)));
                    let init_ty = self.eval_expr(&mut state, init)?;
                    self.check_escaping_func(init_inst, init_ty, 0, None)?;
                    let norm = self
                        .types
                        .normalize_for_storage(self.ctx, self.decls, init_ty);
                    self.write_loc(field_loc, norm);
                }
            }
        }
        if has_field_inits && let Some(init_inst) = self.init_inst {
            self.enqueue(init_inst);
        }
        if let Some(ctor_tid) = cinfo.ctor {
            let ctor_inst = self.request_instance(
                InstanceReq::new(ctor_tid, args, caller)
                    .receiver(Some(Receiver::Class(cinst_id)))
                    .at(byte_offset),
            )?;
            if let Some(ci) = self.class_insts.get_mut(cinst_id) {
                ci.ctor_inst = Some(ctor_inst);
            }
        }
        Ok(cinst_id)
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

    pub(super) fn callable_arg_mask_in(&mut self, reader: InstanceId, args: &[Ty]) -> u64 {
        let mut mask = 0u64;
        for (i, &ty) in args.iter().enumerate().take(64) {
            if let TyKind::Func(ident) = self.types.kind(ty)
                && !self.is_escaping_env(ident.template, reader)
            {
                mask |= 1u64 << i;
            }
        }
        mask
    }

    pub(super) fn mark_escaping_env(&mut self, _caller: InstanceId, ty: Ty) {
        let members = self.types.func_members(ty);
        for ident in members {
            let tid = ident.template;
            if ident.capture_owner.is_none()
                && self.index.capture_free_templates.contains(tid.index())
            {
                continue;
            }
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
            self.record_site_coercion(
                inst_id,
                expr,
                FuncCoercion::Widen {
                    from: src_ty,
                    to: dst_ty,
                },
            );
        }
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

    fn verify_contracts(&self) -> Result<()> {
        for c in &self.result_contracts {
            let inst = self.instances.get(c.inst).ok_or_else(|| {
                Diagnostic::internal("callback contract references missing instance")
            })?;
            let actual = *self.locations.get(inst.return_loc).ok_or_else(|| {
                Diagnostic::internal("callback contract references missing return location")
            })?;
            if !self.types.accepts(self.ctx, c.expected, actual) {
                return Err(self.metadata.locations.annotate_call(
                    diag!(
                        codes::TYPE_MISMATCH,
                        "callback result does not match the declared result of parameter '{}'",
                        c.param_name
                    ),
                    c.byte_offset,
                ));
            }
        }
        Ok(())
    }
}
