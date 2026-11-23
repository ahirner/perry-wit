//! Worklist monomorphization and storage-join solver over dense indices.
//!
//! Layout:
//! - One `locations: Vec<Ty>` of monotone joins across locals, module bindings, class fields,
//!   empty aggregates, and instance returns.
//! - One CSR compressed reader adjacency `(reader_offsets, reader_instances)` built from recorded
//!   read dependencies `(LocId, InstanceId)`.
//! - One `worklist: Vec<InstanceId>` guarded by a `queued: BitSet`.
//! - One `FlatHashIndex` over `instances` + `instance_args` for `(TemplateId, receiver, args)`.
mod calls;
mod eval;
mod expr;
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
    /// For class methods with multiple specializations, the method name on the emitted class.
    pub(crate) emitted_method_name: String,
}

/// Concrete monomorphic class instantiation `(ClassId, ctor/type args)`.
pub(crate) struct ClassInst {
    pub(crate) class: ClassId,
    pub(crate) args: TySpan,
    pub(crate) field_loc_base: u32,
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
}

/// Output of the solver handed to `emit.rs` after borrowing of `&HirModule` ends.
pub(crate) struct Solved {
    pub(crate) instances: Vec<Instance>,
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
    /// Instances reachable from roots through final-wave sites (prunes stale transient wave clones).
    pub(crate) live_instances: BitSet,
    /// Module-level resolved binding types `(LocalId, Ty)`.
    pub(crate) module_bindings: Vec<(LocalId, Ty)>,
    /// Class static field resolved types `(ClassId, field_idx, Ty)`.
    pub(crate) static_fields: Vec<(ClassId, usize, Ty)>,
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
    explicit_type_args: &'a [Ty],
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
    instance_type_args: Vec<Ty>,
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
    /// Aggregate `LocId`s whose elements/properties were read (gates non-primitive dict inference).
    agg_element_reads: BitSet,
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
            instance_type_args: Vec::new(),
            instance_index: FlatHashIndex::with_capacity(64),
            template_counts: vec![0; template_count],
            param_locs: Vec::new(),
            inst_locals: Vec::new(),
            inst_aggs: Vec::new(),
            inst_written: Vec::new(),
            inst_captures: Vec::new(),
            module_aggs,
            agg_element_reads: BitSet::default(),
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
        if old == value {
            return false;
        }
        // Guard against cyclic self-nesting writes (e.g. `const a = []; a.push(a)`):
        // if `old` is a composite type and `value` strictly nests `old` inside a composite
        // type, widen to `Ty::OPEN` so the join lattice height stays bounded.
        let old_is_composite = matches!(
            self.types.kind(old),
            TyKind::Array(_)
                | TyKind::Object { .. }
                | TyKind::Tuple(_)
                | TyKind::Generic { .. }
                | TyKind::Promise(_)
        );
        let value = if old_is_composite && self.types.contains_ty(value, old) {
            Ty::OPEN
        } else {
            value
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

    fn set_loc_exact(&mut self, loc: LocId, value: Ty) {
        if let Some(slot) = self.locations.get_mut(loc.0 as usize)
            && *slot != value
        {
            *slot = value;
            self.dirty_locs.insert(loc.0 as usize);
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
            explicit_type_args: &[],
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
                explicit_type_args: &[],
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
        explicit_type_args: &[Ty],
    ) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        template.hash(&mut h);
        receiver.hash(&mut h);
        capture_owner.hash(&mut h);
        args.hash(&mut h);
        explicit_type_args.hash(&mut h);
        h.finish()
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
        // For monomorphic plain functions (no type params, no unannotated/callable/rest/default
        // params, no capture owner or class receiver), canonicalize incoming arguments to the
        // declared parameter types so structurally-typed callers (e.g. `{ value: n }` passed to
        // `export function read(cell: Cell)`) reuse the single canonical instance instead of
        // emitting duplicate symbols.
        let canonical_args_buf: Vec<Ty>;
        let args = if receiver.is_none()
            && capture_owner.is_none()
            && explicit_type_args.is_empty()
            && !args
                .iter()
                .any(|&t| matches!(self.types.kind(t), TyKind::Func(_)))
            && let Some(view) = self.index.view(template)
            && matches!(view.kind, TemplateKind::Function { .. })
            && view.type_params.is_empty()
            && !view.params.is_empty()
            && view.params.iter().all(|p| {
                !matches!(p.ty, HirType::Any | HirType::Unknown | HirType::Function(_))
                    && !p.is_rest
                    && p.default.is_none()
            }) {
            let mut declared_args = Vec::with_capacity(view.params.len());
            for param in view.params {
                let mut ty = self.types.intern_hir(self.ctx, &param.ty, true);
                ty = self.instantiate_annotated_class(ty, 0, None)?;
                declared_args.push(ty);
            }
            canonical_args_buf = declared_args;
            canonical_args_buf.as_slice()
        } else {
            args
        };
        let hash =
            Self::hash_instance_key(template, receiver, capture_owner, args, explicit_type_args);
        if let Some(id) = self.instance_index.find(hash, |idx| {
            self.instances.get(idx as usize).is_some_and(|inst| {
                inst.template == template
                    && inst.receiver == receiver
                    && inst.capture_owner == capture_owner
                    && self.instance_args.get(inst.args.range()) == Some(args)
                    && self.instance_type_args.get(inst.explicit_type_args.range())
                        == Some(explicit_type_args)
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
        let targ_start = self.instance_type_args.len() as u32;
        self.instance_type_args
            .extend_from_slice(explicit_type_args);
        let explicit_type_args_span = TySpan {
            start: targ_start,
            len: explicit_type_args.len() as u32,
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

        // Compute explicit type-parameter substitutions for generic functions/methods/classes,
        // including type parameters inherited from enclosing `capture_owner` scopes.
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
        // Check that every own type parameter on a called generic function was bound.
        if caller.is_some()
            && !view.type_params.is_empty()
            && let Some(own_tps) = self.template_type_params.get(template.0 as usize)
        {
            for (tp, &tp_id) in view.type_params.iter().zip(own_tps.iter()) {
                let bound = type_bindings.iter().any(|(id, _)| *id == tp_id);
                if !bound {
                    let fn_display = hir_compat::display_name(view.name);
                    let mut err = diag!(
                        codes::MISSING_TYPE_ANNOTATION,
                        "cannot infer type argument '{}' for generic function '{fn_display}'",
                        tp.name
                    );
                    if call_byte_offset != 0 {
                        err = self.metadata.locations.annotate_call(err, call_byte_offset);
                    }
                    if let Some(caller_id) = caller {
                        err = self.attach_instantiation_chain(err, caller_id);
                    }
                    return Err(err);
                }
            }
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
                        // Parameter defaults narrow `undefined` when evaluated; keep explicit
                        // annotations intact so defaults (e.g. `suffix: string | null = null`)
                        // remain valid even when a call passes a narrower non-null argument.
                        if !self.types.is_open(declared) {
                            declared
                        } else {
                            let stripped =
                                self.types.exclude_absences(incoming, Absence::Undefined);
                            if stripped == Ty::NEVER {
                                Ty::BOTTOM
                            } else {
                                stripped
                            }
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

        let is_plain_fn = count == 0
            && matches!(view.kind, TemplateKind::Function { .. })
            && view.type_params.is_empty()
            && !args
                .iter()
                .any(|&t| matches!(self.types.kind(t), TyKind::Func(_)))
            && view.params.iter().all(|p| {
                !matches!(p.ty, HirType::Any | HirType::Unknown | HirType::Function(_))
                    && !p.is_rest
                    && p.default.is_none()
            });
        let keep_original = count == 0
            && (view.is_exported || view.name.starts_with("__obj_method_") || is_plain_fn);
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
        capture_owner: Option<InstanceId>,
        args: &[Ty],
        explicit_type_args: &[Ty],
    ) -> Vec<(shapes::TypeId, Ty)> {
        let mut bindings = Vec::new();
        // Inherit enclosing generic scope bindings (e.g. closure inside `function f<T>(x: T)`).
        if let Some(owner_id) = capture_owner {
            bindings.extend(self.active_instance_type_bindings(owner_id));
        }
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
        for (&tp_id, &explicit_ty) in tps.iter().zip(explicit_type_args) {
            if !self.types.is_open(explicit_ty) {
                bindings.push((tp_id, explicit_ty));
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
                            v.params
                                .get(idx)
                                .and_then(|p| first_type_var_name(&p.ty))
                                .or_else(|| v.type_params.get(idx).map(|tp| tp.name.as_str()))
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
                explicit_type_args: &[],
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
        let mut sites = std::mem::take(&mut self.sites);
        sites.sort_by_key(|(inst, addr, _)| (inst.0, *addr));
        let mut deduped: Vec<(InstanceId, usize, SiteResolution)> = Vec::with_capacity(sites.len());
        for entry in sites {
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
                if let Some(projected) = self.project_agg_from_elem(agg_loc, agg_kind, elem) {
                    ty = projected;
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

        let mut live_instances = BitSet::default();
        let mut live_queue = Vec::new();
        for (idx, inst) in self.instances.iter().enumerate() {
            let is_non_site_root = inst.caller.is_none()
                || self.index.view(inst.template).is_some_and(|v| {
                    matches!(
                        v.kind,
                        TemplateKind::ClassConstructor { .. }
                            | TemplateKind::ClassStaticMethod { .. }
                            | TemplateKind::ClassGetter { .. }
                            | TemplateKind::ClassSetter { .. }
                    )
                });
            if is_non_site_root && live_instances.insert(idx) {
                live_queue.push(InstanceId(idx as u32));
            }
        }
        for contract in &self.result_contracts {
            if live_instances.insert(contract.inst.0 as usize) {
                live_queue.push(contract.inst);
            }
        }
        let mut head = 0usize;
        while let Some(&caller) = live_queue.get(head) {
            head += 1;
            let start = deduped.partition_point(|(i, _, _)| i.0 < caller.0);
            for (i, _, res) in &deduped[start..] {
                if *i != caller {
                    break;
                }
                let target = match *res {
                    SiteResolution::DirectCall { target, .. }
                    | SiteResolution::MethodCall { target, .. }
                    | SiteResolution::Callback { target, .. } => Some(target),
                    SiteResolution::ClassNew { class_inst, .. } => self
                        .class_insts
                        .get(class_inst.0 as usize)
                        .and_then(|c| c.ctor_inst),
                    _ => None,
                };
                if let Some(t) = target
                    && live_instances.insert(t.0 as usize)
                {
                    live_queue.push(t);
                }
            }
        }

        Solved {
            instances: self.instances,
            class_insts: self.class_insts,
            locations: self.locations,
            param_locs: self.param_locs,
            instance_locals,
            sites: deduped,
            instance_captures,
            live_instances,
            module_bindings,
            static_fields,
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

fn first_type_var_name(ty: &HirType) -> Option<&str> {
    match ty {
        HirType::TypeVar(name) => Some(name.as_str()),
        HirType::Array(inner) | HirType::Promise(inner) => first_type_var_name(inner),
        HirType::Tuple(items) | HirType::Union(items) => items.iter().find_map(first_type_var_name),
        HirType::Generic { type_args, .. } => type_args.iter().find_map(first_type_var_name),
        _ => None,
    }
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
