//! Worklist monomorphization and storage-join solver over dense indices.
//!
//! Layout:
//! - One `locations: Vec<Ty>` of monotone joins across locals, module bindings, class fields,
//!   empty aggregates, and instance returns.
//! - One CSR compressed reader adjacency `(reader_offsets, reader_instances)` built from recorded
//!   read dependencies `(LocId, InstanceId)`.
//! - One `worklist: Vec<InstanceId>` guarded by a `queued: BitSet`.
//! - One `FlatHashIndex` over `instances` + `instance_args` for `(TemplateId, receiver, args)`.
use super::bitset::BitSet;
use super::boundary::{AggregateKind, ModuleIndex, SourceFacts, TemplateKind};
use super::builtins::{self, CallbackContract, MethodResultArgs};
use super::types::{
    ClassId, ClassInstId, FlatHashIndex, FuncIdentity, InstanceId, LocId, LocalIdx, ObjectProp,
    TemplateId, Ty, TyKind, TySpan, TypeTable,
};
use crate::{
    Absence,
    hir::Foreign,
    hir_compat::{self, Metadata},
    narrowing::{DefinedGuard, Guards},
    shapes::{self, Declarations},
};
use perry_hir::{
    ir::{CompareOp, Expr, LogicalOp, Module as HirModule, Stmt},
    types::{LocalId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};
use pliron::context::Context;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

const INSTANCE_BUDGET: u16 = 256;
const MAX_WAVES: usize = 512;

/// Concrete monomorphic function/closure/method instance.
pub(crate) struct Instance {
    pub(crate) template: TemplateId,
    pub(crate) receiver: Option<ClassInstId>,
    pub(crate) capture_owner: Option<InstanceId>,
    pub(crate) args: TySpan,
    pub(crate) return_loc: LocId,
    pub(crate) explicit_return: Option<Ty>,
    pub(crate) param_locs: TySpan,
    pub(crate) caller: Option<InstanceId>,
    pub(crate) call_byte_offset: u32,
    pub(crate) is_inline_callback: bool,
    pub(crate) captures_non_global: bool,
    pub(crate) emitted_func_id: u32,
    pub(crate) emitted_name: String,
    /// For class methods with multiple specializations, the method name on the emitted class.
    pub(crate) emitted_method_name: String,
}

/// Concrete monomorphic class instantiation `(ClassId, ctor/type args)`.
pub(crate) struct ClassInst {
    pub(crate) class: ClassId,
    pub(crate) args: TySpan,
    pub(crate) field_loc_base: u32,
    pub(crate) field_count: u32,
    pub(crate) ctor_inst: Option<InstanceId>,
    pub(crate) emitted_name: String,
}

/// Resolution recorded for an expression address within an instance, consumed by `emit.rs`.
#[derive(Clone, Debug)]
pub(crate) enum SiteResolution {
    /// Direct call to a specialized function/closure instance (with static callable arg mask).
    DirectCall {
        target: InstanceId,
        strip_arg_mask: u64,
    },
    /// Call to a specialized class method on `receiver`.
    MethodCall {
        target: InstanceId,
        strip_arg_mask: u64,
    },
    /// Built-in callback (`map`, `forEach`, `reduce`, `sort`, `replace`, `Array.from`, etc.).
    Callback {
        target: InstanceId,
        keep_inline_closure: bool,
    },
    /// `new Class(...)` specialized to a concrete `ClassInstId`.
    ClassNew {
        class_inst: ClassInstId,
        strip_arg_mask: u64,
    },
    /// Materialized `new Map<K, V>` / `new Set<T>` constructor.
    CollectionNew { is_map: bool, type_args: Vec<Ty> },
    /// Folded static function identity comparison (`f === double`).
    FoldBool(bool),
    /// Static function alias/parameter read replaced by a function reference (or void if stripped).
    FuncAlias(InstanceId),
}

/// Output of the solver handed to `emit.rs` after borrowing of `&HirModule` ends.
pub(crate) struct Solved {
    pub(crate) instances: Vec<Instance>,
    pub(crate) instance_args: Vec<Ty>,
    pub(crate) class_insts: Vec<ClassInst>,
    pub(crate) locations: Vec<Ty>,
    /// Per-instance parameter `LocId`s (indexed by `Instance::param_locs`).
    pub(crate) param_locs: Vec<LocId>,
    /// Per-instance local bindings `(InstanceId, LocalId, Ty)`.
    pub(crate) instance_locals: Vec<(InstanceId, LocalId, Ty)>,
    /// Sorted per-instance site resolutions `(InstanceId, expr_addr, SiteResolution)`.
    pub(crate) sites: Vec<(InstanceId, usize, SiteResolution)>,
    /// Transitive non-global captured `LocalId`s per instance `(InstanceId, LocalId)`.
    pub(crate) instance_captures: Vec<(InstanceId, LocalId)>,
    /// Module-level resolved binding types `(LocalId, Ty)`.
    pub(crate) module_bindings: Vec<(LocalId, Ty)>,
    /// Class static field resolved types `(ClassId, field_idx, Ty)`.
    pub(crate) static_fields: Vec<(ClassId, usize, Ty)>,
    /// Class instance counts per `ClassId` (1 keeps original name, >1 mangles + records origin).
    pub(crate) class_inst_counts: Vec<u32>,
    /// Whether `delete` targeted an array-typed expression (making module arrays potentially sparse).
    pub(crate) deletes_array_elements: bool,
}

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
    call_byte_offset: u32,
    caller: Option<InstanceId>,
    is_inline_callback: bool,
}

#[derive(Default)]
struct EvalState<'h> {
    guards: Guards<'h>,
    iteration: hir_compat::Iteration,
}

struct InstanceLocalSlot {
    local: LocalIdx,
    loc: LocId,
}

struct AggregateSlot {
    local: LocalIdx,
    loc: LocId,
    kind: AggregateKind,
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

    /// Monotone join table indexed by `LocId`.
    locations: Vec<Ty>,
    dirty_locs: BitSet,

    /// Dependency edges `(LocId, reader InstanceId)` and CSR compressed adjacency.
    deps: Vec<(LocId, InstanceId)>,
    reader_offsets: Vec<u32>,
    reader_instances: Vec<InstanceId>,
    csr_dirty: bool,
    current_reads: BitSet,

    /// Worklist queue + membership bitset.
    worklist: Vec<InstanceId>,
    worklist_head: usize,
    queued: BitSet,

    /// Function/method/closure instances.
    instances: Vec<Instance>,
    instance_args: Vec<Ty>,
    instance_index: FlatHashIndex,
    template_counts: Vec<u16>,
    param_locs: Vec<LocId>,

    /// Per-instance local slots and empty-aggregate slots (indexed by `InstanceId`).
    inst_locals: Vec<Vec<InstanceLocalSlot>>,
    inst_aggs: Vec<Vec<AggregateSlot>>,
    inst_written: Vec<BitSet>,
    inst_captures: Vec<Vec<LocalId>>,

    /// Module-level aggregate slots (indexed by `LocalIdx`).
    module_aggs: Vec<Option<(LocId, AggregateKind)>>,
    /// Class static field `LocId`s `(ClassId, field_idx, LocId)`.
    static_field_locs: Vec<(ClassId, usize, LocId)>,

    /// Class instances `(ClassId, args) -> ClassInstId`.
    class_insts: Vec<ClassInst>,
    class_args: Vec<Ty>,
    class_index: FlatHashIndex,
    class_counts: Vec<u32>,
    /// Type-param `shapes::TypeId` cache per template / class (computed once at start).
    template_type_params: Vec<Vec<shapes::TypeId>>,
    class_type_params: Vec<Vec<shapes::TypeId>>,

    /// Recorded per-instance expression resolutions.
    sites: Vec<(InstanceId, usize, SiteResolution)>,
    /// Deferred callback result contract checks.
    result_contracts: Vec<ResultContract>,
    /// Next fresh `FuncId` for emitted instances.
    next_func_id: u32,
    /// Whether `delete` targeted an array-typed value.
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
        // Ensure module-level empty-array / empty-object aggregate bindings start with BOTTOM element loc.
        let mut module_aggs = vec![None; index.local_count()];
        for (i, agg) in index.local_empty_agg.iter().enumerate() {
            if index.local_module.contains(i)
                && let Some(slot) = module_aggs.get_mut(i)
            {
                *slot = *agg;
            }
        }
        let mut static_field_locs = Vec::new();
        for (loc_i, kind) in index.locations.iter().enumerate() {
            if let super::boundary::LocationKind::StaticField { class, field_idx } = kind {
                static_field_locs.push((*class, *field_idx, LocId(loc_i as u32)));
            }
        }
        // Keep `locations` non-empty.
        if locations.is_empty() {
            locations.push(Ty::VOID);
        }

        let template_type_params = index
            .views
            .iter()
            .map(|view| {
                view.type_params
                    .iter()
                    .map(|tp| shapes::TypeId::from_owned(ctx, HirType::Named(tp.name.clone())))
                    .collect()
            })
            .collect();
        let class_type_params = index
            .class_hir_idx
            .iter()
            .map(|&hir_idx| {
                hir.classes
                    .get(hir_idx)
                    .map(|c| {
                        c.type_params
                            .iter()
                            .map(|tp| {
                                shapes::TypeId::from_owned(ctx, HirType::Named(tp.name.clone()))
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect();

        let next_func_id = index.max_func_id.saturating_add(1);
        let template_count = index.template_count();
        let class_count = index.class_hir_idx.len();

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
            instance_index: FlatHashIndex::with_capacity(64),
            template_counts: vec![0; template_count],
            param_locs: Vec::new(),
            inst_locals: Vec::new(),
            inst_aggs: Vec::new(),
            inst_written: Vec::new(),
            inst_captures: Vec::new(),
            module_aggs,
            static_field_locs,
            class_insts: Vec::new(),
            class_args: Vec::new(),
            class_index: FlatHashIndex::with_capacity(16),
            class_counts: vec![0; class_count],
            template_type_params,
            class_type_params,
            sites: Vec::new(),
            result_contracts: Vec::new(),
            next_func_id,
            deletes_array_elements: false,
        }
    }

    fn alloc_loc(&mut self, initial: Ty) -> LocId {
        let id = LocId(self.locations.len() as u32);
        self.locations.push(initial);
        id
    }

    fn read_loc(&mut self, loc: LocId, reader: InstanceId) -> Ty {
        if self.current_reads.insert(loc.0 as usize) {
            self.deps.push((loc, reader));
            self.csr_dirty = true;
        }
        self.locations
            .get(loc.0 as usize)
            .copied()
            .unwrap_or(Ty::BOTTOM)
    }

    fn write_loc(&mut self, loc: LocId, value: Ty) -> bool {
        if value == Ty::BOTTOM || value == Ty::NEVER {
            return false;
        }
        let Some(&old) = self.locations.get(loc.0 as usize) else {
            return false;
        };
        let next = self.types.join(old, value);
        if next != old {
            if let Some(slot) = self.locations.get_mut(loc.0 as usize) {
                *slot = next;
            }
            self.dirty_locs.insert(loc.0 as usize);
            true
        } else {
            false
        }
    }

    fn enqueue(&mut self, inst: InstanceId) {
        if self.queued.insert(inst.0 as usize) {
            self.worklist.push(inst);
        }
    }

    fn rebuild_csr(&mut self) {
        let loc_count = self.locations.len();
        self.reader_offsets.clear();
        self.reader_offsets.resize(loc_count + 1, 0);
        for &(loc, _) in &self.deps {
            if let Some(slot) = self.reader_offsets.get_mut(loc.0 as usize + 1) {
                *slot = slot.saturating_add(1);
            }
        }
        for i in 1..self.reader_offsets.len() {
            let prev = self.reader_offsets.get(i - 1).copied().unwrap_or(0);
            if let Some(cur) = self.reader_offsets.get_mut(i) {
                *cur = cur.saturating_add(prev);
            }
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
        // Root 0: ModuleInit (TemplateId(0)).
        let init_inst = self.request_instance(InstanceReq {
            template: TemplateId(0),
            receiver: None,
            capture_owner: None,
            args: &[],
            call_byte_offset: 0,
            caller: None,
            is_inline_callback: false,
        })?;
        self.enqueue(init_inst);

        // Roots: every exported top-level function (and WIT object-export `__obj_method_*` method)
        // instantiated at its declared parameter types.
        let template_count = self.index.template_count();
        for tid_idx in 1..template_count {
            let tid = TemplateId(tid_idx as u32);
            let Some(view) = self.index.view(tid) else {
                continue;
            };
            let is_root = self.index.template_exported.contains(tid_idx)
                || view.name.starts_with("__obj_method_");
            if !is_root {
                continue;
            }
            // Bind any constrained/defaulted type params on an exported function.
            let type_bindings = self.default_type_param_bindings(tid);
            let mut arg_tys = Vec::with_capacity(view.params.len());
            for param in view.params {
                let explicit = self
                    .hir
                    .local_source_spans
                    .get(&param.id)
                    .is_some_and(|span| self.facts.is_param_annotated(span.start));
                let mut ty = self.types.intern_hir(self.ctx, &param.ty, explicit);
                ty = self.types.substitute(self.ctx, ty, &type_bindings);
                // If an exported parameter is annotated with a generic user class `Box<number>`,
                // instantiate that class root now.
                ty = self.instantiate_annotated_class(ty, 0, None)?;
                arg_tys.push(ty);
            }
            let inst = self.request_instance(InstanceReq {
                template: tid,
                receiver: None,
                capture_owner: None,
                args: &arg_tys,
                call_byte_offset: 0,
                caller: None,
                is_inline_callback: false,
            })?;
            self.enqueue(inst);
        }
        Ok(())
    }

    fn default_type_param_bindings(&mut self, tid: TemplateId) -> Vec<(shapes::TypeId, Ty)> {
        let Some(view) = self.index.view(tid) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (i, tp) in view.type_params.iter().enumerate() {
            if let Some(bound) = tp.constraint.as_deref().or(tp.default.as_deref())
                && let Some(&tp_id) = self
                    .template_type_params
                    .get(tid.0 as usize)
                    .and_then(|v| v.get(i))
            {
                let bound_ty = self.types.intern_hir(self.ctx, bound, true);
                out.push((tp_id, bound_ty));
            }
        }
        out
    }

    /// Resolve a user-class type annotation (`Named("C")` or `Generic { base: "Box", args: [Number] }`)
    /// into its `TyKind::ClassInst(ClassInstId)` so methods/fields on annotated params work uniformly.
    fn instantiate_annotated_class(
        &mut self,
        ty: Ty,
        byte_offset: u32,
        caller: Option<InstanceId>,
    ) -> Result<Ty> {
        match self.types.kind(ty) {
            TyKind::Leaf(id) => {
                if let HirType::Named(name) = id.hir(self.ctx)
                    && let Some(class_id) = self.index.class_by_name(name)
                {
                    let cinst = self.request_class_inst(class_id, &[], byte_offset, caller)?;
                    return Ok(self.types.class_inst(cinst));
                }
                Ok(ty)
            }
            TyKind::Generic { base, args } => {
                if let HirType::Named(name) = base.hir(self.ctx)
                    && let Some(class_id) = self.index.class_by_name(name)
                {
                    let arg_vec: Vec<Ty> = self.types.generic_slice(args).to_vec();
                    let mut resolved = Vec::with_capacity(arg_vec.len());
                    for a in arg_vec {
                        resolved.push(self.instantiate_annotated_class(a, byte_offset, caller)?);
                    }
                    let cinst =
                        self.request_class_inst(class_id, &resolved, byte_offset, caller)?;
                    return Ok(self.types.class_inst(cinst));
                }
                Ok(ty)
            }
            TyKind::Array(inner) => {
                let next = self.instantiate_annotated_class(inner, byte_offset, caller)?;
                Ok(if next == inner {
                    ty
                } else {
                    self.types.array(next)
                })
            }
            TyKind::Promise(inner) => {
                let next = self.instantiate_annotated_class(inner, byte_offset, caller)?;
                Ok(if next == inner {
                    ty
                } else {
                    self.types.promise(next)
                })
            }
            TyKind::Union(span) => {
                let members: Vec<Ty> = self.types.union_slice(span).to_vec();
                let mut changed = false;
                let mut out = Vec::with_capacity(members.len());
                for m in members {
                    let next = self.instantiate_annotated_class(m, byte_offset, caller)?;
                    changed |= next != m;
                    out.push(next);
                }
                Ok(if changed { self.types.union(&out) } else { ty })
            }
            _ => Ok(ty),
        }
    }

    fn hash_instance_key(
        template: TemplateId,
        receiver: Option<ClassInstId>,
        capture_owner: Option<InstanceId>,
        args: &[Ty],
    ) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        template.hash(&mut h);
        receiver.hash(&mut h);
        capture_owner.hash(&mut h);
        args.hash(&mut h);
        h.finish()
    }

    fn request_instance(&mut self, req: InstanceReq<'_>) -> Result<InstanceId> {
        let InstanceReq {
            template,
            receiver,
            capture_owner,
            args,
            call_byte_offset,
            caller,
            is_inline_callback,
        } = req;
        let hash = Self::hash_instance_key(template, receiver, capture_owner, args);
        if let Some(id) = self.instance_index.find(hash, |idx| {
            self.instances.get(idx as usize).is_some_and(|inst| {
                inst.template == template
                    && inst.receiver == receiver
                    && inst.capture_owner == capture_owner
                    && self.instance_args.get(inst.args.range()) == Some(args)
            })
        }) {
            if is_inline_callback && let Some(inst) = self.instances.get_mut(id as usize) {
                inst.is_inline_callback = true;
            }
            return Ok(InstanceId(id));
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
        let arg_start = self.instance_args.len() as u32;
        self.instance_args.extend_from_slice(args);
        let arg_span = TySpan {
            start: arg_start,
            len: args.len() as u32,
        };

        // Allocate parameter locations and bind incoming argument tuple (fixed / default / rest / missing).
        let param_loc_start = self.param_locs.len() as u32;
        let mut local_slots = Vec::with_capacity(view.params.len() + 4);
        let mut captures_non_global = capture_owner.is_some();
        let mut inst_caps = Vec::new();
        if let Some(owner) = capture_owner {
            for &cap in view.captures {
                if let Some(lidx) = self.index.local_idx(cap)
                    && !self.index.local_module.contains(lidx.0 as usize)
                {
                    captures_non_global = true;
                    inst_caps.push(cap);
                }
            }
            let _ = owner;
        }
        // Any static function argument whose template captures non-global locals makes this
        // instance inline too (transitively preserving caller-frame locals).
        for &arg in args {
            if let TyKind::Func(ident) = self.types.kind(arg)
                && let Some(cb_view) = self.index.view(ident.template)
            {
                for &cap in cb_view.captures {
                    if let Some(lidx) = self.index.local_idx(cap)
                        && !self.index.local_module.contains(lidx.0 as usize)
                    {
                        captures_non_global = true;
                        if !inst_caps.contains(&cap) {
                            inst_caps.push(cap);
                        }
                    }
                }
            }
        }

        // Compute explicit type-parameter substitutions for generic functions/methods/classes.
        let mut type_bindings = self.compute_type_param_bindings(template, receiver, args);
        if view.is_exported && type_bindings.is_empty() {
            type_bindings = self.default_type_param_bindings(template);
        }

        let fixed_count = view
            .params
            .iter()
            .position(|p| p.is_rest)
            .unwrap_or(view.params.len());

        for (p_idx, param) in view.params.iter().enumerate() {
            let explicit = self
                .hir
                .local_source_spans
                .get(&param.id)
                .is_some_and(|span| self.facts.is_param_annotated(span.start));
            let declared = self.types.intern_hir(self.ctx, &param.ty, explicit);
            let declared = self.types.substitute(self.ctx, declared, &type_bindings);
            let bound_ty = if param.is_rest {
                if args.len() > fixed_count {
                    let mut elem = Ty::BOTTOM;
                    for &extra in &args[fixed_count..] {
                        let norm = self
                            .types
                            .normalize_for_storage(self.ctx, self.decls, extra);
                        elem = self.types.join(elem, norm);
                    }
                    self.types.array(elem)
                } else if !self.types.is_open(declared) {
                    declared
                } else {
                    self.types.array(Ty::NEVER)
                }
            } else {
                let incoming = args.get(p_idx).copied().unwrap_or(Ty::VOID);
                if incoming != Ty::OPEN && incoming != Ty::BOTTOM {
                    if matches!(self.types.kind(incoming), TyKind::Func(_)) {
                        incoming
                    } else if param.default.is_some() {
                        // Parameter defaults narrow `undefined` when evaluated; body evaluation
                        // joins the default expression type if `incoming` admits `undefined`.
                        let stripped = self.types.exclude_absences(incoming, Absence::Undefined);
                        if stripped == Ty::NEVER {
                            // Omitted or literal `undefined`: default expression will supply the type.
                            if !self.types.is_open(declared) {
                                declared
                            } else {
                                Ty::BOTTOM
                            }
                        } else {
                            stripped
                        }
                    } else if !self.types.is_open(declared)
                        && !matches!(self.types.kind(declared), TyKind::Leaf(id) if matches!(id.hir(self.ctx), HirType::Function(_)))
                    {
                        declared
                    } else {
                        incoming
                    }
                } else {
                    declared
                }
            };
            let loc = self.alloc_loc(bound_ty);
            self.param_locs.push(loc);
            if let Some(lidx) = self.index.local_idx(param.id) {
                local_slots.push(InstanceLocalSlot { local: lidx, loc });
            }
        }
        let param_locs = TySpan {
            start: param_loc_start,
            len: view.params.len() as u32,
        };

        let explicit_return = if self
            .index
            .template_explicit_return
            .contains(template.0 as usize)
        {
            let ret = self.types.intern_hir(self.ctx, view.return_type, true);
            let ret = self.types.substitute(self.ctx, ret, &type_bindings);
            (!self.types.is_open(ret)).then_some(ret)
        } else {
            None
        };
        let return_loc = self.alloc_loc(explicit_return.unwrap_or(Ty::BOTTOM));

        let keep_original = view.is_exported || view.name.starts_with("__obj_method_");
        let emitted_func_id = if keep_original {
            view.func_id
        } else {
            let id = self.next_func_id;
            self.next_func_id = self.next_func_id.saturating_add(1);
            id
        };
        let emitted_name = if keep_original || template == TemplateId(0) {
            view.name.to_owned()
        } else if view.name.is_empty() {
            format!("callback_{}_instance_{emitted_func_id}", view.func_id)
        } else {
            format!("{}_instance_{emitted_func_id}", view.name)
        };
        let emitted_method_name = match view.kind {
            TemplateKind::ClassMethod { .. }
            | TemplateKind::ClassStaticMethod { .. }
            | TemplateKind::ClassGetter { .. }
            | TemplateKind::ClassSetter { .. } => {
                let same_recv = self
                    .instances
                    .iter()
                    .any(|i| i.template == template && i.receiver == receiver);
                if !same_recv {
                    view.name.to_owned()
                } else {
                    format!("{}__inst_{emitted_func_id}", view.name)
                }
            }
            _ => String::new(),
        };

        self.instances.push(Instance {
            template,
            receiver,
            capture_owner,
            args: arg_span,
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
        self.instance_index.insert(inst_id.0, hash);
        self.inst_locals.push(local_slots);
        self.inst_aggs.push(Vec::new());
        self.inst_written.push(BitSet::default());
        self.inst_captures.push(inst_caps);
        self.enqueue(inst_id);
        Ok(inst_id)
    }

    fn compute_type_param_bindings(
        &mut self,
        template: TemplateId,
        receiver: Option<ClassInstId>,
        args: &[Ty],
    ) -> Vec<(shapes::TypeId, Ty)> {
        let mut bindings = Vec::new();
        // Class type parameters from receiver `ClassInst`.
        if let Some(cinst_id) = receiver
            && let Some(cinst) = self.class_insts.get(cinst_id.0 as usize)
            && let Some(class_tps) = self.class_type_params.get(cinst.class.0 as usize)
        {
            let cargs: Vec<Ty> = self
                .class_args
                .get(cinst.args.range())
                .unwrap_or(&[])
                .to_vec();
            for (&tp_id, &arg_ty) in class_tps.iter().zip(&cargs) {
                bindings.push((tp_id, arg_ty));
            }
        }
        let Some(view) = self.index.view(template) else {
            return bindings;
        };
        let Some(tps) = self.template_type_params.get(template.0 as usize) else {
            return bindings;
        };
        if tps.is_empty() {
            return bindings;
        }
        let tps = tps.clone();
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
        if let Some(caller_id) = caller
            && let Some(caller_inst) = self.instances.get(caller_id.0 as usize)
            && caller_inst.template == template
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
                            v.type_params
                                .get(idx)
                                .map(|tp| tp.name.as_str())
                                .or_else(|| v.params.get(idx).map(|p| p.name.as_str()))
                        })
                        .unwrap_or("T");
                    err = err.with_note(format!(
                        "this recursive call grows type argument {param_name} by nesting its previous type"
                    ));
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
        let mut h = std::collections::hash_map::DefaultHasher::new();
        class.hash(&mut h);
        args.hash(&mut h);
        let hash = h.finish();
        if let Some(id) = self.class_index.find(hash, |idx| {
            self.class_insts.get(idx as usize).is_some_and(|ci| {
                ci.class == class && self.class_args.get(ci.args.range()) == Some(args)
            })
        }) {
            return Ok(ClassInstId(id));
        }
        let Some(&hir_idx) = self.index.class_hir_idx.get(class.0 as usize) else {
            return Err(Diagnostic::internal("unknown class index"));
        };
        let Some(hir_class) = self.hir.classes.get(hir_idx) else {
            return Err(Diagnostic::internal("unknown class in HIR"));
        };
        let cinst_id = ClassInstId(self.class_insts.len() as u32);
        let arg_start = self.class_args.len() as u32;
        self.class_args.extend_from_slice(args);
        let arg_span = TySpan {
            start: arg_start,
            len: args.len() as u32,
        };

        let class_tps = self
            .class_type_params
            .get(class.0 as usize)
            .cloned()
            .unwrap_or_default();
        let bindings: Vec<(shapes::TypeId, Ty)> =
            class_tps.into_iter().zip(args.iter().copied()).collect();

        let field_loc_base = self.locations.len() as u32;
        for field in &hir_class.fields {
            let explicit = !matches!(field.ty, HirType::Any | HirType::Unknown);
            let ty = self.types.intern_hir(self.ctx, &field.ty, explicit);
            let ty = self.types.substitute(self.ctx, ty, &bindings);
            let initial = if self.types.is_open(ty) {
                Ty::BOTTOM
            } else {
                ty
            };
            self.alloc_loc(initial);
        }
        let field_count = hir_class.fields.len() as u32;
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
            field_count,
            ctor_inst: None,
            emitted_name,
        });
        self.class_index.insert(cinst_id.0, hash);

        // Evaluate field initializers in the context of this class instance.
        for (f_idx, field) in hir_class.fields.iter().enumerate() {
            if let Some(init) = &field.init {
                let mut state = EvalState::default();
                let init_ty = self.eval_expr(InstanceId(0), Some(cinst_id), init, &mut state)?;
                let norm = self
                    .types
                    .normalize_for_storage(self.ctx, self.decls, init_ty);
                self.write_loc(LocId(field_loc_base + f_idx as u32), norm);
            }
        }

        // If the class has a constructor and arguments were supplied (or 0-arg default ctor),
        // request the constructor instance.
        if let Some(ctor_tid) = self
            .index
            .class_ctor
            .get(class.0 as usize)
            .copied()
            .flatten()
        {
            let ctor_inst = self.request_instance(InstanceReq {
                template: ctor_tid,
                receiver: Some(cinst_id),
                capture_owner: None,
                args,
                call_byte_offset: byte_offset,
                caller,
                is_inline_callback: false,
            })?;
            if let Some(ci) = self.class_insts.get_mut(cinst_id.0 as usize) {
                ci.ctor_inst = Some(ctor_inst);
            }
        }
        Ok(cinst_id)
    }

    fn run_worklist(&mut self) -> Result<()> {
        for _wave in 0..MAX_WAVES {
            while self.worklist_head < self.worklist.len() {
                let Some(&inst_id) = self.worklist.get(self.worklist_head) else {
                    break;
                };
                self.worklist_head += 1;
                self.queued.remove(inst_id.0 as usize);
                self.evaluate_instance(inst_id)?;
            }
            if self.dirty_locs.is_empty() {
                break;
            }
            if self.csr_dirty {
                self.rebuild_csr();
            }
            self.worklist.clear();
            self.worklist_head = 0;
            let dirty: Vec<usize> = self.dirty_locs.iter().collect();
            self.dirty_locs.clear();
            for loc_idx in dirty {
                let start = self.reader_offsets.get(loc_idx).copied().unwrap_or(0) as usize;
                let end = self
                    .reader_offsets
                    .get(loc_idx + 1)
                    .copied()
                    .unwrap_or(start as u32) as usize;
                if let Some(readers) = self.reader_instances.get(start..end) {
                    for &reader in readers {
                        if self.queued.insert(reader.0 as usize) {
                            self.worklist.push(reader);
                        }
                    }
                }
            }
            if self.worklist.is_empty() {
                break;
            }
        }
        Ok(())
    }

    fn evaluate_instance(&mut self, inst_id: InstanceId) -> Result<()> {
        let Some(inst) = self.instances.get(inst_id.0 as usize) else {
            return Ok(());
        };
        let template = inst.template;
        let receiver = inst.receiver;
        let return_loc = inst.return_loc;
        let explicit_return = inst.explicit_return;
        let param_locs_span = inst.param_locs;
        let args_span = inst.args;

        let Some(view) = self.index.view(template) else {
            return Ok(());
        };
        self.current_reads.clear();
        let mut state = EvalState::default();

        // Evaluate parameter default expressions (narrowing `undefined` in frontend + lowering).
        for (p_idx, param) in view.params.iter().enumerate() {
            if let Some(default_expr) = &param.default {
                let incoming = self
                    .instance_args
                    .get(args_span.start as usize + p_idx)
                    .copied()
                    .unwrap_or(Ty::VOID);
                let (_, abs) = self.types.strip_nullish(incoming);
                let default_runs = incoming == Ty::VOID
                    || incoming == Ty::BOTTOM
                    || abs.is_some_and(Absence::admits_undefined);
                let def_ty = self.eval_expr(inst_id, receiver, default_expr, &mut state)?;
                if default_runs
                    && let Some(&ploc) = self.param_locs.get(param_locs_span.start as usize + p_idx)
                {
                    let norm = self
                        .types
                        .normalize_for_storage(self.ctx, self.decls, def_ty);
                    // A default of literal `undefined` (TS optional `x?: T`) adds `undefined`;
                    // a non-undefined default narrows `undefined` away!
                    if matches!(default_expr, Expr::Undefined) {
                        if !view.is_exported {
                            self.write_loc(ploc, Ty::VOID);
                        }
                    } else {
                        self.write_loc(ploc, norm);
                    }
                }
            }
        }

        // For ModuleInit, also evaluate class static field initializers.
        if template == TemplateId(0) {
            let static_entries: Vec<(ClassId, usize, LocId)> = self.static_field_locs.clone();
            for (cid, f_idx, loc) in static_entries {
                if let Some(&hir_idx) = self.index.class_hir_idx.get(cid.0 as usize)
                    && let Some(field) = self
                        .hir
                        .classes
                        .get(hir_idx)
                        .and_then(|c| c.static_fields.get(f_idx))
                    && let Some(init) = &field.init
                {
                    let ty = self.eval_expr(inst_id, None, init, &mut state)?;
                    let norm = self.types.normalize_for_storage(self.ctx, self.decls, ty);
                    self.write_loc(loc, norm);
                }
            }
        }

        let mut returned = Ty::BOTTOM;
        let mut has_return_value = false;
        let body = hir_compat::strip_synthetic_default_guards(view.body, view.params.iter());
        self.eval_stmts(
            inst_id,
            receiver,
            body,
            &mut state,
            &mut returned,
            &mut has_return_value,
        )?;

        if template == TemplateId(0)
            || matches!(
                view.kind,
                TemplateKind::ClassConstructor { .. } | TemplateKind::ClassSetter { .. }
            )
        {
            return Ok(());
        }

        if explicit_return.is_none() {
            let mut result_ty = if has_return_value {
                if crate::completion::returns_optional(body) {
                    self.types.with_absences(returned, Absence::Undefined)
                } else {
                    returned
                }
            } else {
                Ty::VOID
            };
            if view.is_async {
                while let TyKind::Promise(inner) = self.types.kind(result_ty) {
                    result_ty = inner;
                }
                result_ty = self.types.promise(result_ty);
            }
            if !self.types.is_open(result_ty) {
                self.write_loc(return_loc, result_ty);
            }
        }
        Ok(())
    }

    fn local_loc_in_instance(&mut self, inst_id: InstanceId, local: LocalIdx) -> Option<LocId> {
        // 1. Module-level binding?
        if self.index.local_module.contains(local.0 as usize) {
            return self
                .index
                .local_loc
                .get(local.0 as usize)
                .copied()
                .flatten();
        }
        // 2. Walk capture_owner chain to find the instance owning `local`.
        let mut cursor = Some(inst_id);
        while let Some(cur) = cursor {
            if let Some(slots) = self.inst_locals.get(cur.0 as usize)
                && let Some(slot) = slots.iter().find(|s| s.local == local)
            {
                return Some(slot.loc);
            }
            cursor = self
                .instances
                .get(cur.0 as usize)
                .and_then(|inst| inst.capture_owner);
        }
        None
    }

    fn ensure_instance_local(
        &mut self,
        inst_id: InstanceId,
        local: LocalIdx,
        initial: Ty,
    ) -> LocId {
        if self.index.local_module.contains(local.0 as usize)
            && let Some(loc) = self
                .index
                .local_loc
                .get(local.0 as usize)
                .copied()
                .flatten()
        {
            return loc;
        }
        if let Some(slots) = self.inst_locals.get(inst_id.0 as usize)
            && let Some(slot) = slots.iter().find(|s| s.local == local)
        {
            return slot.loc;
        }
        let loc = self.alloc_loc(initial);
        if let Some(slots) = self.inst_locals.get_mut(inst_id.0 as usize) {
            slots.push(InstanceLocalSlot { local, loc });
        }
        loc
    }

    fn lookup_agg(&self, inst_id: InstanceId, local: LocalIdx) -> Option<(LocId, AggregateKind)> {
        if self.index.local_module.contains(local.0 as usize) {
            return self.module_aggs.get(local.0 as usize).copied().flatten();
        }
        let mut cursor = Some(inst_id);
        while let Some(cur) = cursor {
            if let Some(aggs) = self.inst_aggs.get(cur.0 as usize)
                && let Some(slot) = aggs.iter().find(|s| s.local == local)
            {
                return Some((slot.loc, slot.kind));
            }
            cursor = self
                .instances
                .get(cur.0 as usize)
                .and_then(|inst| inst.capture_owner);
        }
        None
    }

    fn bind_agg(&mut self, inst_id: InstanceId, local: LocalIdx, kind: AggregateKind) -> LocId {
        if let Some((loc, _)) = self.lookup_agg(inst_id, local) {
            return loc;
        }
        let loc = self.alloc_loc(Ty::BOTTOM);
        if self.index.local_module.contains(local.0 as usize) {
            if let Some(slot) = self.module_aggs.get_mut(local.0 as usize) {
                *slot = Some((loc, kind));
            }
        } else if let Some(aggs) = self.inst_aggs.get_mut(inst_id.0 as usize) {
            aggs.push(AggregateSlot { local, loc, kind });
        }
        loc
    }

    fn alias_agg(&mut self, inst_id: InstanceId, dst: LocalIdx, loc: LocId, kind: AggregateKind) {
        if self.index.local_module.contains(dst.0 as usize) {
            if let Some(slot) = self.module_aggs.get_mut(dst.0 as usize) {
                *slot = Some((loc, kind));
            }
        } else if let Some(aggs) = self.inst_aggs.get_mut(inst_id.0 as usize) {
            if let Some(existing) = aggs.iter_mut().find(|s| s.local == dst) {
                existing.loc = loc;
                existing.kind = kind;
            } else {
                aggs.push(AggregateSlot {
                    local: dst,
                    loc,
                    kind,
                });
            }
        }
    }

    fn mark_written(&mut self, inst_id: InstanceId, local: LocalIdx) {
        // Walk up capture_owner if `local` belongs to an enclosing instance (e.g. S3 `set("hi")`).
        let mut cursor = Some(inst_id);
        while let Some(cur) = cursor {
            if let Some(written) = self.inst_written.get_mut(cur.0 as usize) {
                written.insert(local.0 as usize);
            }
            cursor = self
                .instances
                .get(cur.0 as usize)
                .and_then(|inst| inst.capture_owner);
        }
    }

    fn record_site(&mut self, inst_id: InstanceId, expr: &Expr, res: SiteResolution) {
        let addr = std::ptr::from_ref(expr) as usize;
        self.sites.push((inst_id, addr, res));
    }

    fn eval_stmts(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        stmts: &'h [Stmt],
        state: &mut EvalState<'h>,
        returned: &mut Ty,
        has_return_value: &mut bool,
    ) -> Result<()> {
        let depth = state.guards.depth();
        for (idx, stmt) in stmts.iter().enumerate() {
            let mut handled_let = false;
            if let Stmt::Let { id, name, init, .. } = stmt {
                state.iteration.register_binding(*id, name, init.as_ref());
                if let Some(init) = init {
                    state.iteration.guard(*id, init);
                    if let Some(hir_compat::RecordExpr::Value(mut value)) =
                        state.iteration.expression(init)
                    {
                        while let Some(hir_compat::RecordExpr::Value(inner)) =
                            state.iteration.expression(value)
                        {
                            value = inner;
                        }
                        let ty = self.eval_expr(inst_id, receiver, value, state)?;
                        if let Some(lidx) = self.index.local_idx(*id) {
                            let loc = self.ensure_instance_local(inst_id, lidx, Ty::BOTTOM);
                            self.write_loc(loc, ty);
                            self.mark_written(inst_id, lidx);
                        }
                        handled_let = true;
                    } else if matches!(init, Expr::Undefined)
                        && let Some(element) = state.iteration.element(&stmts[idx..])
                    {
                        let ty = self.eval_expr(inst_id, receiver, element.value, state)?;
                        let ty = match element.available {
                            hir_compat::Availability::Known(true) => ty,
                            hir_compat::Availability::Known(false) => Ty::VOID,
                            hir_compat::Availability::Test(_) => {
                                self.types.with_absences(ty, Absence::Undefined)
                            }
                        };
                        if let Some(lidx) = self.index.local_idx(element.id) {
                            let loc = self.ensure_instance_local(inst_id, lidx, Ty::BOTTOM);
                            self.write_loc(loc, ty);
                            self.mark_written(inst_id, lidx);
                        }
                        handled_let = true;
                    }
                }
            }
            if !handled_let {
                self.eval_stmt(inst_id, receiver, stmt, state, returned, has_return_value)?;
            }
        }
        state.guards.truncate(depth);
        Ok(())
    }

    fn eval_stmt(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        stmt: &'h Stmt,
        state: &mut EvalState<'h>,
        returned: &mut Ty,
        has_return_value: &mut bool,
    ) -> Result<()> {
        match stmt {
            Stmt::Let { id, ty, init, .. } => {
                let Some(lidx) = self.index.local_idx(*id) else {
                    if let Some(init) = init {
                        self.eval_expr(inst_id, receiver, init, state)?;
                    }
                    return Ok(());
                };
                let is_inferred = self.index.local_inferred.contains(lidx.0 as usize)
                    || !self.hir.local_source_spans.contains_key(id);
                let is_deferred = self.index.local_deferred.contains(lidx.0 as usize);
                let declared = self.types.intern_hir(self.ctx, ty, !is_inferred);
                let declared = if !is_inferred {
                    self.instantiate_annotated_class(declared, 0, Some(inst_id))?
                } else {
                    declared
                };
                let initial = if is_deferred && is_inferred {
                    Ty::BOTTOM
                } else if !is_inferred && !self.types.is_open(declared) {
                    declared
                } else {
                    Ty::BOTTOM
                };
                let loc = self.ensure_instance_local(inst_id, lidx, initial);

                if let Some(init_expr) = init {
                    if matches!(init_expr, Expr::Array(items, _) if items.is_empty()) {
                        let agg_loc = self.bind_agg(inst_id, lidx, AggregateKind::Array);
                        let elem = self.read_loc(agg_loc, inst_id);
                        if elem != Ty::BOTTOM {
                            let arr = self.types.array(elem);
                            self.write_loc(loc, arr);
                        }
                        self.mark_written(inst_id, lidx);
                        return Ok(());
                    }
                    if self.index.is_empty_object_expr(init_expr) {
                        let agg_loc = self.bind_agg(inst_id, lidx, AggregateKind::Dictionary);
                        let val = self.read_loc(agg_loc, inst_id);
                        if val != Ty::BOTTOM {
                            let obj = self.types.object(&mut [], Some(val));
                            self.write_loc(loc, obj);
                        }
                        self.mark_written(inst_id, lidx);
                        return Ok(());
                    }
                    if let Expr::LocalGet(src_id) = init_expr
                        && let Some(src_lidx) = self.index.local_idx(*src_id)
                        && let Some((agg_loc, agg_kind)) = self.lookup_agg(inst_id, src_lidx)
                    {
                        self.alias_agg(inst_id, lidx, agg_loc, agg_kind);
                    }
                    let init_ty = self.eval_expr(inst_id, receiver, init_expr, state)?;
                    if !matches!(init_expr, Expr::Undefined) {
                        self.mark_written(inst_id, lidx);
                    }
                    if matches!(self.types.kind(init_ty), TyKind::Func(_)) {
                        if let Some(slot) = self.locations.get_mut(loc.0 as usize)
                            && *slot != init_ty
                        {
                            *slot = init_ty;
                            self.dirty_locs.insert(loc.0 as usize);
                        }
                    } else if is_inferred || self.types.is_open(declared) {
                        let stored = if matches!(init_expr, Expr::Undefined) && is_deferred {
                            Ty::BOTTOM
                        } else {
                            self.types
                                .normalize_for_storage(self.ctx, self.decls, init_ty)
                        };
                        self.write_loc(loc, stored);
                    }
                }
            }
            Stmt::Expr(expr) | Stmt::Throw(expr) => {
                self.eval_expr(inst_id, receiver, expr, state)?;
            }
            Stmt::Return(value) => {
                *has_return_value = true;
                let ty = match value {
                    Some(expr) => self.eval_expr(inst_id, receiver, expr, state)?,
                    None => Ty::VOID,
                };
                let norm = self.types.normalize_for_storage(self.ctx, self.decls, ty);
                *returned = self.types.join(*returned, norm);
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.eval_expr(inst_id, receiver, condition, state)?;
                let otherwise = else_branch.as_deref().unwrap_or(&[]);
                let then_reaches = crate::completion::falls_through(then_branch);
                let else_reaches = crate::completion::falls_through(otherwise);
                let branch = state.guards.branch();
                state.guards.exclude_absences(condition, true);
                self.eval_stmts(
                    inst_id,
                    receiver,
                    then_branch,
                    state,
                    returned,
                    has_return_value,
                )?;
                let then_writes = state.guards.other_arm(branch);
                state.guards.exclude_absences(condition, false);
                self.eval_stmts(
                    inst_id,
                    receiver,
                    otherwise,
                    state,
                    returned,
                    has_return_value,
                )?;
                if let Some(truth) =
                    state
                        .guards
                        .join(branch, then_writes, then_reaches, else_reaches)
                {
                    state.guards.exclude_absences(condition, truth);
                    state.guards.assign_in(
                        if then_reaches { then_branch } else { otherwise },
                        [],
                        |_| None,
                    );
                }
            }
            Stmt::While { condition, body } => {
                state.guards.assign_in(body, [condition], |_| None);
                let depth = state.guards.depth();
                self.eval_expr(inst_id, receiver, condition, state)?;
                state.guards.exclude_absences(condition, true);
                self.eval_stmts(inst_id, receiver, body, state, returned, has_return_value)?;
                state.guards.truncate(depth);
            }
            Stmt::DoWhile { condition, body } => {
                state.guards.assign_in(body, [condition], |_| None);
                self.eval_stmts(inst_id, receiver, body, state, returned, has_return_value)?;
                self.eval_expr(inst_id, receiver, condition, state)?;
            }
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => {
                if let Some(init) = init {
                    self.eval_stmt(inst_id, receiver, init, state, returned, has_return_value)?;
                }
                state.guards.assign_in(
                    body,
                    condition.as_ref().into_iter().chain(update.as_ref()),
                    |_| None,
                );
                let depth = state.guards.depth();
                if let Some(cond) = condition {
                    self.eval_expr(inst_id, receiver, cond, state)?;
                    state.guards.exclude_absences(cond, true);
                }
                self.eval_stmts(inst_id, receiver, body, state, returned, has_return_value)?;
                state.guards.truncate(depth);
                if let Some(upd) = update {
                    self.eval_expr(inst_id, receiver, upd, state)?;
                }
            }
            Stmt::Try {
                body,
                catch,
                finally,
                ..
            } => {
                self.eval_stmts(inst_id, receiver, body, state, returned, has_return_value)?;
                if let Some(catch) = catch {
                    if let Some((id, _)) = &catch.param
                        && let Some(lidx) = self.index.local_idx(*id)
                    {
                        let err_ty = self.types.named(self.ctx, "Error");
                        let loc = self.ensure_instance_local(inst_id, lidx, err_ty);
                        self.write_loc(loc, err_ty);
                    }
                    self.eval_stmts(
                        inst_id,
                        receiver,
                        &catch.body,
                        state,
                        returned,
                        has_return_value,
                    )?;
                }
                if let Some(finally) = finally {
                    self.eval_stmts(
                        inst_id,
                        receiver,
                        finally,
                        state,
                        returned,
                        has_return_value,
                    )?;
                }
            }
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                self.eval_expr(inst_id, receiver, discriminant, state)?;
                for case in cases {
                    if let Some(test) = &case.test {
                        self.eval_expr(inst_id, receiver, test, state)?;
                    }
                    self.eval_stmts(
                        inst_id,
                        receiver,
                        &case.body,
                        state,
                        returned,
                        has_return_value,
                    )?;
                }
            }
            Stmt::Labeled { body, .. } => {
                self.eval_stmt(inst_id, receiver, body, state, returned, has_return_value)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn eval_expr(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        expr: &'h Expr,
        state: &mut EvalState<'h>,
    ) -> Result<Ty> {
        // 0. Compiler-owned dense-array iterator records (skip dead iterator-protocol branch).
        if let Some(record) = state.iteration.expression(expr) {
            return match record {
                hir_compat::RecordExpr::Value(inner) => {
                    self.eval_expr(inst_id, receiver, inner, state)
                }
                hir_compat::RecordExpr::RequireObject(inner) => {
                    let ty = self.eval_expr(inst_id, receiver, inner, state)?;
                    Ok(self.types.strip_nullish(ty).0)
                }
                hir_compat::RecordExpr::Length(src) => {
                    self.eval_expr(inst_id, receiver, src, state)?;
                    Ok(Ty::NUMBER)
                }
                hir_compat::RecordExpr::Close => Ok(Ty::VOID),
            };
        }

        // 1. Preserved `new Map` / `new Set` constructors.
        if self.metadata.constructors.get(expr).is_some() {
            return self.eval_collection_constructor(inst_id, receiver, expr, state);
        }

        // 2. Built-in callbacks (`map`, `forEach`, `reduce`, `sort`, `replace`, `Array.from`, `Map.groupBy`).
        if let Some(cb_ty) = self.try_eval_builtin_callback(inst_id, receiver, expr, state)? {
            return Ok(cb_ty);
        }

        // 3. Leaf HIR expressions (numbers, strings, booleans, Math.*, Date.*, Uint8Array.*, etc.).
        if let Some(leaf) = builtins::leaf_expr_type(self.ctx, self.types, expr) {
            // Still evaluate child expressions for side effects, calls, and storage writes!
            self.eval_leaf_children(inst_id, receiver, expr, state)?;
            return Ok(leaf);
        }

        match expr {
            Expr::LocalGet(id) => self.eval_local_get(inst_id, *id, &state.guards),
            Expr::LocalSet(id, value) => {
                if state.iteration.is_release(expr) {
                    return Ok(Ty::VOID);
                }
                state.guards.assign(*id);
                let val_ty = self.eval_expr(inst_id, receiver, value, state)?;
                if let Some(lidx) = self.index.local_idx(*id) {
                    if let Some(loc) = self.local_loc_in_instance(inst_id, lidx) {
                        let existing = self
                            .locations
                            .get(loc.0 as usize)
                            .copied()
                            .unwrap_or(Ty::BOTTOM);
                        if matches!(self.types.kind(existing), TyKind::Func(_)) {
                            return Err(Diagnostic::unsupported(
                                "a reassigned function binding requires a runtime function representation",
                            ));
                        }
                        if self.index.local_inferred.contains(lidx.0 as usize)
                            || self.index.local_deferred.contains(lidx.0 as usize)
                            || self.types.is_open(existing)
                        {
                            let norm = self
                                .types
                                .normalize_for_storage(self.ctx, self.decls, val_ty);
                            self.write_loc(loc, norm);
                        }
                    }
                    self.mark_written(inst_id, lidx);
                }
                Ok(val_ty)
            }
            Expr::This => Ok(receiver.map_or(Ty::OPEN, |cinst| self.types.class_inst(cinst))),
            Expr::FuncRef(fid) => {
                if let Some(tid) = self.index.template_of_func(*fid) {
                    Ok(self.types.func(FuncIdentity {
                        template: tid,
                        capture_owner: None,
                    }))
                } else {
                    Ok(Ty::OPEN)
                }
            }
            Expr::Closure {
                func_id, captures, ..
            } => {
                let Some(tid) = self.index.template_of_func(*func_id) else {
                    return Ok(Ty::OPEN);
                };
                let captures_local = captures.iter().any(|cap| {
                    self.index
                        .local_idx(*cap)
                        .is_some_and(|l| !self.index.local_module.contains(l.0 as usize))
                });
                let owner = if captures_local { Some(inst_id) } else { None };
                Ok(self.types.func(FuncIdentity {
                    template: tid,
                    capture_owner: owner,
                }))
            }
            Expr::Binary { op, left, right } => {
                let l = self.eval_expr(inst_id, receiver, left, state)?;
                let r = self.eval_expr(inst_id, receiver, right, state)?;
                Ok(builtins::binary_result(self.ctx, self.types, *op, l, r))
            }
            Expr::Unary { op, operand } => {
                let v = self.eval_expr(inst_id, receiver, operand, state)?;
                Ok(builtins::unary_result(*op, v))
            }
            Expr::Logical { op, left, right } => {
                let l = self.eval_expr(inst_id, receiver, left, state)?;
                let depth = state.guards.depth();
                if matches!(op, LogicalOp::And | LogicalOp::Or) {
                    state.guards.exclude_absences(left, *op == LogicalOp::And);
                }
                let r = self.eval_expr(inst_id, receiver, right, state)?;
                state.guards.truncate(depth);
                Ok(builtins::logical_result(self.types, *op, l, r))
            }
            Expr::Conditional {
                condition,
                then_expr,
                else_expr,
            } => {
                self.eval_expr(inst_id, receiver, condition, state)?;
                let guard = DefinedGuard::of(condition);
                let branch = state.guards.branch();
                state.guards.exclude_absences(condition, true);
                let mut then_ty = self.eval_expr(inst_id, receiver, then_expr, state)?;
                if guard.as_ref().is_some_and(|g| g.proves(true, then_expr)) {
                    then_ty = self.types.exclude_absences(then_ty, Absence::Undefined);
                }
                let then_writes = state.guards.other_arm(branch);
                state.guards.exclude_absences(condition, false);
                let mut else_ty = self.eval_expr(inst_id, receiver, else_expr, state)?;
                if guard.as_ref().is_some_and(|g| g.proves(false, else_expr)) {
                    else_ty = self.types.exclude_absences(else_ty, Absence::Undefined);
                }
                state.guards.join(branch, then_writes, true, true);
                Ok(self.types.join(then_ty, else_ty))
            }
            Expr::Sequence(parts) => {
                let mut last = Ty::VOID;
                for part in parts {
                    last = self.eval_expr(inst_id, receiver, part, state)?;
                }
                Ok(last)
            }
            Expr::ScopedTemp { id, value, body } => {
                let val_ty = self.eval_expr(inst_id, receiver, value, state)?;
                if let Some(lidx) = self.index.local_idx(*id) {
                    let loc = self.ensure_instance_local(inst_id, lidx, Ty::BOTTOM);
                    self.write_loc(loc, val_ty);
                    self.mark_written(inst_id, lidx);
                }
                self.eval_expr(inst_id, receiver, body, state)
            }
            Expr::Await(promise) => {
                let ty = self.eval_expr(inst_id, receiver, promise, state)?;
                Ok(match self.types.kind(ty) {
                    TyKind::Promise(inner) => inner,
                    _ => ty,
                })
            }
            Expr::StructuredClone { value, .. } => self.eval_expr(inst_id, receiver, value, state),
            Expr::Array(elements, _) => {
                if elements.is_empty() {
                    return Ok(Ty::OPEN);
                }
                let mut elem_tys = Vec::with_capacity(elements.len());
                for elem in elements {
                    elem_tys.push(self.eval_expr(inst_id, receiver, elem, state)?);
                }
                if elem_tys.contains(&Ty::BOTTOM) {
                    return Ok(Ty::BOTTOM);
                }
                if let [first, second] = elem_tys.as_slice() {
                    let (f_def, _) = self.types.strip_nullish(*first);
                    let (s_def, _) = self.types.strip_nullish(*second);
                    if f_def != s_def && !self.types.is_open(*first) && !self.types.is_open(*second)
                    {
                        return Ok(self.types.tuple(&[*first, *second]));
                    }
                }
                let mut joined = Ty::BOTTOM;
                for t in elem_tys {
                    joined = self.types.join(joined, t);
                }
                Ok(self.types.array(joined))
            }
            Expr::ArraySpread(items) => {
                let mut joined = Ty::BOTTOM;
                for item in items {
                    let t = match item {
                        perry_hir::ir::ArrayElement::Expr(e) => {
                            self.eval_expr(inst_id, receiver, e, state)?
                        }
                        perry_hir::ir::ArrayElement::Hole => Ty::VOID,
                        perry_hir::ir::ArrayElement::Spread(e) => {
                            let s = self.eval_expr(inst_id, receiver, e, state)?;
                            builtins::iterated_element(self.ctx, self.types, s).unwrap_or(Ty::OPEN)
                        }
                    };
                    joined = self.types.join(joined, t);
                }
                Ok(self.types.array(joined))
            }
            Expr::ArrayFrom(source, _) => {
                let s = self.eval_expr(inst_id, receiver, source, state)?;
                if s == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                if let Expr::Object(props) = source.as_ref()
                    && props.iter().any(|(k, _)| k == "length")
                {
                    return Ok(self.types.array(Ty::OPEN));
                }
                Ok(builtins::iterated_element(self.ctx, self.types, s)
                    .map_or(Ty::OPEN, |e| self.types.array(e)))
            }
            Expr::Object(fields) => {
                let mut props = Vec::with_capacity(fields.len());
                for (name, value) in fields {
                    let vty = self.eval_expr(inst_id, receiver, value, state)?;
                    let vty = self.types.normalize_for_storage(self.ctx, self.decls, vty);
                    props.push(ObjectProp {
                        name: self.types.intern_name(name),
                        ty: vty,
                        optional: false,
                        readonly: false,
                    });
                }
                Ok(self.types.object(&mut props, None))
            }
            Expr::ObjectSpread { parts } => {
                self.eval_object_spread(inst_id, receiver, parts, state)
            }
            Expr::ObjectRest {
                object,
                exclude_keys,
            } => {
                let src = self.eval_expr(inst_id, receiver, object, state)?;
                if src == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let (stripped, _) = self.types.strip_nullish(src);
                if let TyKind::Object { props, .. } = self.types.kind(stripped) {
                    let mut kept: Vec<ObjectProp> = self
                        .types
                        .object_slice(props)
                        .iter()
                        .copied()
                        .filter(|p| {
                            let name = self.types.name_str(p.name);
                            !exclude_keys.iter().any(|k| k == name)
                        })
                        .collect();
                    return Ok(self.types.object(&mut kept, None));
                }
                Ok(Ty::OPEN)
            }
            Expr::PropertyGet {
                object, property, ..
            } => self.eval_property_get(inst_id, receiver, object, property, state),
            Expr::PropertySet {
                object,
                property,
                value,
                ..
            } => {
                let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
                let val_ty = self.eval_expr(inst_id, receiver, value, state)?;
                let norm = self
                    .types
                    .normalize_for_storage(self.ctx, self.decls, val_ty);
                // Write into a class field (`this.field = val` or `obj.field = val`).
                let (obj_def, _) = self.types.strip_nullish(obj_ty);
                if let TyKind::ClassInst(cinst_id) = self.types.kind(obj_def) {
                    self.write_class_field(cinst_id, property, norm);
                }
                // Write into an open empty-object dictionary (`dict.prop = val`).
                if let Expr::LocalGet(id) = object.as_ref()
                    && let Some(lidx) = self.index.local_idx(*id)
                    && let Some((agg_loc, AggregateKind::Dictionary)) =
                        self.lookup_agg(inst_id, lidx)
                {
                    self.write_loc(agg_loc, norm);
                }
                Ok(val_ty)
            }
            Expr::IndexGet { object, index, .. } => {
                let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
                self.eval_expr(inst_id, receiver, index, state)?;
                if obj_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                Ok(
                    builtins::index_type(self.ctx, self.decls, self.types, obj_ty, index)
                        .unwrap_or(Ty::OPEN),
                )
            }
            Expr::IndexSet {
                object,
                index,
                value,
                ..
            } => {
                let _obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
                let idx_ty = self.eval_expr(inst_id, receiver, index, state)?;
                let val_ty = self.eval_expr(inst_id, receiver, value, state)?;
                if let Expr::LocalGet(id) = object.as_ref()
                    && let Some(lidx) = self.index.local_idx(*id)
                    && let Some((agg_loc, agg_kind)) = self.lookup_agg(inst_id, lidx)
                {
                    let matches_kind = match agg_kind {
                        AggregateKind::Array => matches!(idx_ty, Ty::NUMBER | Ty::INT32),
                        AggregateKind::Dictionary => idx_ty == Ty::STRING,
                    };
                    if matches_kind {
                        let norm = self
                            .types
                            .normalize_for_storage(self.ctx, self.decls, val_ty);
                        self.write_loc(agg_loc, norm);
                    }
                }
                Ok(val_ty)
            }
            Expr::PutValueSet {
                target,
                key,
                value,
                receiver: recv,
                ..
            } => {
                let obj_ty = self.eval_expr(inst_id, receiver, target, state)?;
                let idx_ty = self.eval_expr(inst_id, receiver, key, state)?;
                let val_ty = self.eval_expr(inst_id, receiver, value, state)?;
                let norm = self
                    .types
                    .normalize_for_storage(self.ctx, self.decls, val_ty);
                if let Expr::String(field_name) = key.as_ref() {
                    let (obj_def, _) = self.types.strip_nullish(obj_ty);
                    if let TyKind::ClassInst(cinst_id) = self.types.kind(obj_def) {
                        self.write_class_field(cinst_id, field_name, norm);
                    }
                }
                if let (Expr::LocalGet(id), Expr::LocalGet(rid)) = (target.as_ref(), recv.as_ref())
                    && id == rid
                    && let Some(lidx) = self.index.local_idx(*id)
                    && let Some((agg_loc, agg_kind)) = self.lookup_agg(inst_id, lidx)
                {
                    let matches_kind = match agg_kind {
                        AggregateKind::Array => matches!(idx_ty, Ty::NUMBER | Ty::INT32),
                        AggregateKind::Dictionary => idx_ty == Ty::STRING,
                    };
                    if matches_kind {
                        self.write_loc(agg_loc, norm);
                    }
                }
                Ok(val_ty)
            }
            Expr::StaticFieldGet {
                class_name,
                field_name,
            } => {
                if let Some(cid) = self.index.class_by_name(class_name)
                    && let Some(loc) = self.index.class_static_field_loc(cid, field_name)
                {
                    return Ok(self.read_loc(loc, inst_id));
                }
                Ok(Ty::OPEN)
            }
            Expr::StaticFieldSet {
                class_name,
                field_name,
                value,
            } => {
                let val_ty = self.eval_expr(inst_id, receiver, value, state)?;
                if let Some(cid) = self.index.class_by_name(class_name)
                    && let Some(loc) = self.index.class_static_field_loc(cid, field_name)
                {
                    let norm = self
                        .types
                        .normalize_for_storage(self.ctx, self.decls, val_ty);
                    self.write_loc(loc, norm);
                }
                Ok(val_ty)
            }
            Expr::StaticMethodCall {
                class_name,
                method_name,
                args,
                ..
            } => {
                let mut arg_tys = Vec::with_capacity(args.len());
                for arg in args {
                    arg_tys.push(self.eval_expr(inst_id, receiver, arg, state)?);
                }
                if arg_tys.contains(&Ty::BOTTOM) {
                    return Ok(Ty::BOTTOM);
                }
                if let Some(cid) = self.index.class_by_name(class_name)
                    && let Some(tid) = self.index.class_static_method(cid, method_name)
                {
                    let target = self.request_instance(InstanceReq {
                        template: tid,
                        receiver: None,
                        capture_owner: None,
                        args: &arg_tys,
                        call_byte_offset: 0,
                        caller: Some(inst_id),
                        is_inline_callback: false,
                    })?;
                    let ret_loc = self
                        .instances
                        .get(target.0 as usize)
                        .map_or(LocId(0), |i| i.return_loc);
                    return Ok(self.read_loc(ret_loc, inst_id));
                }
                Ok(Ty::OPEN)
            }
            Expr::New {
                class_name,
                args,
                type_args,
                byte_offset,
                ..
            } => self.eval_new(
                inst_id,
                receiver,
                expr,
                class_name,
                args,
                type_args,
                *byte_offset,
                state,
            ),
            Expr::Call {
                callee,
                args,
                type_args,
                byte_offset,
                ..
            } => self.eval_call(
                inst_id,
                receiver,
                expr,
                callee,
                args,
                type_args,
                *byte_offset,
                state,
            ),
            Expr::CallSpread { callee, .. } => {
                self.eval_leaf_children(inst_id, receiver, expr, state)?;
                if let Expr::PropertyGet {
                    object, property, ..
                } = callee.as_ref()
                {
                    let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
                    let name = hir_compat::method(property).name();
                    if matches!(name, "concat" | "filter" | "sort" | "toSorted") {
                        return Ok(obj_ty);
                    }
                    if matches!(name, "max" | "min") {
                        return Ok(Ty::NUMBER);
                    }
                }
                Ok(Ty::OPEN)
            }
            Expr::NativeMethodCall {
                module,
                object,
                method,
                args,
                ..
            } => {
                if module == "__perry_runtime" && method == "requireObjectCoercible" {
                    if let Some(first) = args.first() {
                        let ty = self.eval_expr(inst_id, receiver, first, state)?;
                        return Ok(self.types.strip_nullish(ty).0);
                    }
                    return Ok(Ty::OPEN);
                }
                let mut arg_tys = Vec::with_capacity(args.len());
                for arg in args {
                    arg_tys.push(self.eval_expr(inst_id, receiver, arg, state)?);
                }
                if let Some(obj) = object {
                    let obj_ty = self.eval_expr(inst_id, receiver, obj, state)?;
                    if obj_ty == Ty::BOTTOM {
                        return Ok(Ty::BOTTOM);
                    }
                    let (stripped, _) = self.types.strip_nullish(obj_ty);
                    let rule = args
                        .is_empty()
                        .then(|| {
                            builtins::lookup_method_result(
                                self.ctx, self.types, stripped, method, true,
                            )
                        })
                        .flatten()
                        .or_else(|| {
                            builtins::lookup_method_result(
                                self.ctx, self.types, stripped, method, false,
                            )
                        });
                    if let Some(rule) = rule {
                        return Ok(builtins::apply_method_result(
                            self.ctx,
                            self.types,
                            rule,
                            MethodResultArgs {
                                receiver: obj_ty,
                                arg0: arg_tys.first().copied(),
                                arg0_expr: args.first(),
                                callback_result: None,
                                sparse_arrays: self.facts.sparse_arrays,
                            },
                        ));
                    }
                }
                Ok(Ty::OPEN)
            }
            Expr::ObjectValues(object) => {
                let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
                if obj_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let (stripped, _) = self.types.strip_nullish(obj_ty);
                let hir = self.types.materialize_hir(self.ctx, stripped, &|_| None);
                Ok(self
                    .decls
                    .dict_entry_types(self.ctx, &hir)
                    .map_or(Ty::OPEN, |(_, val)| {
                        let v = self.types.intern_hir(self.ctx, &val, true);
                        self.types.array(v)
                    }))
            }
            Expr::ObjectEntries(object) => {
                let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
                if obj_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let (stripped, _) = self.types.strip_nullish(obj_ty);
                let hir = self.types.materialize_hir(self.ctx, stripped, &|_| None);
                Ok(self
                    .decls
                    .dict_entry_types(self.ctx, &hir)
                    .map_or(Ty::OPEN, |(k, v)| {
                        let k = self.types.intern_hir(self.ctx, &k, true);
                        let v = self.types.intern_hir(self.ctx, &v, true);
                        let pair = self.types.tuple(&[k, v]);
                        self.types.array(pair)
                    }))
            }
            Expr::ObjectFromEntries(entries) => {
                let ent_ty = self.eval_expr(inst_id, receiver, entries, state)?;
                if ent_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let (stripped, _) = self.types.strip_nullish(ent_ty);
                if let TyKind::Array(inner) = self.types.kind(stripped)
                    && let TyKind::Tuple(span) = self.types.kind(inner)
                    && let [_, val] = self.types.tuple_slice(span)
                {
                    let val = *val;
                    let base =
                        shapes::TypeId::from_owned(self.ctx, HirType::Named("Record".into()));
                    return Ok(self.types.generic(base, &[Ty::STRING, val]));
                }
                Ok(Ty::OPEN)
            }
            Expr::ArraySlice {
                array, start, end, ..
            } => {
                let arr_ty = self.eval_expr(inst_id, receiver, array, state)?;
                self.eval_expr(inst_id, receiver, start, state)?;
                if let Some(e) = end {
                    self.eval_expr(inst_id, receiver, e, state)?;
                }
                Ok(arr_ty)
            }
            Expr::ArrayToReversed { array }
            | Expr::ArraySort { array, .. }
            | Expr::ArrayToSorted { array, .. } => self.eval_expr(inst_id, receiver, array, state),
            Expr::ArrayPop(id) | Expr::ArrayShift(id) => {
                let arr_ty = self.eval_local_get(inst_id, *id, &state.guards)?;
                if arr_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                Ok(builtins::apply_method_result(
                    self.ctx,
                    self.types,
                    crate::methods::MethodResult::Element,
                    MethodResultArgs::receiver_only(arr_ty),
                ))
            }
            Expr::ArrayAt { array, index } => {
                let arr_ty = self.eval_expr(inst_id, receiver, array, state)?;
                self.eval_expr(inst_id, receiver, index, state)?;
                if arr_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                Ok(builtins::apply_method_result(
                    self.ctx,
                    self.types,
                    crate::methods::MethodResult::At,
                    MethodResultArgs::receiver_only(arr_ty),
                ))
            }
            Expr::ArrayWith {
                array,
                index,
                value,
            } => {
                let arr_ty = self.eval_expr(inst_id, receiver, array, state)?;
                self.eval_expr(inst_id, receiver, index, state)?;
                self.eval_expr(inst_id, receiver, value, state)?;
                Ok(arr_ty)
            }
            Expr::ArrayKeys(src) | Expr::MapKeys(src) => {
                let s = self.eval_expr(inst_id, receiver, src, state)?;
                if s == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                Ok(builtins::iterator_view(
                    self.ctx,
                    self.types,
                    s,
                    crate::collections::IteratorMethod::Keys,
                )
                .unwrap_or(Ty::OPEN))
            }
            Expr::ArrayValues(src) | Expr::MapValues(src) | Expr::SetValues(src) => {
                let s = self.eval_expr(inst_id, receiver, src, state)?;
                if s == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                Ok(builtins::iterator_view(
                    self.ctx,
                    self.types,
                    s,
                    crate::collections::IteratorMethod::Values,
                )
                .unwrap_or(Ty::OPEN))
            }
            Expr::ArrayEntries(src) | Expr::MapEntries(src) => {
                let s = self.eval_expr(inst_id, receiver, src, state)?;
                if s == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                Ok(builtins::iterator_view(
                    self.ctx,
                    self.types,
                    s,
                    crate::collections::IteratorMethod::Entries,
                )
                .unwrap_or(Ty::OPEN))
            }
            Expr::MapGet { map, key } => {
                let m = self.eval_expr(inst_id, receiver, map, state)?;
                self.eval_expr(inst_id, receiver, key, state)?;
                if m == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                Ok(builtins::apply_method_result(
                    self.ctx,
                    self.types,
                    crate::methods::MethodResult::Lookup,
                    MethodResultArgs::receiver_only(m),
                ))
            }
            Expr::MapSet { map, key, value } => {
                let m = self.eval_expr(inst_id, receiver, map, state)?;
                self.eval_expr(inst_id, receiver, key, state)?;
                self.eval_expr(inst_id, receiver, value, state)?;
                Ok(m)
            }
            Expr::SetAdd { set_id, value } => {
                self.eval_expr(inst_id, receiver, value, state)?;
                self.eval_local_get(inst_id, *set_id, &state.guards)
            }
            Expr::CryptoRandomFillSync {
                buffer,
                offset,
                size,
            } => {
                let b = self.eval_expr(inst_id, receiver, buffer, state)?;
                self.eval_expr(inst_id, receiver, offset, state)?;
                self.eval_expr(inst_id, receiver, size, state)?;
                Ok(b)
            }
            _ => {
                // Any other expression variant: evaluate children for side effects/writes.
                self.eval_leaf_children(inst_id, receiver, expr, state)?;
                Ok(Ty::OPEN)
            }
        }
    }

    fn eval_leaf_children(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        expr: &'h Expr,
        state: &mut EvalState<'h>,
    ) -> Result<()> {
        match expr {
            // Check function-identity comparison folding (`f === double`, `keep === keep`).
            Expr::Compare { op, left, right }
                if matches!(
                    op,
                    CompareOp::Eq | CompareOp::Ne | CompareOp::LooseEq | CompareOp::LooseNe
                ) =>
            {
                let l = self.eval_expr(inst_id, receiver, left, state)?;
                let r = self.eval_expr(inst_id, receiver, right, state)?;
                if let (TyKind::Func(a), TyKind::Func(b)) = (self.types.kind(l), self.types.kind(r))
                {
                    for ident in [a, b] {
                        if ident.capture_owner.is_some()
                            || self.index.view(ident.template).is_some_and(|v| {
                                v.captures.iter().any(|c| {
                                    self.index.local_idx(*c).is_some_and(|li| {
                                        !self.index.local_module.contains(li.0 as usize)
                                    })
                                })
                            })
                        {
                            let name = self
                                .index
                                .view(ident.template)
                                .map_or("closure", |v| v.name);
                            return Err(Diagnostic::unsupported(format!(
                                "cannot compare capturing function '{name}'"
                            )));
                        }
                    }
                    let eq = (a.template == b.template)
                        == matches!(op, CompareOp::Eq | CompareOp::LooseEq);
                    self.record_site(inst_id, expr, SiteResolution::FoldBool(eq));
                }
                return Ok(());
            }
            // `arr.push(value)` lowered directly to `Expr::ArrayPush { array_id, value }`.
            Expr::ArrayPush {
                array_id, value, ..
            } => {
                let val_ty = self.eval_expr(inst_id, receiver, value, state)?;
                if let Some(lidx) = self.index.local_idx(*array_id)
                    && let Some((agg_loc, AggregateKind::Array)) = self.lookup_agg(inst_id, lidx)
                {
                    let norm = self
                        .types
                        .normalize_for_storage(self.ctx, self.decls, val_ty);
                    self.write_loc(agg_loc, norm);
                }
                return Ok(());
            }
            Expr::Delete(target) => {
                if let Expr::IndexGet { object, .. } | Expr::PropertyGet { object, .. } =
                    target.as_ref()
                {
                    let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
                    if self.type_may_be_array(obj_ty) {
                        self.deletes_array_elements = true;
                    }
                }
            }
            Expr::Update { id, .. } => {
                state.guards.assign(*id);
                if let Some(lidx) = self.index.local_idx(*id) {
                    self.mark_written(inst_id, lidx);
                }
                return Ok(());
            }
            _ => {}
        }
        let mut err = None;
        perry_hir::walker::walk_expr_children(expr, &mut |child| {
            if err.is_none()
                && let Err(e) = self.eval_expr(inst_id, receiver, child, state)
            {
                err = Some(e);
            }
        });
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn eval_local_get(
        &mut self,
        inst_id: InstanceId,
        id: LocalId,
        guards: &Guards<'h>,
    ) -> Result<Ty> {
        let Some(lidx) = self.index.local_idx(id) else {
            return Ok(Ty::OPEN);
        };
        // If `local` aliases an open empty array/object aggregate, read its element/value join!
        if let Some((agg_loc, agg_kind)) = self.lookup_agg(inst_id, lidx) {
            let elem = self.read_loc(agg_loc, inst_id);
            if elem != Ty::BOTTOM {
                let agg_ty = match agg_kind {
                    AggregateKind::Array => self.types.array(elem),
                    AggregateKind::Dictionary => self.types.object(&mut [], Some(elem)),
                };
                return Ok(agg_ty);
            }
        }
        let Some(loc) = self.local_loc_in_instance(inst_id, lidx) else {
            return Ok(self
                .index
                .local_declared
                .get(lidx.0 as usize)
                .copied()
                .unwrap_or(Ty::OPEN));
        };
        let mut ty = self.read_loc(loc, inst_id);
        // Deferred local read before any write sees `undefined`.
        if self.index.local_deferred.contains(lidx.0 as usize)
            && self
                .inst_written
                .get(inst_id.0 as usize)
                .is_none_or(|w| !w.contains(lidx.0 as usize))
            && ty == Ty::BOTTOM
        {
            return Ok(Ty::VOID);
        }
        // Apply flow-narrowing guards when the local is not written from a closure.
        if !self
            .index
            .local_written_from_closure
            .contains(lidx.0 as usize)
            && let Some(excluded) = guards.excluded(id)
        {
            ty = self.types.exclude_absences(ty, excluded);
        }
        Ok(ty)
    }

    fn eval_collection_constructor(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        expr: &'h Expr,
        state: &mut EvalState<'h>,
    ) -> Result<Ty> {
        // Extract explicit types from metadata first so `init` keeps its `'h` borrow of `expr`.
        let (is_map, explicit_tys) = match self.metadata.constructors.get(expr) {
            Some(hir_compat::Constructor::Map { types, .. }) => (
                true,
                types.map(|(k, v)| {
                    vec![
                        self.types.intern_hir(self.ctx, k, true),
                        self.types.intern_hir(self.ctx, v, true),
                    ]
                }),
            ),
            Some(hir_compat::Constructor::Set { element, .. }) => (
                false,
                element.map(|e| vec![self.types.intern_hir(self.ctx, e, true)]),
            ),
            None => return Ok(Ty::OPEN),
        };
        let init = constructor_init_expr(expr);
        if let Some(init) = init {
            self.eval_expr(inst_id, receiver, init, state)?;
        }
        let base_name = if is_map { "Map" } else { "Set" };
        let base = shapes::TypeId::from_owned(self.ctx, HirType::Named(base_name.into()));
        if let Some(mut arg_tys) = explicit_tys {
            let type_bindings = self.active_instance_type_bindings(inst_id);
            for ty in &mut arg_tys {
                *ty = self.types.substitute(self.ctx, *ty, &type_bindings);
            }
            if !type_bindings.is_empty() {
                self.record_site(
                    inst_id,
                    expr,
                    SiteResolution::CollectionNew {
                        is_map,
                        type_args: arg_tys.clone(),
                    },
                );
            }
            return Ok(self.types.generic(base, &arg_tys));
        }
        if let Some(init) = init {
            if is_map {
                if let Expr::Array(entries, _) = init
                    && let Some(Expr::Array(pair, _)) = entries.first()
                    && let [key, value] = pair.as_slice()
                {
                    let k = self.eval_expr(inst_id, receiver, key, state)?;
                    let v = self.eval_expr(inst_id, receiver, value, state)?;
                    if k == Ty::BOTTOM || v == Ty::BOTTOM {
                        return Ok(Ty::BOTTOM);
                    }
                    return Ok(self.types.generic(base, &[k, v]));
                }
                let init_ty = self.eval_expr(inst_id, receiver, init, state)?;
                if init_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                let (stripped, _) = self.types.strip_nullish(init_ty);
                if let TyKind::Array(elem) = self.types.kind(stripped)
                    && let TyKind::Tuple(span) = self.types.kind(elem)
                    && let [k, v] = self.types.tuple_slice(span)
                {
                    let (k, v) = (*k, *v);
                    return Ok(self.types.generic(base, &[k, v]));
                }
            } else {
                let init_ty = self.eval_expr(inst_id, receiver, init, state)?;
                if init_ty == Ty::BOTTOM {
                    return Ok(Ty::BOTTOM);
                }
                if let Some(elem) = builtins::iterated_element(self.ctx, self.types, init_ty) {
                    return Ok(self.types.generic(base, &[elem]));
                }
            }
        }
        Ok(Ty::OPEN)
    }

    fn active_instance_type_bindings(&mut self, inst_id: InstanceId) -> Vec<(shapes::TypeId, Ty)> {
        let Some(inst) = self.instances.get(inst_id.0 as usize) else {
            return Vec::new();
        };
        let template = inst.template;
        let receiver = inst.receiver;
        let args: Vec<Ty> = self
            .instance_args
            .get(inst.args.range())
            .unwrap_or(&[])
            .to_vec();
        self.compute_type_param_bindings(template, receiver, &args)
    }

    fn try_eval_builtin_callback(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        expr: &'h Expr,
        state: &mut EvalState<'h>,
    ) -> Result<Option<Ty>> {
        // Pre-evaluate receiver / iterable expressions needed by the callback table.
        let (callback, mut inputs, contract, recv_ty, map_group_elem) = match expr {
            Expr::ArrayMap { array, callback } => {
                let r = self.eval_expr(inst_id, receiver, array, state)?;
                if r == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let Some(elem) = builtins::iterated_element(self.ctx, self.types, r) else {
                    return Ok(None);
                };
                (
                    callback.as_ref(),
                    vec![elem, Ty::NUMBER, r],
                    CallbackContract::Mapped {
                        keep_holes: self.facts.sparse_arrays,
                    },
                    r,
                    None,
                )
            }
            Expr::ArrayFlatMap { array, callback } => {
                let r = self.eval_expr(inst_id, receiver, array, state)?;
                if r == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let Some(elem) = builtins::iterated_element(self.ctx, self.types, r) else {
                    return Ok(None);
                };
                (
                    callback.as_ref(),
                    vec![elem, Ty::NUMBER, r],
                    CallbackContract::FlatMapped,
                    r,
                    None,
                )
            }
            Expr::ArrayFilter { array, callback }
            | Expr::ArraySome { array, callback }
            | Expr::ArrayEvery { array, callback } => {
                let r = self.eval_expr(inst_id, receiver, array, state)?;
                if r == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let Some(elem) = builtins::iterated_element(self.ctx, self.types, r) else {
                    return Ok(None);
                };
                (
                    callback.as_ref(),
                    vec![elem, Ty::NUMBER, r],
                    CallbackContract::Predicate,
                    r,
                    None,
                )
            }
            Expr::ArrayFind { array, callback } | Expr::ArrayFindIndex { array, callback } => {
                let r = self.eval_expr(inst_id, receiver, array, state)?;
                if r == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let Some(elem) = builtins::iterated_element(self.ctx, self.types, r) else {
                    return Ok(None);
                };
                let opt = self.types.with_absences(elem, Absence::Undefined);
                (
                    callback.as_ref(),
                    vec![opt, Ty::NUMBER, r],
                    CallbackContract::Predicate,
                    r,
                    None,
                )
            }
            Expr::ArraySort { array, comparator }
                if !matches!(comparator.as_ref(), Expr::Undefined) =>
            {
                let r = self.eval_expr(inst_id, receiver, array, state)?;
                if r == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let Some(elem) = builtins::iterated_element(self.ctx, self.types, r) else {
                    return Ok(None);
                };
                let def = self.types.exclude_absences(elem, Absence::Undefined);
                (
                    comparator.as_ref(),
                    vec![def, def],
                    CallbackContract::Comparator,
                    r,
                    None,
                )
            }
            Expr::ArrayToSorted {
                array,
                comparator: Some(comparator),
                ..
            } if !matches!(comparator.as_ref(), Expr::Undefined) => {
                let r = self.eval_expr(inst_id, receiver, array, state)?;
                if r == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let Some(elem) = builtins::iterated_element(self.ctx, self.types, r) else {
                    return Ok(None);
                };
                let def = self.types.exclude_absences(elem, Absence::Undefined);
                (
                    comparator.as_ref(),
                    vec![def, def],
                    CallbackContract::Comparator,
                    r,
                    None,
                )
            }
            Expr::ArrayFromMapped {
                iterable, map_fn, ..
            } => {
                let src = self.eval_expr(inst_id, receiver, iterable, state)?;
                if src == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let elem = if let Expr::Object(props) = iterable.as_ref()
                    && props.iter().any(|(k, _)| k == "length")
                {
                    Ty::VOID
                } else {
                    builtins::iterated_element(self.ctx, self.types, src).unwrap_or(Ty::OPEN)
                };
                (
                    map_fn.as_ref(),
                    vec![elem, Ty::NUMBER],
                    CallbackContract::Mapped { keep_holes: false },
                    src,
                    None,
                )
            }
            Expr::MapGroupBy { items, key_fn } => {
                let src = self.eval_expr(inst_id, receiver, items, state)?;
                if src == Ty::BOTTOM {
                    return Ok(Some(Ty::BOTTOM));
                }
                let elem =
                    builtins::iterated_element(self.ctx, self.types, src).unwrap_or(Ty::OPEN);
                (
                    key_fn.as_ref(),
                    vec![elem, Ty::NUMBER],
                    CallbackContract::GroupKey,
                    src,
                    Some(elem),
                )
            }
            Expr::Call { callee, args, .. } => {
                let Expr::PropertyGet {
                    object, property, ..
                } = callee.as_ref()
                else {
                    return Ok(None);
                };
                let method_name = hir_compat::method(property).name();
                // Map.groupBy(items, keyFn)
                if let Expr::PropertyGet {
                    object: global,
                    property: class,
                    ..
                } = object.as_ref()
                    && matches!(global.as_ref(), Expr::GlobalGet(_))
                    && class == "Map"
                    && method_name == "groupBy"
                    && let [items, key_fn] = args.as_slice()
                {
                    let src = self.eval_expr(inst_id, receiver, items, state)?;
                    if src == Ty::BOTTOM {
                        return Ok(Some(Ty::BOTTOM));
                    }
                    let elem =
                        builtins::iterated_element(self.ctx, self.types, src).unwrap_or(Ty::OPEN);
                    (
                        key_fn,
                        vec![elem, Ty::NUMBER],
                        CallbackContract::GroupKey,
                        src,
                        Some(elem),
                    )
                } else {
                    let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
                    if obj_ty == Ty::BOTTOM {
                        return Ok(Some(Ty::BOTTOM));
                    }
                    let (stripped, _) = self.types.strip_nullish(obj_ty);
                    // Map.forEach / Set.forEach
                    if method_name == "forEach"
                        && let TyKind::Generic { base, args: gargs } = self.types.kind(stripped)
                        && let HirType::Named(base_name) = base.hir(self.ctx)
                        && let Some(cb) = args.first()
                    {
                        let gslice: Vec<Ty> = self.types.generic_slice(gargs).to_vec();
                        match (base_name.as_str(), gslice.as_slice()) {
                            ("Map" | "ReadonlyMap", [k, v]) => (
                                cb,
                                vec![*v, *k, stripped],
                                CallbackContract::Void,
                                obj_ty,
                                None,
                            ),
                            ("Set" | "ReadonlySet", [v]) => (
                                cb,
                                vec![*v, *v, stripped],
                                CallbackContract::Void,
                                obj_ty,
                                None,
                            ),
                            _ => return Ok(None),
                        }
                    } else {
                        let Some(recv_kind) = builtins::receiver_of(self.ctx, self.types, stripped)
                        else {
                            return Ok(None);
                        };
                        let Some(method) = crate::methods::Method::lookup(recv_kind, method_name)
                        else {
                            return Ok(None);
                        };
                        let Some(cb_kind) = method.callback() else {
                            return Ok(None);
                        };
                        let Some((first_arg, tail)) = args.split_first() else {
                            return Ok(None);
                        };
                        match cb_kind {
                            crate::methods::Callback::Replacement => {
                                let Some(cb) = tail.first() else {
                                    return Ok(None);
                                };
                                let cb_ty = self.eval_expr(inst_id, receiver, cb, state)?;
                                if cb_ty == Ty::BOTTOM {
                                    return Ok(Some(Ty::BOTTOM));
                                }
                                if !matches!(self.types.kind(cb_ty), TyKind::Func(_)) {
                                    return Ok(None);
                                }
                                let mut in_tys = vec![Ty::STRING];
                                let captures = match first_arg {
                                    Expr::RegExp { pattern, .. } => {
                                        Some(crate::methods::regex_captures(pattern))
                                    }
                                    Expr::String(_) => Some((0, false)),
                                    _ => None,
                                };
                                if let Some((count, named)) = captures {
                                    let opt_str =
                                        self.types.with_absences(Ty::STRING, Absence::Undefined);
                                    in_tys.extend(std::iter::repeat_n(opt_str, count));
                                    in_tys.extend([Ty::NUMBER, Ty::STRING]);
                                    if named {
                                        in_tys.push(Ty::DYNAMIC);
                                    }
                                }
                                (cb, in_tys, CallbackContract::Replacement, obj_ty, None)
                            }
                            crate::methods::Callback::Elements => {
                                let Some(elem) =
                                    builtins::iterated_element(self.ctx, self.types, stripped)
                                else {
                                    return Ok(None);
                                };
                                let contract = match method_name {
                                    "forEach" => CallbackContract::Void,
                                    "map" => CallbackContract::Mapped {
                                        keep_holes: self.facts.sparse_arrays,
                                    },
                                    "flatMap" => CallbackContract::FlatMapped,
                                    _ => CallbackContract::Predicate,
                                };
                                (
                                    first_arg,
                                    vec![elem, Ty::NUMBER, stripped],
                                    contract,
                                    obj_ty,
                                    None,
                                )
                            }
                            crate::methods::Callback::AllIndices => {
                                let Some(elem) =
                                    builtins::iterated_element(self.ctx, self.types, stripped)
                                else {
                                    return Ok(None);
                                };
                                let opt = self.types.with_absences(elem, Absence::Undefined);
                                (
                                    first_arg,
                                    vec![opt, Ty::NUMBER, stripped],
                                    CallbackContract::Predicate,
                                    obj_ty,
                                    None,
                                )
                            }
                            crate::methods::Callback::Comparator => {
                                if matches!(first_arg, Expr::Undefined) {
                                    return Ok(None);
                                }
                                let Some(elem) =
                                    builtins::iterated_element(self.ctx, self.types, stripped)
                                else {
                                    return Ok(None);
                                };
                                let def = self.types.exclude_absences(elem, Absence::Undefined);
                                (
                                    first_arg,
                                    vec![def, def],
                                    CallbackContract::Comparator,
                                    obj_ty,
                                    None,
                                )
                            }
                            crate::methods::Callback::Fold => {
                                let Some(elem) =
                                    builtins::iterated_element(self.ctx, self.types, stripped)
                                else {
                                    return Ok(None);
                                };
                                let acc = match tail.first() {
                                    Some(init) => self.eval_expr(inst_id, receiver, init, state)?,
                                    None => elem,
                                };
                                if acc == Ty::BOTTOM {
                                    return Ok(Some(Ty::BOTTOM));
                                }
                                (
                                    first_arg,
                                    vec![acc, elem, Ty::NUMBER, stripped],
                                    CallbackContract::Fold,
                                    obj_ty,
                                    None,
                                )
                            }
                        }
                    }
                }
            }
            _ => return Ok(None),
        };

        if inputs.contains(&Ty::BOTTOM) {
            return Ok(Some(Ty::BOTTOM));
        }
        let cb_val = self.eval_expr(inst_id, receiver, callback, state)?;
        if cb_val == Ty::BOTTOM {
            return Ok(Some(Ty::BOTTOM));
        }
        let TyKind::Func(ident) = self.types.kind(cb_val) else {
            return Ok(None);
        };
        // Trim inputs to the callback's parameter count if it has fewer params (and no rest param).
        let Some(cb_view) = self.index.view(ident.template) else {
            return Ok(None);
        };
        let keep_inline_closure = matches!(callback, Expr::Closure { .. })
            && (cb_view.captures_this || cb_view.captures_new_target);
        if !cb_view.params.iter().any(|p| p.is_rest) && inputs.len() > cb_view.params.len() {
            inputs.truncate(cb_view.params.len());
        }
        let cb_inst = self.request_instance(InstanceReq {
            template: ident.template,
            receiver: if keep_inline_closure { receiver } else { None },
            capture_owner: ident.capture_owner,
            args: &inputs,
            call_byte_offset: 0,
            caller: Some(inst_id),
            is_inline_callback: true,
        })?;
        self.record_site(
            inst_id,
            callback,
            SiteResolution::Callback {
                target: cb_inst,
                keep_inline_closure,
            },
        );
        let ret_loc = self
            .instances
            .get(cb_inst.0 as usize)
            .map_or(LocId(0), |i| i.return_loc);
        let cb_ret = self.read_loc(ret_loc, inst_id);

        // Compute the overall built-in expression result type!
        let result = match contract {
            CallbackContract::Void => Ty::VOID,
            CallbackContract::Mapped { keep_holes } => {
                if cb_ret == Ty::BOTTOM {
                    Ty::BOTTOM
                } else if self.types.is_open(cb_ret) {
                    Ty::OPEN
                } else {
                    let elem = if keep_holes {
                        self.types.with_absences(cb_ret, Absence::Undefined)
                    } else {
                        cb_ret
                    };
                    self.types.array(elem)
                }
            }
            CallbackContract::FlatMapped => {
                if cb_ret == Ty::BOTTOM {
                    Ty::BOTTOM
                } else if self.types.is_open(cb_ret) {
                    Ty::OPEN
                } else if matches!(self.types.kind(cb_ret), TyKind::Array(_)) {
                    cb_ret
                } else {
                    self.types.array(cb_ret)
                }
            }
            CallbackContract::Comparator => recv_ty,
            CallbackContract::Replacement => Ty::STRING,
            CallbackContract::Fold => {
                if cb_ret == Ty::BOTTOM {
                    Ty::BOTTOM
                } else if !self.types.is_open(cb_ret) && cb_ret != Ty::VOID {
                    cb_ret
                } else {
                    inputs.first().copied().unwrap_or(Ty::OPEN)
                }
            }
            CallbackContract::GroupKey => {
                if cb_ret == Ty::BOTTOM {
                    Ty::BOTTOM
                } else {
                    let key = if self.types.is_open(cb_ret) {
                        Ty::STRING
                    } else {
                        cb_ret
                    };
                    let elem = map_group_elem.unwrap_or(Ty::OPEN);
                    let arr = self.types.array(elem);
                    let base = shapes::TypeId::from_owned(self.ctx, HirType::Named("Map".into()));
                    self.types.generic(base, &[key, arr])
                }
            }
            CallbackContract::Predicate => match expr {
                Expr::ArrayFilter { .. } => recv_ty,
                Expr::ArraySome { .. } | Expr::ArrayEvery { .. } => Ty::BOOLEAN,
                Expr::ArrayFindIndex { .. } => Ty::NUMBER,
                Expr::ArrayFind { .. } => {
                    let elem = builtins::iterated_element(self.ctx, self.types, recv_ty)
                        .unwrap_or(Ty::OPEN);
                    self.types.with_absences(elem, Absence::Undefined)
                }
                Expr::Call { callee, .. } => {
                    if let Expr::PropertyGet { property, .. } = callee.as_ref() {
                        match hir_compat::method(property).name() {
                            "filter" => recv_ty,
                            "some" | "every" => Ty::BOOLEAN,
                            "findIndex" | "findLastIndex" => Ty::NUMBER,
                            "find" | "findLast" => {
                                let elem =
                                    builtins::iterated_element(self.ctx, self.types, recv_ty)
                                        .unwrap_or(Ty::OPEN);
                                self.types.with_absences(elem, Absence::Undefined)
                            }
                            _ => Ty::OPEN,
                        }
                    } else {
                        Ty::OPEN
                    }
                }
                _ => Ty::OPEN,
            },
        };
        Ok(Some(result))
    }

    fn eval_property_get(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        object: &'h Expr,
        property: &str,
        state: &mut EvalState<'h>,
    ) -> Result<Ty> {
        if let Expr::ClassRef(class_name) = object {
            if let Some(cid) = self.index.class_by_name(class_name)
                && let Some(loc) = self.index.class_static_field_loc(cid, property)
            {
                return Ok(self.read_loc(loc, inst_id));
            }
            return Ok(Ty::OPEN);
        }
        let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
        if obj_ty == Ty::BOTTOM {
            return Ok(Ty::BOTTOM);
        }
        let (stripped, _) = self.types.strip_nullish(obj_ty);

        // Class instance field or getter!
        if let TyKind::ClassInst(cinst_id) = self.types.kind(stripped) {
            if let Some(ty) = self.read_class_field(cinst_id, property, inst_id) {
                return Ok(ty);
            }
            if let Some(cinst) = self.class_insts.get(cinst_id.0 as usize)
                && let Some(getter_tid) = self.index.class_getter(cinst.class, property)
            {
                let target = self.request_instance(InstanceReq {
                    template: getter_tid,
                    receiver: Some(cinst_id),
                    capture_owner: None,
                    args: &[],
                    call_byte_offset: 0,
                    caller: Some(inst_id),
                    is_inline_callback: false,
                })?;
                let ret_loc = self
                    .instances
                    .get(target.0 as usize)
                    .map_or(LocId(0), |i| i.return_loc);
                return Ok(self.read_loc(ret_loc, inst_id));
            }
        }

        Ok(
            builtins::property_type(self.ctx, self.decls, self.types, obj_ty, property)
                .unwrap_or(Ty::OPEN),
        )
    }

    fn read_class_field(
        &mut self,
        cinst_id: ClassInstId,
        property: &str,
        reader: InstanceId,
    ) -> Option<Ty> {
        let cinst = self.class_insts.get(cinst_id.0 as usize)?;
        let hir_idx = *self.index.class_hir_idx.get(cinst.class.0 as usize)?;
        let class = self.hir.classes.get(hir_idx)?;
        let f_idx = class.fields.iter().position(|f| f.name == property)?;
        let loc = LocId(cinst.field_loc_base + f_idx as u32);
        Some(self.read_loc(loc, reader))
    }

    fn write_class_field(&mut self, cinst_id: ClassInstId, property: &str, value: Ty) {
        let Some(cinst) = self.class_insts.get(cinst_id.0 as usize) else {
            return;
        };
        let Some(&hir_idx) = self.index.class_hir_idx.get(cinst.class.0 as usize) else {
            return;
        };
        let Some(class) = self.hir.classes.get(hir_idx) else {
            return;
        };
        if let Some(f_idx) = class.fields.iter().position(|f| f.name == property) {
            let loc = LocId(cinst.field_loc_base + f_idx as u32);
            self.write_loc(loc, value);
        }
    }

    fn eval_object_spread(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        parts: &'h [perry_hir::ir::ObjectPart],
        state: &mut EvalState<'h>,
    ) -> Result<Ty> {
        let mut props: Vec<ObjectProp> = Vec::new();
        for part in parts {
            let val_expr = part.value();
            let val_ty = self.eval_expr(inst_id, receiver, val_expr, state)?;
            if let Some(key) = hir_compat::static_property(part) {
                let name = self.types.intern_name(key);
                let norm = self
                    .types
                    .normalize_for_storage(self.ctx, self.decls, val_ty);
                if let Some(existing) = props.iter_mut().find(|p| p.name == name) {
                    existing.ty = match val_expr {
                        Expr::Null => self.types.with_absences(existing.ty, Absence::Null),
                        Expr::Undefined => {
                            self.types.with_absences(existing.ty, Absence::Undefined)
                        }
                        _ => norm,
                    };
                } else {
                    props.push(ObjectProp {
                        name,
                        ty: norm,
                        optional: false,
                        readonly: false,
                    });
                }
            } else if part.defines_key() {
                return Ok(Ty::OPEN);
            } else {
                let (stripped, _) = self.types.strip_nullish(val_ty);
                if let TyKind::Object {
                    props: src_span, ..
                } = self.types.kind(stripped)
                {
                    let src_props: Vec<ObjectProp> = self.types.object_slice(src_span).to_vec();
                    for sp in src_props {
                        if let Some(existing) = props.iter_mut().find(|p| p.name == sp.name) {
                            *existing = sp;
                        } else {
                            props.push(sp);
                        }
                    }
                }
            }
        }
        Ok(self.types.object(&mut props, None))
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_new(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        expr: &'h Expr,
        class_name: &str,
        args: &'h [Expr],
        type_args: &'h [HirType],
        byte_offset: u32,
        state: &mut EvalState<'h>,
    ) -> Result<Ty> {
        // Object literal lowered as `new __AnonShape_N(arg0, arg1, ...)`.
        if hir_compat::is_object_literal(class_name)
            && let Some(fields) = self.index.literal_fields(class_name)
        {
            let mut props = Vec::with_capacity(fields.len());
            for (field, arg) in fields.iter().zip(args) {
                let ty = self.eval_expr(inst_id, receiver, arg, state)?;
                let ty = self.types.normalize_for_storage(self.ctx, self.decls, ty);
                props.push(ObjectProp {
                    name: self.types.intern_name(&field.name),
                    ty,
                    optional: false,
                    readonly: false,
                });
            }
            return Ok(self.types.object(&mut props, None));
        }

        let mut arg_tys = Vec::with_capacity(args.len());
        for arg in args {
            arg_tys.push(self.eval_expr(inst_id, receiver, arg, state)?);
        }

        // User class (explicit generic, implicit generic, or plain class).
        if let Some(cid) = self.index.class_by_name(class_name) {
            if arg_tys.contains(&Ty::BOTTOM) {
                return Ok(Ty::BOTTOM);
            }
            let class_args = if !type_args.is_empty() {
                let type_bindings = self.active_instance_type_bindings(inst_id);
                type_args
                    .iter()
                    .map(|ta| {
                        let t = self.types.intern_hir(self.ctx, ta, true);
                        self.types.substitute(self.ctx, t, &type_bindings)
                    })
                    .collect::<Vec<_>>()
            } else if let Some(tps) = self.class_type_params.get(cid.0 as usize)
                && !tps.is_empty()
            {
                let tps = tps.clone();
                let mut bindings = Vec::new();
                if let Some(ctor_tid) = self.index.class_ctor.get(cid.0 as usize).copied().flatten()
                    && let Some(ctor_view) = self.index.view(ctor_tid)
                {
                    for (param, &arg_ty) in ctor_view.params.iter().zip(&arg_tys) {
                        let pat = self.types.intern_hir(self.ctx, &param.ty, true);
                        self.types.unify(self.ctx, pat, arg_ty, &tps, &mut bindings);
                    }
                }
                tps.iter()
                    .map(|tp_id| {
                        bindings
                            .iter()
                            .find(|(k, _)| k == tp_id)
                            .map_or(Ty::OPEN, |(_, v)| *v)
                    })
                    .collect()
            } else if self
                .index
                .class_has_implicit_params
                .contains(cid.0 as usize)
            {
                arg_tys.clone()
            } else {
                Vec::new()
            };

            let cinst = self.request_class_inst(cid, &class_args, byte_offset, Some(inst_id))?;
            let strip_mask = callable_arg_mask(self.types, &arg_tys);
            self.record_site(
                inst_id,
                expr,
                SiteResolution::ClassNew {
                    class_inst: cinst,
                    strip_arg_mask: strip_mask,
                },
            );
            return Ok(self.types.class_inst(cinst));
        }

        if class_name == "Array" {
            return Ok(self.types.array(Ty::OPEN));
        }
        Ok(self.types.named(self.ctx, class_name))
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_call(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        expr: &'h Expr,
        callee: &'h Expr,
        args: &'h [Expr],
        type_args: &'h [HirType],
        byte_offset: u32,
        state: &mut EvalState<'h>,
    ) -> Result<Ty> {
        // 1. `structuredClone(x)`
        if matches!(callee, Expr::ExternFuncRef { name, .. } if name == "structuredClone") {
            return match args.first() {
                Some(arg) => self.eval_expr(inst_id, receiver, arg, state),
                None => Ok(Ty::OPEN),
            };
        }

        // 2. Foreign (WIT) imported function call.
        if let Expr::ExternFuncRef {
            name, return_type, ..
        } = callee
        {
            for arg in args {
                self.eval_expr(inst_id, receiver, arg, state)?;
            }
            if let Some((_, sig)) = self.decls.foreign_functions.get(name.as_str()) {
                return Ok(self.types.intern_hir(self.ctx, &sig.return_type, true));
            }
            return Ok(self.types.intern_hir(self.ctx, return_type, true));
        }

        // 3. Method call: `object.property(args...)`
        if let Expr::PropertyGet {
            object, property, ..
        } = callee
        {
            return self.eval_method_call(
                inst_id,
                receiver,
                expr,
                object,
                property,
                args,
                byte_offset,
                state,
            );
        }

        // 4. Direct or indirect call through a static function identity (`fn(args...)`).
        let callee_ty = self.eval_expr(inst_id, receiver, callee, state)?;
        let mut arg_tys = Vec::with_capacity(args.len());
        for arg in args {
            arg_tys.push(self.eval_expr(inst_id, receiver, arg, state)?);
        }
        if callee_ty == Ty::BOTTOM || arg_tys.contains(&Ty::BOTTOM) {
            return Ok(Ty::BOTTOM);
        }

        let TyKind::Func(ident) = self.types.kind(callee_ty) else {
            return Ok(Ty::OPEN);
        };
        let Some(target_view) = self.index.view(ident.template) else {
            return Ok(Ty::OPEN);
        };

        // Check `captures_this` / `captures_new_target` on callback arguments passed to user fns.
        for arg in args {
            if let Expr::Closure {
                captures_this,
                captures_new_target,
                ..
            } = arg
                && (*captures_this || *captures_new_target)
            {
                return Err(self.metadata.locations.annotate_call(
                    Diagnostic::unsupported(
                        "a callback that captures this or new.target requires its original receiver",
                    ),
                    byte_offset,
                ));
            }
        }

        // Check explicit `FunctionType` parameter annotations and escaping callback params!
        self.check_callback_params(inst_id, target_view, &mut arg_tys, byte_offset)?;

        // Bind explicit type arguments (`fn<number>(x)`).
        let _ = type_args;

        let target_inst = self.request_instance(InstanceReq {
            template: ident.template,
            receiver: None,
            capture_owner: ident.capture_owner,
            args: &arg_tys,
            call_byte_offset: byte_offset,
            caller: Some(inst_id),
            is_inline_callback: false,
        })?;
        let strip_mask = callable_arg_mask(self.types, &arg_tys);
        self.record_site(
            inst_id,
            expr,
            SiteResolution::DirectCall {
                target: target_inst,
                strip_arg_mask: strip_mask,
            },
        );
        // Propagate closure-written locals so reads after `set("hi")` know `v` is initialized (S3).
        if let Some(target_written) = self.inst_written.get(target_inst.0 as usize).cloned()
            && let Some(cur_written) = self.inst_written.get_mut(inst_id.0 as usize)
        {
            cur_written.union_with(&target_written);
        }
        let ret_loc = self
            .instances
            .get(target_inst.0 as usize)
            .map_or(LocId(0), |i| i.return_loc);
        Ok(self.read_loc(ret_loc, inst_id))
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_method_call(
        &mut self,
        inst_id: InstanceId,
        receiver: Option<ClassInstId>,
        expr: &'h Expr,
        object: &'h Expr,
        property: &str,
        args: &'h [Expr],
        byte_offset: u32,
        state: &mut EvalState<'h>,
    ) -> Result<Ty> {
        let method = hir_compat::method(property);
        let name = method.name();

        // Global static calls: `Promise.allSettled`, `performance.now`, etc.
        if let Expr::PropertyGet {
            object: global,
            property: class,
            ..
        } = object
            && matches!(global.as_ref(), Expr::GlobalGet(_))
        {
            let mut arg_tys = Vec::with_capacity(args.len());
            for arg in args {
                arg_tys.push(self.eval_expr(inst_id, receiver, arg, state)?);
            }
            let mut idx = 0usize;
            if let Some(res) = builtins::static_global_method(
                self.ctx,
                self.types,
                class,
                name,
                args,
                &mut |_, _| {
                    let t = arg_tys.get(idx).copied().unwrap_or(Ty::OPEN);
                    idx += 1;
                    t
                },
            ) {
                return Ok(res);
            }
            return Ok(Ty::OPEN);
        }

        // Temporal.* static calls.
        if let Expr::PropertyGet {
            object: ns,
            property: class,
            ..
        } = object
            && let Expr::PropertyGet {
                object: global,
                property: ns_name,
                ..
            } = ns.as_ref()
            && matches!(global.as_ref(), Expr::GlobalGet(_))
            && ns_name == "Temporal"
        {
            for arg in args {
                self.eval_expr(inst_id, receiver, arg, state)?;
            }
            return Ok(
                builtins::temporal_static_method(self.ctx, self.types, class, property)
                    .unwrap_or(Ty::OPEN),
            );
        }

        // `ClassName.staticMethod(args...)`
        if let Expr::ClassRef(class_name) = object {
            let mut arg_tys = Vec::with_capacity(args.len());
            for arg in args {
                arg_tys.push(self.eval_expr(inst_id, receiver, arg, state)?);
            }
            if arg_tys.contains(&Ty::BOTTOM) {
                return Ok(Ty::BOTTOM);
            }
            if let Some(cid) = self.index.class_by_name(class_name)
                && let Some(tid) = self.index.class_static_method(cid, name)
            {
                let target = self.request_instance(InstanceReq {
                    template: tid,
                    receiver: None,
                    capture_owner: None,
                    args: &arg_tys,
                    call_byte_offset: byte_offset,
                    caller: Some(inst_id),
                    is_inline_callback: false,
                })?;
                let strip_mask = callable_arg_mask(self.types, &arg_tys);
                self.record_site(
                    inst_id,
                    expr,
                    SiteResolution::MethodCall {
                        target,
                        strip_arg_mask: strip_mask,
                    },
                );
                let ret_loc = self
                    .instances
                    .get(target.0 as usize)
                    .map_or(LocId(0), |i| i.return_loc);
                return Ok(self.read_loc(ret_loc, inst_id));
            }
            return Ok(Ty::OPEN);
        }

        let obj_ty = self.eval_expr(inst_id, receiver, object, state)?;
        let mut arg_tys = Vec::with_capacity(args.len());
        for arg in args {
            arg_tys.push(self.eval_expr(inst_id, receiver, arg, state)?);
        }
        if obj_ty == Ty::BOTTOM {
            return Ok(Ty::BOTTOM);
        }

        // Open empty-array `.push(v1, v2, ...)`!
        if name == "push"
            && let Expr::LocalGet(id) = object
            && let Some(lidx) = self.index.local_idx(*id)
            && let Some((agg_loc, AggregateKind::Array)) = self.lookup_agg(inst_id, lidx)
        {
            for &arg_ty in &arg_tys {
                let norm = self
                    .types
                    .normalize_for_storage(self.ctx, self.decls, arg_ty);
                self.write_loc(agg_loc, norm);
            }
            return Ok(Ty::NUMBER);
        }

        if method == hir_compat::Method::Iteration {
            return Ok(obj_ty);
        }

        let (stripped, _) = self.types.strip_nullish(obj_ty);

        // User class method call (`new Box(2).map(...)`, `c.add(2)`, etc.)!
        if let TyKind::ClassInst(cinst_id) = self.types.kind(stripped)
            && let Some(cinst) = self.class_insts.get(cinst_id.0 as usize)
            && let Some(method_tid) = self.index.class_method(cinst.class, name)
        {
            if arg_tys.contains(&Ty::BOTTOM) {
                return Ok(Ty::BOTTOM);
            }
            if let Some(method_view) = self.index.view(method_tid) {
                self.check_callback_params(inst_id, method_view, &mut arg_tys, byte_offset)?;
            }
            let target = self.request_instance(InstanceReq {
                template: method_tid,
                receiver: Some(cinst_id),
                capture_owner: None,
                args: &arg_tys,
                call_byte_offset: byte_offset,
                caller: Some(inst_id),
                is_inline_callback: false,
            })?;
            let strip_mask = callable_arg_mask(self.types, &arg_tys);
            self.record_site(
                inst_id,
                expr,
                SiteResolution::MethodCall {
                    target,
                    strip_arg_mask: strip_mask,
                },
            );
            let ret_loc = self
                .instances
                .get(target.0 as usize)
                .map_or(LocId(0), |i| i.return_loc);
            return Ok(self.read_loc(ret_loc, inst_id));
        }

        // Built-in method call (Array/String/Bytes/Date/Map/Set/Response/Stream/Temporal/etc.).
        if let Some(rule) =
            builtins::lookup_method_result(self.ctx, self.types, stripped, name, false)
        {
            return Ok(builtins::apply_method_result(
                self.ctx,
                self.types,
                rule,
                MethodResultArgs {
                    receiver: obj_ty,
                    arg0: arg_tys.first().copied(),
                    arg0_expr: args.first(),
                    callback_result: None,
                    sparse_arrays: self.facts.sparse_arrays,
                },
            ));
        }

        Ok(Ty::OPEN)
    }

    fn check_callback_params(
        &mut self,
        caller: InstanceId,
        target_view: &super::boundary::TemplateView<'h>,
        arg_tys: &mut [Ty],
        byte_offset: u32,
    ) -> Result<()> {
        let fn_display = hir_compat::display_name(target_view.name);
        for (p_idx, (param, actual)) in target_view
            .params
            .iter()
            .zip(arg_tys.iter_mut())
            .enumerate()
        {
            let _ = p_idx;
            if let HirType::Function(expected) = &param.ty {
                let TyKind::Func(ident) = self.types.kind(*actual) else {
                    if !self.types.is_open(*actual) {
                        return Err(self.metadata.locations.annotate_call(
                            diag!(
                                codes::TYPE_MISMATCH,
                                "parameter '{}' of {fn_display} requires a statically known function",
                                param.name
                            ),
                            byte_offset,
                        ));
                    }
                    continue;
                };
                if let Some(lidx) = self.index.local_idx(param.id)
                    && let Some(span) = self
                        .index
                        .local_value_use
                        .get(lidx.0 as usize)
                        .copied()
                        .flatten()
                {
                    return Err(self.metadata.locations.annotate(
                        Diagnostic::unsupported(format!(
                            "callback '{}' of {fn_display} escapes its static call site",
                            param.name
                        )),
                        span,
                    ));
                }
                let Some(cb_view) = self.index.view(ident.template) else {
                    continue;
                };
                let mismatch = || {
                    self.metadata.locations.annotate_call(
                        diag!(
                            codes::TYPE_MISMATCH,
                            "callback does not match parameter '{}' of {fn_display}",
                            param.name
                        ),
                        byte_offset,
                    )
                };
                let required = cb_view
                    .params
                    .iter()
                    .filter(|p| p.default.is_none() && !p.is_rest)
                    .count();
                if cb_view.is_async != expected.is_async
                    || cb_view.is_generator != expected.is_generator
                    || required > expected.params.len()
                {
                    return Err(mismatch());
                }
                let mut expected_param_tys = Vec::with_capacity(expected.params.len());
                for (cb_p, (_, exp_p, _)) in cb_view.params.iter().zip(&expected.params) {
                    let exp_ty = self.types.intern_hir(self.ctx, exp_p, true);
                    let cb_ty = self.types.intern_hir(self.ctx, &cb_p.ty, true);
                    if !self.types.is_open(cb_ty) && !self.types.accepts(self.ctx, cb_ty, exp_ty) {
                        return Err(mismatch());
                    }
                    expected_param_tys.push(exp_ty);
                }
                // Instantiate the callback at the declared contract parameter types and check its return.
                let bound_cb = self.request_instance(InstanceReq {
                    template: ident.template,
                    receiver: None,
                    capture_owner: ident.capture_owner,
                    args: &expected_param_tys,
                    call_byte_offset: byte_offset,
                    caller: Some(caller),
                    is_inline_callback: false,
                })?;
                let exp_ret = self.types.intern_hir(self.ctx, &expected.return_type, true);
                if exp_ret != Ty::VOID {
                    self.result_contracts.push(ResultContract {
                        inst: bound_cb,
                        expected: exp_ret,
                        param_name: param.name.clone(),
                        byte_offset,
                    });
                }
            } else if matches!(self.types.kind(*actual), TyKind::Func(_))
                && let Some(lidx) = self.index.local_idx(param.id)
                && let Some(span) = self
                    .index
                    .local_value_use
                    .get(lidx.0 as usize)
                    .copied()
                    .flatten()
            {
                return Err(self.metadata.locations.annotate(
                    Diagnostic::unsupported(format!(
                        "callback '{}' of {fn_display} escapes its static call site",
                        param.name
                    )),
                    span,
                ));
            }
        }
        Ok(())
    }

    fn verify_contracts(&self) -> Result<()> {
        for contract in &self.result_contracts {
            let ret_loc = self
                .instances
                .get(contract.inst.0 as usize)
                .map_or(LocId(0), |i| i.return_loc);
            let actual = self
                .locations
                .get(ret_loc.0 as usize)
                .copied()
                .unwrap_or(Ty::BOTTOM);
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

    fn finish(mut self) -> Solved {
        // Deduplicate sites keeping the latest resolution from the final wave.
        self.sites.sort_by_key(|(inst, addr, _)| (inst.0, *addr));
        let mut deduped: Vec<(InstanceId, usize, SiteResolution)> =
            Vec::with_capacity(self.sites.len());
        for entry in self.sites {
            if let Some(last) = deduped.last_mut()
                && last.0 == entry.0
                && last.1 == entry.1
            {
                *last = entry;
            } else {
                deduped.push(entry);
            }
        }

        // Collect per-instance local resolved types.
        let mut instance_locals = Vec::new();
        for (inst_i, slots) in self.inst_locals.iter().enumerate() {
            let inst_id = InstanceId(inst_i as u32);
            for slot in slots {
                if let Some(&hir_id) = self.index.local_ids.get(slot.local.0 as usize) {
                    let ty = self
                        .locations
                        .get(slot.loc.0 as usize)
                        .copied()
                        .unwrap_or(Ty::OPEN);
                    instance_locals.push((inst_id, hir_id, ty));
                }
            }
        }

        // Collect module-level binding types (including empty-array / empty-object joins).
        let mut module_bindings = Vec::new();
        for l_idx in self.index.local_module.iter() {
            let Some(&hir_id) = self.index.local_ids.get(l_idx) else {
                continue;
            };
            let mut ty = self
                .index
                .local_loc
                .get(l_idx)
                .copied()
                .flatten()
                .and_then(|loc| self.locations.get(loc.0 as usize).copied())
                .unwrap_or(Ty::OPEN);
            if let Some((agg_loc, agg_kind)) = self.module_aggs.get(l_idx).copied().flatten() {
                let elem = self
                    .locations
                    .get(agg_loc.0 as usize)
                    .copied()
                    .unwrap_or(Ty::BOTTOM);
                if elem != Ty::BOTTOM {
                    ty = match agg_kind {
                        AggregateKind::Array => self.types.array(elem),
                        AggregateKind::Dictionary => self.types.object(&mut [], Some(elem)),
                    };
                }
            }
            module_bindings.push((hir_id, ty));
        }

        let static_fields = self
            .static_field_locs
            .iter()
            .map(|&(cid, f_idx, loc)| {
                let ty = self
                    .locations
                    .get(loc.0 as usize)
                    .copied()
                    .unwrap_or(Ty::OPEN);
                (cid, f_idx, ty)
            })
            .collect();

        let mut instance_captures = Vec::new();
        for (inst_i, caps) in self.inst_captures.into_iter().enumerate() {
            let inst_id = InstanceId(inst_i as u32);
            for cap in caps {
                instance_captures.push((inst_id, cap));
            }
        }

        Solved {
            instances: self.instances,
            instance_args: self.instance_args,
            class_insts: self.class_insts,
            locations: self.locations,
            param_locs: self.param_locs,
            instance_locals,
            sites: deduped,
            instance_captures,
            module_bindings,
            static_fields,
            class_inst_counts: self.class_counts,
            deletes_array_elements: self.deletes_array_elements,
        }
    }

    fn type_may_be_array(&self, ty: Ty) -> bool {
        match self.types.kind(ty) {
            TyKind::Array(_) | TyKind::Tuple(_) => true,
            TyKind::Union(span) => self
                .types
                .union_slice(span)
                .iter()
                .any(|&m| self.type_may_be_array(m)),
            _ => false,
        }
    }
}

fn callable_arg_mask(types: &TypeTable, args: &[Ty]) -> u64 {
    let mut mask = 0u64;
    for (i, &ty) in args.iter().enumerate() {
        if i < 64 && matches!(types.kind(ty), TyKind::Func(_)) {
            mask |= 1u64 << i;
        }
    }
    mask
}

fn constructor_init_expr(expr: &Expr) -> Option<&Expr> {
    if let Expr::New {
        class_name, args, ..
    } = expr
        && matches!(class_name.as_str(), "\0perry_Map" | "\0perry_Set")
    {
        return args.first();
    }
    let argument = match expr {
        Expr::NewDynamic { callee, args, .. }
            if matches!(&**callee, Expr::PropertyGet { object, property, .. }
                if matches!(&**object, Expr::GlobalGet(0)) && property == "Map") =>
        {
            args.first()?
        }
        Expr::MapNewFromArray(argument) | Expr::SetNewFromArray(argument) => argument,
        Expr::New {
            class_name, args, ..
        } if matches!(class_name.as_str(), "Map" | "Set") => args.first()?,
        _ => return None,
    };
    let Expr::Sequence(items) = argument else {
        return None;
    };
    let (_, rest) = items.split_first()?;
    rest.first()
        .filter(|expr| !matches!(expr, Expr::Undefined | Expr::Null))
}
