//! Worklist monomorphization and storage-join solver over dense indices.
mod calls;
mod eval;
mod expr;
use super::bitset::BitSet;
use super::boundary::{ClassInfo, ModuleIndex, SourceFacts, TemplateKind};
use super::builtins::{self, MethodResultArgs};
use super::types::{
    CellKind, ClassId, ClassInstId, FuncIdentity, InstanceId, LocId, LocalIdx, ObjectProp,
    TemplateId, Ty, TyKind, TySpan, TypeTable,
};
use crate::{
    Absence,
    hir::Foreign,
    hir_compat::{self, Metadata},
    narrowing::{DefinedGuard, Guards},
    shapes::{self, Declarations},
};
use eval::{callable_arg_mask, first_type_var_name};
use perry_hir::{
    ir::{CompareOp, Expr, LogicalOp, Module as HirModule, Stmt},
    types::{LocalId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};
use pliron::context::Context;
use std::collections::HashMap;

const INSTANCE_BUDGET: u16 = 256;
const MAX_WAVES: usize = 512;

pub(crate) struct Instance {
    pub(crate) template: TemplateId,
    pub(crate) receiver: Option<ClassInstId>,
    pub(crate) capture_owner: Option<InstanceId>,
    pub(crate) args: TySpan,
    pub(crate) explicit_type_args: TySpan,
    pub(crate) return_loc: LocId,
    pub(crate) explicit_return: Option<Ty>,
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
    pub(crate) args: TySpan,
    pub(crate) field_loc_base: u32,
    pub(crate) ctor_inst: Option<InstanceId>,
    pub(crate) emitted_name: String,
}

#[derive(Clone, Debug)]
pub(crate) enum SiteResolution {
    DirectCall {
        target: InstanceId,
        strip_arg_mask: u64,
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
    CollectionNew {
        is_map: bool,
        type_args: Vec<Ty>,
    },
    FoldBool(bool),
}

pub(crate) struct Solved {
    pub(crate) instances: Vec<Instance>,
    pub(crate) class_insts: Vec<ClassInst>,
    pub(crate) locations: Vec<Ty>,
    pub(crate) param_locs: Vec<LocId>,
    pub(crate) inst_local_tys: Vec<Vec<(LocalId, Ty)>>,
    pub(crate) inst_frame_locals: Vec<Vec<Ty>>,
    pub(crate) inst_sites: Vec<Vec<Option<SiteResolution>>>,
    pub(crate) inst_captures: Vec<Vec<LocalId>>,
    pub(crate) live_instances: BitSet,
    pub(crate) module_local_tys: Vec<Option<Ty>>,
    pub(crate) class_static_field_tys: Vec<Vec<Ty>>,
    pub(crate) class_inst_methods: Vec<Vec<InstanceId>>,
    pub(crate) class_inst_getters: Vec<Vec<(usize, InstanceId)>>,
    pub(crate) class_inst_setters: Vec<Vec<(usize, InstanceId)>>,
    pub(crate) class_static_methods: Vec<Vec<InstanceId>>,
    pub(crate) template_instances: Vec<Vec<InstanceId>>,
    pub(crate) dict_reads: BitSet,
    pub(crate) deletes_array_elements: bool,
}

type InstanceKey = (
    TemplateId,
    Option<ClassInstId>,
    Option<InstanceId>,
    Box<[Ty]>,
    Box<[Ty]>,
);

struct ResultContract {
    inst: InstanceId,
    expected: Ty,
    param_name: String,
    byte_offset: u32,
}

struct InstanceReq<'a> {
    template: TemplateId,
    receiver: Option<ClassInstId>,
    capture_owner: Option<InstanceId>,
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
            capture_owner: None,
            args,
            explicit_type_args: &[],
            call_byte_offset: 0,
            caller,
            is_inline_callback: false,
        }
    }
    fn receiver(mut self, r: Option<ClassInstId>) -> Self {
        self.receiver = r;
        self
    }
    fn owner(mut self, o: Option<InstanceId>) -> Self {
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
    receiver: Option<ClassInstId>,
    returned: Ty,
    has_return_value: bool,
    guards: Guards<'h>,
    iteration: hir_compat::Iteration,
    local_changed: bool,
}

impl EvalState<'_> {
    fn new(inst: InstanceId, receiver: Option<ClassInstId>) -> Self {
        Self {
            inst,
            receiver,
            returned: Ty::BOTTOM,
            has_return_value: false,
            guards: Guards::default(),
            iteration: hir_compat::Iteration::default(),
            local_changed: false,
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
    solver.seed_roots()?;
    solver.run_worklist()?;
    solver.verify_contracts()?;
    Ok(solver.finish())
}

struct Solver<'h, 'a> {
    ctx: &'a Context,
    hir: &'h HirModule,
    index: &'a ModuleIndex<'h>,
    metadata: &'a Metadata<'a>,
    facts: &'a SourceFacts,
    decls: &'a Declarations<'a>,
    types: &'a mut TypeTable,
    locations: Vec<Ty>,
    dirty_locs: BitSet,
    deps: Vec<(LocId, InstanceId)>,
    reader_offsets: Vec<u32>,
    reader_instances: Vec<InstanceId>,
    csr_dirty: bool,
    current_reads: BitSet,
    worklist: Vec<InstanceId>,
    worklist_head: usize,
    queued: BitSet,
    instances: Vec<Instance>,
    instance_args: Vec<Ty>,
    instance_type_args: Vec<Ty>,
    instance_index: HashMap<InstanceKey, InstanceId>,
    template_counts: Vec<u16>,
    param_locs: Vec<LocId>,
    inst_locals: Vec<Vec<InstanceLocalSlot>>,
    inst_frame_locals: Vec<Vec<Ty>>,
    inst_written: Vec<BitSet>,
    inst_captures: Vec<Vec<LocalId>>,
    inst_sites: Vec<Vec<Option<SiteResolution>>>,
    inst_refcount: Vec<u32>,
    inst_superseded: BitSet,
    dict_reads: BitSet,
    static_field_locs: Vec<(ClassId, usize, LocId)>,
    class_insts: Vec<ClassInst>,
    class_args: Vec<Ty>,
    class_index: HashMap<(ClassId, Box<[Ty]>), ClassInstId>,
    class_counts: Vec<u32>,
    template_type_params: Vec<Vec<shapes::TypeId>>,
    class_type_params: Vec<Vec<shapes::TypeId>>,
    result_contracts: Vec<ResultContract>,
    next_func_id: u32,
    deletes_array_elements: bool,
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
        let static_field_locs = index
            .classes
            .iter()
            .enumerate()
            .flat_map(|(cid_i, c)| {
                c.static_field_locs
                    .iter()
                    .enumerate()
                    .map(move |(f_idx, &(_, loc))| (ClassId(cid_i as u32), f_idx, loc))
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
            locations,
            dirty_locs: BitSet::default(),
            deps: Vec::new(),
            reader_offsets: Vec::new(),
            reader_instances: Vec::new(),
            csr_dirty: true,
            current_reads: BitSet::default(),
            worklist: Vec::new(),
            worklist_head: 0,
            queued: BitSet::default(),
            instances: Vec::new(),
            instance_args: Vec::new(),
            instance_type_args: Vec::new(),
            instance_index: HashMap::with_capacity(64),
            template_counts: vec![0; index.template_count()],
            param_locs: Vec::new(),
            inst_locals: Vec::new(),
            inst_frame_locals: Vec::new(),
            inst_written: Vec::new(),
            inst_captures: Vec::new(),
            inst_sites: Vec::new(),
            inst_refcount: Vec::new(),
            inst_superseded: BitSet::default(),
            dict_reads: BitSet::default(),
            static_field_locs,
            class_insts: Vec::new(),
            class_args: Vec::new(),
            class_index: HashMap::with_capacity(16),
            class_counts: vec![0; index.classes.len()],
            template_type_params,
            class_type_params,
            result_contracts: Vec::new(),
            next_func_id: index.max_func_id.saturating_add(1),
            deletes_array_elements: false,
        }
    }

    fn alloc_loc(&mut self, initial: Ty) -> LocId {
        let id = LocId(self.locations.len() as u32);
        self.locations.push(initial);
        id
    }

    fn read_loc(&mut self, loc: LocId, reader: InstanceId) -> Ty {
        debug_assert!((loc.0 as usize) < self.locations.len());
        if self.current_reads.insert(loc.0 as usize) {
            self.deps.push((loc, reader));
            self.csr_dirty = true;
        }
        self.locations[loc.0 as usize]
    }

    fn write_loc(&mut self, loc: LocId, value: Ty) -> bool {
        let Some(&old) = self.locations.get(loc.0 as usize) else {
            return false;
        };
        let next = self.types.join_bounded(old, value);
        if next == old {
            return false;
        }
        self.locations[loc.0 as usize] = next;
        self.dirty_locs.insert(loc.0 as usize);
        true
    }

    fn is_non_site_root(&self, inst_id: InstanceId) -> bool {
        let Some(inst) = self.instances.get(inst_id.0 as usize) else {
            return false;
        };
        inst.caller.is_none()
            || self.index.view(inst.template).is_some_and(|v| {
                matches!(
                    v.kind,
                    TemplateKind::ClassConstructor { .. }
                        | TemplateKind::ClassGetter { .. }
                        | TemplateKind::ClassSetter { .. }
                )
            })
            || self.result_contracts.iter().any(|c| c.inst == inst_id)
    }

    fn site_target(&self, res: &SiteResolution) -> Option<InstanceId> {
        match *res {
            SiteResolution::DirectCall { target, .. }
            | SiteResolution::MethodCall { target, .. }
            | SiteResolution::Callback { target, .. } => Some(target),
            SiteResolution::ClassNew { class_inst, .. } => {
                self.class_insts.get(class_inst.0 as usize)?.ctor_inst
            }
            _ => None,
        }
    }

    fn release_site_target(&mut self, caller: InstanceId, target: InstanceId) {
        if target == caller {
            return;
        }
        let idx = target.0 as usize;
        if let Some(rc) = self.inst_refcount.get_mut(idx) {
            *rc = rc.saturating_sub(1);
            if *rc > 0 {
                return;
            }
        }
        if self.is_non_site_root(target) || !self.inst_superseded.insert(idx) {
            return;
        }
        if let Some(inst) = self.instances.get(idx)
            && let Some(slot) = self.template_counts.get_mut(inst.template.0 as usize)
        {
            *slot = slot.saturating_sub(1);
        }
        let outgoing: Vec<InstanceId> = self
            .inst_sites
            .get(idx)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|r| self.site_target(r))
            .collect();
        if let Some(sites) = self.inst_sites.get_mut(idx) {
            sites.fill(None);
        }
        for child in outgoing {
            self.release_site_target(target, child);
        }
    }

    fn retain_site_target(&mut self, caller: InstanceId, target: InstanceId) {
        if target == caller {
            return;
        }
        let idx = target.0 as usize;
        if let Some(rc) = self.inst_refcount.get_mut(idx) {
            *rc = rc.saturating_add(1);
        }
        if self.inst_superseded.remove(idx) {
            if let Some(inst) = self.instances.get(idx)
                && let Some(slot) = self.template_counts.get_mut(inst.template.0 as usize)
            {
                *slot = slot.saturating_add(1);
            }
            self.enqueue(target);
        }
    }

    fn enqueue(&mut self, inst: InstanceId) {
        if !self.inst_superseded.contains(inst.0 as usize) && self.queued.insert(inst.0 as usize) {
            self.worklist.push(inst);
        }
    }

    fn rebuild_csr(&mut self) {
        self.reader_offsets.clear();
        self.reader_offsets.resize(self.locations.len() + 1, 0);
        for &(loc, _) in &self.deps {
            if let Some(slot) = self.reader_offsets.get_mut(loc.0 as usize + 1) {
                *slot = slot.saturating_add(1);
            }
        }
        for i in 1..self.reader_offsets.len() {
            self.reader_offsets[i] =
                self.reader_offsets[i].saturating_add(self.reader_offsets[i - 1]);
        }
        let total = self.reader_offsets.last().copied().unwrap_or(0) as usize;
        self.reader_instances.clear();
        self.reader_instances.resize(total, InstanceId(0));
        let mut cursor = self.reader_offsets.clone();
        for &(loc, reader) in &self.deps {
            if let Some(pos) = cursor.get_mut(loc.0 as usize) {
                let idx = *pos as usize;
                *pos = pos.saturating_add(1);
                if let Some(slot) = self.reader_instances.get_mut(idx) {
                    *slot = reader;
                }
            }
        }
        self.csr_dirty = false;
    }

    fn seed_roots(&mut self) -> Result<()> {
        let init_inst = self.request_instance(InstanceReq::new(TemplateId(0), &[], None))?;
        self.enqueue(init_inst);
        for tid_idx in 1..self.index.template_count() {
            let tid = TemplateId(tid_idx as u32);
            let Some(view) = self.index.view(tid) else {
                continue;
            };
            if !self.index.template_exported.contains(tid_idx)
                && !self.index.template_is_obj_method.contains(tid_idx)
            {
                continue;
            }
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
        Ok(())
    }

    fn default_type_param_bindings(&mut self, tid: TemplateId) -> Vec<(shapes::TypeId, Ty)> {
        let Some(view) = self.index.view(tid) else {
            return Vec::new();
        };
        let Some(tps) = self.template_type_params.get(tid.0 as usize) else {
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
            TyKind::Leaf(id) => {
                if let HirType::Named(name) = id.hir(self.ctx) {
                    let nid = self.types.intern_name(name);
                    if let Some(cid) = self.index.class_by_name_id(nid) {
                        let cinst = self.request_class_inst(cid, &[], byte_offset, caller)?;
                        return Ok(self.types.class_inst(cinst));
                    }
                }
                Ok(ty)
            }
            TyKind::Generic { base, args } => {
                if let HirType::Named(name) = base.hir(self.ctx) {
                    let nid = self.types.intern_name(name);
                    if let Some(cid) = self.index.class_by_name_id(nid) {
                        let mut resolved = Vec::with_capacity(args.len as usize);
                        for a in self.types.ty_slice(args).to_vec() {
                            resolved.push(self.instantiate_annotated_class(
                                a,
                                byte_offset,
                                caller,
                            )?);
                        }
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
            matches!(v.kind, TemplateKind::Function { .. })
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
        let key: InstanceKey = (
            template,
            receiver,
            capture_owner,
            args.into(),
            explicit_type_args.into(),
        );
        if let Some(&id) = self.instance_index.get(&key) {
            if is_inline_callback && let Some(inst) = self.instances.get_mut(id.0 as usize) {
                inst.is_inline_callback = true;
            }
            if self.inst_superseded.remove(id.0 as usize) {
                if let Some(slot) = self.template_counts.get_mut(template.0 as usize) {
                    *slot = slot.saturating_add(1);
                }
                self.enqueue(id);
            }
            return Ok(id);
        }

        let count = self
            .template_counts
            .get(template.0 as usize)
            .copied()
            .unwrap_or(0);
        if count >= INSTANCE_BUDGET {
            return Err(self.budget_error(template, args, caller, call_byte_offset));
        }
        if let Some(slot) = self.template_counts.get_mut(template.0 as usize) {
            *slot = slot.saturating_add(1);
        }

        let view = self
            .index
            .view(template)
            .ok_or_else(|| Diagnostic::internal("inference engine requested unknown template"))?;
        let inst_id = InstanceId(self.instances.len() as u32);
        let arg_span = TySpan {
            start: self.instance_args.len() as u32,
            len: args.len() as u32,
        };
        self.instance_args.extend_from_slice(args);
        let explicit_type_args_span = TySpan {
            start: self.instance_type_args.len() as u32,
            len: explicit_type_args.len() as u32,
        };
        self.instance_type_args
            .extend_from_slice(explicit_type_args);

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
            && let Some(own_tps) = self.template_type_params.get(template.0 as usize)
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
                        err = self.attach_instantiation_chain(err, cid);
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
                if args.len() > fixed_count {
                    let elem = args[fixed_count..].iter().fold(Ty::BOTTOM, |acc, &x| {
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
            }
        }
        let param_locs = TySpan {
            start: param_loc_start,
            len: view.params.len() as u32,
        };

        let explicit_return = self
            .index
            .template_explicit_return
            .contains(template.0 as usize)
            .then(|| {
                let ret = self.types.intern_hir(self.ctx, view.return_type, true);
                let ret = self.types.substitute(self.ctx, ret, &type_bindings);
                (!self.types.is_open(ret)).then_some(ret)
            })
            .flatten();
        let return_loc = self.alloc_loc(explicit_return.unwrap_or(Ty::BOTTOM));

        let keep_original = count == 0
            && (view.is_exported
                || self
                    .index
                    .template_is_obj_method
                    .contains(template.0 as usize)
                || is_monomorphic_plain);
        let emitted_func_id = if keep_original {
            view.func_id
        } else {
            let id = self.next_func_id;
            self.next_func_id = self.next_func_id.saturating_add(1);
            id
        };
        let emitted_name = if template == TemplateId(0) || keep_original {
            view.raw_name()
        } else {
            format!("{}_instance_{emitted_func_id}", view.raw_name())
        };
        let emitted_method_name = match view.kind {
            TemplateKind::ClassMethod { .. }
            | TemplateKind::ClassStaticMethod { .. }
            | TemplateKind::ClassGetter { .. }
            | TemplateKind::ClassSetter { .. } => {
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

        self.instances.push(Instance {
            template,
            receiver,
            capture_owner,
            args: arg_span,
            explicit_type_args: explicit_type_args_span,
            return_loc,
            explicit_return,
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
            .push(vec![None; self.index.site_count(template)]);
        self.inst_refcount.push(0);
        self.enqueue(inst_id);
        Ok(inst_id)
    }

    fn compute_type_param_bindings(
        &mut self,
        template: TemplateId,
        receiver: Option<ClassInstId>,
        capture_owner: Option<InstanceId>,
        args: &[Ty],
        explicit_type_args: &[Ty],
    ) -> Vec<(shapes::TypeId, Ty)> {
        let mut bindings =
            capture_owner.map_or_else(Vec::new, |o| self.active_instance_type_bindings(o));
        if let Some(cinst) = receiver.and_then(|id| self.class_insts.get(id.0 as usize))
            && let Some(class_tps) = self.class_type_params.get(cinst.class.0 as usize)
        {
            bindings.extend(
                class_tps.iter().copied().zip(
                    self.class_args
                        .get(cinst.args.range())
                        .unwrap_or(&[])
                        .iter()
                        .copied(),
                ),
            );
        }
        let Some(view) = self.index.view(template) else {
            return bindings;
        };
        let Some(tps) = self
            .template_type_params
            .get(template.0 as usize)
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
        for (param, &arg_ty) in view.params.iter().zip(args) {
            let pat = self.types.intern_hir(self.ctx, &param.ty, true);
            self.types.unify(self.ctx, pat, arg_ty, &tps, &mut bindings);
        }
        bindings
    }

    fn budget_error(
        &self,
        template: TemplateId,
        next_args: &[Ty],
        caller: Option<InstanceId>,
        call_byte_offset: u32,
    ) -> Diagnostic {
        let name = self
            .index
            .view(template)
            .map_or("function", |v| hir_compat::display_name(v.name));
        let mut err = Diagnostic::unsupported(format!(
            "generic specialization limit of {INSTANCE_BUDGET} distinct argument lists reached for {name}"
        ));
        if let Some(caller_inst) = caller
            .and_then(|c| self.instances.get(c.0 as usize))
            .filter(|i| i.template == template)
        {
            let prev_args = self
                .instance_args
                .get(caller_inst.args.range())
                .unwrap_or(&[]);
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
        let key: (ClassId, Box<[Ty]>) = (class, args.into());
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
        let cinst_id = ClassInstId(self.class_insts.len() as u32);
        let arg_span = TySpan {
            start: self.class_args.len() as u32,
            len: args.len() as u32,
        };
        self.class_args.extend_from_slice(args);

        let bindings: Vec<_> = self
            .class_type_params
            .get(class.0 as usize)
            .into_iter()
            .flatten()
            .copied()
            .zip(args.iter().copied())
            .collect();
        let field_loc_base = self.locations.len() as u32;
        for field in &hir_class.fields {
            let ty = self.types.intern_hir(
                self.ctx,
                &field.ty,
                !matches!(field.ty, HirType::Any | HirType::Unknown),
            );
            let ty = self.types.substitute(self.ctx, ty, &bindings);
            self.alloc_loc(if self.types.is_open(ty) {
                Ty::BOTTOM
            } else {
                ty
            });
        }
        let count = self
            .class_counts
            .get(class.0 as usize)
            .copied()
            .unwrap_or(0);
        if let Some(slot) = self.class_counts.get_mut(class.0 as usize) {
            *slot = slot.saturating_add(1);
        }
        let emitted_name = if count == 0 {
            hir_class.name.clone()
        } else {
            format!("{}__inst_{}", hir_class.name, cinst_id.0)
        };

        self.class_insts.push(ClassInst {
            class,
            args: arg_span,
            field_loc_base,
            ctor_inst: None,
            emitted_name,
        });
        self.class_index.insert(key, cinst_id);

        for (f_idx, field) in hir_class.fields.iter().enumerate() {
            if let Some(init) = &field.init {
                let field_loc = LocId(field_loc_base + f_idx as u32);
                if matches!(init, Expr::Array(items, _) if items.is_empty())
                    && self.types.is_open(self.locations[field_loc.0 as usize])
                {
                    let elem_loc = self.alloc_loc(Ty::BOTTOM);
                    let cell = self.types.array_cell(elem_loc);
                    self.write_loc(field_loc, cell);
                } else {
                    let mut state = EvalState::new(InstanceId(0), Some(cinst_id));
                    let init_ty = self.eval_expr(&mut state, init)?;
                    let norm = self
                        .types
                        .normalize_for_storage(self.ctx, self.decls, init_ty);
                    self.write_loc(field_loc, norm);
                }
            }
        }
        if let Some(ctor_tid) = cinfo.ctor {
            let ctor_inst = self.request_instance(
                InstanceReq::new(ctor_tid, args, caller)
                    .receiver(Some(cinst_id))
                    .at(byte_offset),
            )?;
            if let Some(ci) = self.class_insts.get_mut(cinst_id.0 as usize) {
                ci.ctor_inst = Some(ctor_inst);
            }
        }
        Ok(cinst_id)
    }

    fn run_worklist(&mut self) -> Result<()> {
        for _ in 0..MAX_WAVES {
            let wave_end = self.worklist.len();
            while self.worklist_head < wave_end {
                let Some(&inst_id) = self.worklist.get(self.worklist_head) else {
                    break;
                };
                self.worklist_head += 1;
                self.queued.remove(inst_id.0 as usize);
                if !self.inst_superseded.contains(inst_id.0 as usize) {
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
                        if !self.inst_superseded.contains(r.0 as usize)
                            && self.queued.insert(r.0 as usize)
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
        Err(Diagnostic::unsupported(
            "type inference did not converge within the worklist wave limit",
        ))
    }

    fn verify_contracts(&self) -> Result<()> {
        for c in &self.result_contracts {
            let inst = self.instances.get(c.inst.0 as usize).ok_or_else(|| {
                Diagnostic::internal("callback contract references missing instance")
            })?;
            let actual = *self
                .locations
                .get(inst.return_loc.0 as usize)
                .ok_or_else(|| {
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
