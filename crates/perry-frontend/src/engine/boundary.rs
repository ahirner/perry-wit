//! Single-pass boundary indexing over Perry HIR.
//!
//! Numbers every template (module init, functions, class members, closures), user class,
//! local binding, site expression, and initial storage location into dense `Vec`s and `BitSet`s.
pub(crate) use super::ast_scan::{SourceFacts, scan_ast, validate_ast};
use super::bitset::BitSet;
use super::types::{ClassId, LocId, LocalIdx, NameId, TemplateId, Ty, TySpan, TypeTable};
use crate::hir_compat::{self, Locations};
use perry_hir::{
    ir::{Expr, Function, Module as HirModule, Param, Stmt},
    types::{FuncId, LocalId},
    walker::{stmt_any_expr, walk_expr_children},
};
use pliron::context::Context;
use swc_common::Span;

/// What declaration a [`TemplateId`] refers to inside the borrowed [`HirModule`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TemplateKind {
    ModuleInit,
    Function {
        hir_idx: usize,
        func_id: FuncId,
    },
    ClassConstructor {
        class: ClassId,
        func_id: FuncId,
    },
    ClassMethod {
        class: ClassId,
        method_idx: usize,
        func_id: FuncId,
    },
    ClassStaticMethod {
        class: ClassId,
        method_idx: usize,
        func_id: FuncId,
    },
    ClassGetter {
        class: ClassId,
        getter_idx: usize,
        func_id: FuncId,
    },
    ClassSetter {
        class: ClassId,
        setter_idx: usize,
        func_id: FuncId,
    },
    Closure {
        parent: TemplateId,
        func_id: FuncId,
    },
}

/// Borrowed view of a template declaration inside [`HirModule`], avoiding `Rc` and `Function` clones.
pub(crate) struct TemplateView<'h> {
    pub(crate) id: TemplateId,
    pub(crate) kind: TemplateKind,
    pub(crate) name: &'h str,
    pub(crate) func_id: FuncId,
    pub(crate) params: &'h [Param],
    pub(crate) return_type: &'h perry_hir::types::Type,
    pub(crate) body: &'h [Stmt],
    pub(crate) captures: &'h [LocalId],
    pub(crate) type_params: &'h [perry_hir::types::TypeParam],
    pub(crate) is_async: bool,
    pub(crate) is_generator: bool,
    pub(crate) is_strict: bool,
    pub(crate) construct_only: bool,
    pub(crate) is_exported: bool,
    pub(crate) captures_this: bool,
    pub(crate) captures_new_target: bool,
}

impl TemplateView<'_> {
    pub(crate) fn display_name(&self) -> String {
        if self.name.is_empty() {
            format!("callback_{}", self.func_id)
        } else {
            hir_compat::display_name(self.name).to_owned()
        }
    }
}

/// Dense index of all templates, classes, locals, and initial locations built in one pass.
#[derive(Default)]
pub(crate) struct ModuleIndex<'h> {
    pub(crate) templates: Vec<TemplateKind>,
    pub(crate) views: Vec<TemplateView<'h>>,
    pub(crate) template_exported: BitSet,
    pub(crate) template_is_closure: BitSet,
    pub(crate) template_is_obj_method: BitSet,
    pub(crate) template_explicit_return: BitSet,
    /// Dense `FuncId -> TemplateId` table (indexed directly by `FuncId`).
    func_to_template: Vec<Option<TemplateId>>,
    /// Per-template numbered site count and sorted `(expr_addr, local_site)` slice in `site_entries`.
    pub(crate) template_site_counts: Vec<usize>,
    template_site_spans: Vec<TySpan>,
    site_entries: Vec<(usize, u32)>,
    /// User classes (excluding `__AnonShape_` object literal classes): `ClassId -> index in hir.classes`.
    pub(crate) class_hir_idx: Vec<usize>,
    pub(crate) class_names: Vec<&'h str>,
    class_name_ids: Vec<NameId>,
    pub(crate) class_ctor: Vec<Option<TemplateId>>,
    pub(crate) class_methods: Vec<Vec<(NameId, TemplateId)>>,
    pub(crate) class_static_methods: Vec<Vec<(NameId, TemplateId)>>,
    pub(crate) class_getters: Vec<Vec<(NameId, TemplateId)>>,
    pub(crate) class_setters: Vec<Vec<(NameId, TemplateId)>>,
    pub(crate) class_static_field_locs: Vec<Vec<(NameId, LocId)>>,
    pub(crate) class_has_implicit_params: BitSet,
    /// `hir.classes` indices that are `__AnonShape_*` object-literal classes.
    pub(crate) hir_class_is_literal: BitSet,
    /// `__AnonShape_*` literal class fields (interned name -> slice of fields).
    literal_classes: Vec<(NameId, &'h [perry_hir::ir::ClassField])>,
    /// `LocalIdx -> Hir LocalId`.
    pub(crate) local_ids: Vec<LocalId>,
    /// `LocalIdx -> declaring TemplateId`.
    pub(crate) local_owner: Vec<TemplateId>,
    /// `LocalIdx -> boundary declared Ty` (interned once).
    pub(crate) local_declared: Vec<Ty>,
    /// `LocalIdx -> initial LocId` (for module-level bindings).
    pub(crate) local_loc: Vec<Option<LocId>>,
    /// `LocalIdx -> first escaping parameter value-use span` (if any).
    pub(crate) local_value_use: Vec<Option<Span>>,
    /// `LocalIdx -> constant RegExp literal pattern` (for `str.replace(regex, cb)`).
    pub(crate) local_regex_pattern: Vec<Option<&'h str>>,
    /// Dense `Hir LocalId -> LocalIdx` table (indexed directly by `LocalId`).
    local_to_idx: Vec<Option<LocalIdx>>,
    /// Source-fact bitsets indexed by `LocalIdx`.
    pub(crate) local_inferred: BitSet,
    pub(crate) local_deferred: BitSet,
    pub(crate) local_param: BitSet,
    pub(crate) local_param_annotated: BitSet,
    pub(crate) local_module: BitSet,
    pub(crate) local_captured: BitSet,
    pub(crate) local_written_from_closure: BitSet,
    /// `LocalIdx -> template-local slot` for strictly-local (non-escaping) bindings.
    local_frame_slot: Vec<Option<u32>>,
    /// Number of strictly-local frame slots per `TemplateId`.
    template_frame_counts: Vec<usize>,
    /// Initial storage location types numbered at the boundary.
    pub(crate) location_types: Vec<Ty>,
    /// Maximum `FuncId` and `LocalId` seen in the module (for fresh emission IDs).
    pub(crate) max_func_id: FuncId,
    pub(crate) max_local_id: LocalId,
}

static VOID_HIR: perry_hir::types::Type = perry_hir::types::Type::Void;

impl<'h> ModuleIndex<'h> {
    pub(crate) fn build(
        ctx: &Context,
        hir: &'h HirModule,
        facts: &SourceFacts,
        locations: &Locations<'_>,
        types: &mut TypeTable,
    ) -> Self {
        let max_local_hint = hir.local_source_spans.keys().copied().max().unwrap_or(0);
        let mut idx = Self {
            templates: Vec::with_capacity(1 + hir.functions.len()),
            views: Vec::with_capacity(1 + hir.functions.len()),
            func_to_template: Vec::with_capacity(hir.functions.len() + 1),
            local_to_idx: vec![None; max_local_hint as usize + 1],
            max_local_id: max_local_hint,
            ..Self::default()
        };

        // Pre-index `__AnonShape_*` object literal classes so empty-object checks work during init scan.
        for (hir_idx, class) in hir.classes.iter().enumerate() {
            if hir_compat::is_object_literal(&class.name) {
                idx.hir_class_is_literal.insert(hir_idx);
                let name_id = types.intern_name(&class.name);
                idx.literal_classes.push((name_id, class.fields.as_slice()));
            }
        }

        // TemplateId(0) is always ModuleInit.
        let init_tid = idx.add_template(
            TemplateView {
                id: TemplateId(0),
                kind: TemplateKind::ModuleInit,
                name: "<init>",
                func_id: 0,
                params: &[],
                return_type: &VOID_HIR,
                body: &hir.init,
                captures: &[],
                type_params: &[],
                is_async: hir.has_top_level_await,
                is_generator: false,
                is_strict: true,
                construct_only: false,
                is_exported: false,
                captures_this: false,
                captures_new_target: false,
            },
            false,
            false,
            false,
        );
        let mut raw_sites: Vec<Vec<(usize, u32)>> = vec![Vec::new()];
        let mut scan = ScanCtx {
            ctx,
            hir,
            facts,
            types,
            raw_sites: &mut raw_sites,
        };
        idx.scan_stmts(&mut scan, &hir.init, init_tid, true);

        for (hir_idx, function) in hir.functions.iter().enumerate() {
            idx.max_func_id = idx.max_func_id.max(function.id);
            let explicit_ret = locations.has_explicit_return(&function.name);
            let tid = idx.add_function_template(
                TemplateKind::Function {
                    hir_idx,
                    func_id: function.id,
                },
                function,
                function.is_exported,
                explicit_ret,
            );
            scan.raw_sites.resize_with(idx.templates.len(), Vec::new);
            idx.set_func_template(function.id, tid);
            idx.scan_function(&mut scan, function, tid);
        }

        for (hir_idx, class) in hir.classes.iter().enumerate() {
            if idx.hir_class_is_literal.contains(hir_idx) {
                continue;
            }
            let cid = ClassId(idx.class_hir_idx.len() as u32);
            let cname = scan.types.intern_name(&class.name);
            idx.class_hir_idx.push(hir_idx);
            idx.class_names.push(class.name.as_str());
            idx.class_name_ids.push(cname);
            idx.class_ctor.push(None);
            idx.class_methods.push(Vec::new());
            idx.class_static_methods.push(Vec::new());
            idx.class_getters.push(Vec::new());
            idx.class_setters.push(Vec::new());
            idx.class_static_field_locs.push(Vec::new());

            for field in &class.static_fields {
                let ty = scan.types.intern_hir(scan.ctx, &field.ty, true);
                let loc = idx.add_location(ty);
                let fname = scan.types.intern_name(&field.name);
                if let Some(list) = idx.class_static_field_locs.get_mut(cid.0 as usize) {
                    list.push((fname, loc));
                }
                if let Some(init) = &field.init {
                    idx.scan_expr(&mut scan, init, init_tid);
                }
            }
            for field in &class.fields {
                if let Some(init) = &field.init {
                    idx.scan_expr(&mut scan, init, init_tid);
                }
            }

            if let Some(ctor) = &class.constructor {
                let tid = idx.register_member(
                    &mut scan,
                    TemplateKind::ClassConstructor {
                        class: cid,
                        func_id: ctor.id,
                    },
                    ctor,
                    false,
                );
                idx.class_ctor[cid.0 as usize] = Some(tid);
                if ctor
                    .params
                    .iter()
                    .any(|p| idx.param_is_unannotated(scan.hir, scan.facts, p))
                {
                    idx.class_has_implicit_params.insert(cid.0 as usize);
                }
            }

            for (method_idx, method) in class.methods.iter().enumerate() {
                let key = format!("{}::{}", class.name, method.name);
                let tid = idx.register_member(
                    &mut scan,
                    TemplateKind::ClassMethod {
                        class: cid,
                        method_idx,
                        func_id: method.id,
                    },
                    method,
                    locations.has_explicit_return(&key),
                );
                let mname = scan
                    .types
                    .intern_name(hir_compat::method(&method.name).name());
                idx.class_methods[cid.0 as usize].push((mname, tid));
                if method
                    .params
                    .iter()
                    .any(|p| idx.param_is_unannotated(scan.hir, scan.facts, p))
                {
                    idx.class_has_implicit_params.insert(cid.0 as usize);
                }
            }

            for (method_idx, method) in class.static_methods.iter().enumerate() {
                let key = format!("{}::{}", class.name, method.name);
                let tid = idx.register_member(
                    &mut scan,
                    TemplateKind::ClassStaticMethod {
                        class: cid,
                        method_idx,
                        func_id: method.id,
                    },
                    method,
                    locations.has_explicit_return(&key),
                );
                let mname = scan
                    .types
                    .intern_name(hir_compat::method(&method.name).name());
                idx.class_static_methods[cid.0 as usize].push((mname, tid));
            }

            for (getter_idx, (name, getter)) in class.getters.iter().enumerate() {
                let key = format!("{}::get_{name}", class.name);
                let tid = idx.register_member(
                    &mut scan,
                    TemplateKind::ClassGetter {
                        class: cid,
                        getter_idx,
                        func_id: getter.id,
                    },
                    getter,
                    locations.has_explicit_return(&key),
                );
                let gname = scan.types.intern_name(name);
                idx.class_getters[cid.0 as usize].push((gname, tid));
            }

            for (setter_idx, (name, setter)) in class.setters.iter().enumerate() {
                let key = format!("{}::set_{name}", class.name);
                let tid = idx.register_member(
                    &mut scan,
                    TemplateKind::ClassSetter {
                        class: cid,
                        setter_idx,
                        func_id: setter.id,
                    },
                    setter,
                    locations.has_explicit_return(&key),
                );
                let sname = scan.types.intern_name(name);
                idx.class_setters[cid.0 as usize].push((sname, tid));
            }
        }

        // Pack per-template numbered sites into flat sorted slices for O(1) solver/emitter tables.
        raw_sites.resize_with(idx.templates.len(), Vec::new);
        idx.template_site_counts = Vec::with_capacity(idx.templates.len());
        idx.template_site_spans = Vec::with_capacity(idx.templates.len());
        for mut list in raw_sites {
            list.sort_unstable_by_key(|&(addr, _)| addr);
            list.dedup_by_key(|entry| entry.0);
            let count = list.len();
            for (local_idx, entry) in list.iter_mut().enumerate() {
                entry.1 = local_idx as u32;
            }
            let start = idx.site_entries.len() as u32;
            idx.site_entries.extend(list);
            idx.template_site_counts.push(count);
            idx.template_site_spans.push(TySpan {
                start,
                len: count as u32,
            });
        }

        // Assign dense per-template frame slots to strictly-local bindings (non-module,
        // non-captured, non-parameter) so the solver never allocates global locations or CSR
        // dependency edges for values that cannot escape their enclosing instance.
        idx.local_frame_slot = vec![None; idx.local_ids.len()];
        idx.template_frame_counts = vec![0; idx.templates.len()];
        for lidx in 0..idx.local_ids.len() {
            if !idx.local_module.contains(lidx)
                && !idx.local_captured.contains(lidx)
                && !idx.local_param.contains(lidx)
            {
                let owner = idx.local_owner[lidx].0 as usize;
                let slot = idx.template_frame_counts[owner];
                idx.template_frame_counts[owner] = slot + 1;
                idx.local_frame_slot[lidx] = Some(slot as u32);
            }
        }

        idx
    }

    pub(crate) fn local_frame_slot(&self, local: LocalIdx) -> Option<usize> {
        self.local_frame_slot
            .get(local.0 as usize)
            .copied()
            .flatten()
            .map(|s| s as usize)
    }

    pub(crate) fn template_frame_count(&self, template: TemplateId) -> usize {
        self.template_frame_counts
            .get(template.0 as usize)
            .copied()
            .unwrap_or(0)
    }

    fn set_func_template(&mut self, func_id: FuncId, tid: TemplateId) {
        let idx = func_id as usize;
        if idx >= self.func_to_template.len() {
            self.func_to_template.resize(idx + 1, None);
        }
        self.func_to_template[idx] = Some(tid);
    }

    pub(crate) fn site_count(&self, template: TemplateId) -> usize {
        self.template_site_counts
            .get(template.0 as usize)
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn site_of(&self, template: TemplateId, expr: &Expr) -> Option<usize> {
        let span = *self.template_site_spans.get(template.0 as usize)?;
        let entries = &self.site_entries[span.range()];
        let addr = std::ptr::from_ref(expr) as usize;
        let pos = entries.binary_search_by_key(&addr, |&(a, _)| a).ok()?;
        Some(entries[pos].1 as usize)
    }

    pub(crate) fn view(&self, id: TemplateId) -> Option<&TemplateView<'h>> {
        self.views.get(id.0 as usize)
    }

    pub(crate) fn class_by_name_id(&self, name: NameId) -> Option<ClassId> {
        self.class_name_ids
            .iter()
            .position(|&n| n == name)
            .map(|i| ClassId(i as u32))
    }

    pub(crate) fn class_by_name(&self, name: &str) -> Option<ClassId> {
        self.class_names
            .iter()
            .position(|&n| n == name)
            .map(|i| ClassId(i as u32))
    }

    fn lookup_class_entry<T: Copy>(
        table: &[Vec<(NameId, T)>],
        class: ClassId,
        name: NameId,
    ) -> Option<T> {
        table
            .get(class.0 as usize)?
            .iter()
            .find(|&&(n, _)| n == name)
            .map(|&(_, v)| v)
    }

    pub(crate) fn class_method_by_id(&self, class: ClassId, name: NameId) -> Option<TemplateId> {
        Self::lookup_class_entry(&self.class_methods, class, name)
    }

    pub(crate) fn class_static_method_by_id(
        &self,
        class: ClassId,
        name: NameId,
    ) -> Option<TemplateId> {
        Self::lookup_class_entry(&self.class_static_methods, class, name)
    }

    pub(crate) fn class_getter_by_id(&self, class: ClassId, name: NameId) -> Option<TemplateId> {
        Self::lookup_class_entry(&self.class_getters, class, name)
    }

    pub(crate) fn class_setter_by_id(&self, class: ClassId, name: NameId) -> Option<TemplateId> {
        Self::lookup_class_entry(&self.class_setters, class, name)
    }

    pub(crate) fn class_static_field_loc_by_id(
        &self,
        class: ClassId,
        name: NameId,
    ) -> Option<LocId> {
        Self::lookup_class_entry(&self.class_static_field_locs, class, name)
    }

    pub(crate) fn literal_fields_by_id(
        &self,
        name: NameId,
    ) -> Option<&'h [perry_hir::ir::ClassField]> {
        self.literal_classes
            .iter()
            .find(|&&(n, _)| n == name)
            .map(|&(_, fields)| fields)
    }

    pub(crate) fn is_empty_object_expr(&self, types: &mut TypeTable, expr: &Expr) -> bool {
        match expr {
            Expr::Object(fields) => fields.is_empty(),
            Expr::New {
                class_name, args, ..
            } => {
                if !args.is_empty() {
                    return false;
                }
                let name_id = types.intern_name(class_name);
                self.literal_fields_by_id(name_id)
                    .is_some_and(<[_]>::is_empty)
            }
            _ => false,
        }
    }

    pub(crate) fn template_of_func(&self, id: FuncId) -> Option<TemplateId> {
        self.func_to_template.get(id as usize).copied().flatten()
    }

    pub(crate) fn local_idx(&self, id: LocalId) -> Option<LocalIdx> {
        self.local_to_idx.get(id as usize).copied().flatten()
    }

    pub(crate) fn template_count(&self) -> usize {
        self.templates.len()
    }

    pub(crate) fn local_count(&self) -> usize {
        self.local_ids.len()
    }

    fn add_function_template(
        &mut self,
        kind: TemplateKind,
        function: &'h Function,
        exported: bool,
        explicit_return: bool,
    ) -> TemplateId {
        let id = TemplateId(self.templates.len() as u32);
        self.add_template(
            TemplateView {
                id,
                kind,
                name: function.name.as_str(),
                func_id: function.id,
                params: &function.params,
                return_type: &function.return_type,
                body: &function.body,
                captures: &function.captures,
                type_params: &function.type_params,
                is_async: function.is_async,
                is_generator: function.is_generator,
                is_strict: function.is_strict,
                construct_only: function.construct_only,
                is_exported: function.is_exported,
                captures_this: false,
                captures_new_target: false,
            },
            exported,
            false,
            explicit_return,
        )
    }

    fn add_template(
        &mut self,
        mut view: TemplateView<'h>,
        exported: bool,
        is_closure: bool,
        explicit_return: bool,
    ) -> TemplateId {
        let id = TemplateId(self.templates.len() as u32);
        let is_obj_method = view.name.starts_with("__obj_method_");
        view.id = id;
        self.templates.push(view.kind);
        self.views.push(view);
        if exported {
            self.template_exported.insert(id.0 as usize);
        }
        if is_closure {
            self.template_is_closure.insert(id.0 as usize);
        }
        if is_obj_method {
            self.template_is_obj_method.insert(id.0 as usize);
        }
        if explicit_return {
            self.template_explicit_return.insert(id.0 as usize);
        }
        id
    }

    fn register_member(
        &mut self,
        scan: &mut ScanCtx<'h, '_>,
        kind: TemplateKind,
        func: &'h Function,
        explicit_return: bool,
    ) -> TemplateId {
        self.max_func_id = self.max_func_id.max(func.id);
        let tid = self.add_function_template(kind, func, false, explicit_return);
        scan.raw_sites.resize_with(self.templates.len(), Vec::new);
        self.set_func_template(func.id, tid);
        self.scan_function(scan, func, tid);
        tid
    }

    fn add_location(&mut self, initial: Ty) -> LocId {
        let id = LocId(self.location_types.len() as u32);
        self.location_types.push(initial);
        id
    }

    fn ensure_local(
        &mut self,
        id: LocalId,
        owner: TemplateId,
        declared: Ty,
        hir: &HirModule,
        facts: &SourceFacts,
    ) -> LocalIdx {
        self.max_local_id = self.max_local_id.max(id);
        let lid = id as usize;
        if lid < self.local_to_idx.len()
            && let Some(existing) = self.local_to_idx[lid]
        {
            return existing;
        }
        let idx = LocalIdx(self.local_ids.len() as u32);
        self.local_ids.push(id);
        self.local_owner.push(owner);
        self.local_declared.push(declared);
        self.local_loc.push(None);
        self.local_value_use.push(None);
        self.local_regex_pattern.push(None);
        if lid >= self.local_to_idx.len() {
            self.local_to_idx.resize(lid + 1, None);
        }
        self.local_to_idx[lid] = Some(idx);

        if let Some(span) = hir.local_source_spans.get(&id) {
            if facts.is_inferred(span.start, span.end) {
                self.local_inferred.insert(idx.0 as usize);
            }
            if facts.is_deferred(span.start) {
                self.local_deferred.insert(idx.0 as usize);
            }
            if facts.is_param_annotated(span.start) {
                self.local_param_annotated.insert(idx.0 as usize);
            }
            if let Some(use_span) = facts.param_value_use(span.start)
                && let Some(slot) = self.local_value_use.get_mut(idx.0 as usize)
            {
                *slot = Some(use_span);
            }
        }
        idx
    }

    fn param_is_unannotated(&self, hir: &HirModule, facts: &SourceFacts, param: &Param) -> bool {
        let annotated = hir
            .local_source_spans
            .get(&param.id)
            .is_some_and(|span| facts.is_param_annotated(span.start));
        !annotated
            && matches!(
                param.ty,
                perry_hir::types::Type::Any | perry_hir::types::Type::Unknown
            )
    }

    fn scan_params(&mut self, scan: &mut ScanCtx<'h, '_>, params: &'h [Param], owner: TemplateId) {
        for param in params {
            let explicit = scan
                .hir
                .local_source_spans
                .get(&param.id)
                .is_some_and(|span| scan.facts.is_param_annotated(span.start));
            let ty = scan.types.intern_hir(scan.ctx, &param.ty, explicit);
            let local = self.ensure_local(param.id, owner, ty, scan.hir, scan.facts);
            self.local_param.insert(local.0 as usize);
            if let Some(default) = &param.default {
                self.scan_expr(scan, default, owner);
            }
        }
    }

    fn scan_function(
        &mut self,
        scan: &mut ScanCtx<'h, '_>,
        function: &'h Function,
        owner: TemplateId,
    ) {
        self.scan_params(scan, &function.params, owner);
        self.scan_stmts(scan, &function.body, owner, false);
    }

    fn scan_stmts(
        &mut self,
        scan: &mut ScanCtx<'h, '_>,
        stmts: &'h [Stmt],
        owner: TemplateId,
        is_module_top: bool,
    ) {
        for stmt in stmts {
            match stmt {
                Stmt::Let { id, ty, init, .. } => {
                    let explicit = scan
                        .hir
                        .local_source_spans
                        .get(id)
                        .is_some_and(|span| !scan.facts.is_inferred(span.start, span.end));
                    let declared = scan.types.intern_hir(scan.ctx, ty, explicit);
                    let local = self.ensure_local(*id, owner, declared, scan.hir, scan.facts);
                    if is_module_top {
                        self.local_module.insert(local.0 as usize);
                        let initial = if !explicit || scan.types.is_open(declared) {
                            Ty::BOTTOM
                        } else {
                            declared
                        };
                        let loc = self.add_location(initial);
                        if let Some(slot) = self.local_loc.get_mut(local.0 as usize) {
                            *slot = Some(loc);
                        }
                    }
                    if let Some(init) = init {
                        if let Expr::RegExp { pattern, .. } = init
                            && let Some(slot) = self.local_regex_pattern.get_mut(local.0 as usize)
                        {
                            *slot = Some(pattern.as_str());
                        }
                        self.scan_expr(scan, init, owner);
                    }
                }
                Stmt::If {
                    condition,
                    then_branch,
                    else_branch,
                } => {
                    self.scan_expr(scan, condition, owner);
                    self.scan_stmts(scan, then_branch, owner, false);
                    if let Some(else_branch) = else_branch {
                        self.scan_stmts(scan, else_branch, owner, false);
                    }
                }
                Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                    self.scan_expr(scan, condition, owner);
                    self.scan_stmts(scan, body, owner, false);
                }
                Stmt::For {
                    init,
                    condition,
                    update,
                    body,
                } => {
                    if let Some(init) = init {
                        self.scan_stmts(scan, std::slice::from_ref(init), owner, false);
                    }
                    if let Some(condition) = condition {
                        self.scan_expr(scan, condition, owner);
                    }
                    if let Some(update) = update {
                        self.scan_expr(scan, update, owner);
                    }
                    self.scan_stmts(scan, body, owner, false);
                }
                Stmt::Try {
                    body,
                    catch,
                    finally,
                    ..
                } => {
                    self.scan_stmts(scan, body, owner, false);
                    if let Some(catch) = catch {
                        if let Some((id, _)) = &catch.param {
                            let err_ty = scan.types.named(scan.ctx, "Error");
                            self.ensure_local(*id, owner, err_ty, scan.hir, scan.facts);
                        }
                        self.scan_stmts(scan, &catch.body, owner, false);
                    }
                    if let Some(finally) = finally {
                        self.scan_stmts(scan, finally, owner, false);
                    }
                }
                Stmt::Switch {
                    discriminant,
                    cases,
                } => {
                    self.scan_expr(scan, discriminant, owner);
                    for case in cases {
                        if let Some(test) = &case.test {
                            self.scan_expr(scan, test, owner);
                        }
                        self.scan_stmts(scan, &case.body, owner, false);
                    }
                }
                Stmt::Labeled { body, .. } => {
                    self.scan_stmts(scan, std::slice::from_ref(body), owner, false);
                }
                other => {
                    stmt_any_expr(other, &mut |expr| {
                        self.scan_expr(scan, expr, owner);
                        false
                    });
                }
            }
        }
    }

    fn record_site_expr(scan: &mut ScanCtx<'h, '_>, owner: TemplateId, expr: &'h Expr) {
        if let Some(list) = scan.raw_sites.get_mut(owner.0 as usize) {
            list.push((std::ptr::from_ref(expr) as usize, 0));
        }
    }

    fn scan_expr(&mut self, scan: &mut ScanCtx<'h, '_>, expr: &'h Expr, owner: TemplateId) {
        match expr {
            Expr::Call { args, .. } => {
                Self::record_site_expr(scan, owner, expr);
                for arg in args {
                    Self::record_site_expr(scan, owner, arg);
                }
            }
            Expr::CallSpread { .. }
            | Expr::New { .. }
            | Expr::NewDynamic { .. }
            | Expr::MapNew
            | Expr::MapNewFromArray(_)
            | Expr::SetNew
            | Expr::SetNewFromArray(_)
            | Expr::StaticMethodCall { .. }
            | Expr::Compare { .. }
            | Expr::FuncRef(_) => {
                Self::record_site_expr(scan, owner, expr);
            }
            Expr::ArrayMap { callback, .. }
            | Expr::ArrayFilter { callback, .. }
            | Expr::ArrayForEach { callback, .. }
            | Expr::ArrayReduce { callback, .. }
            | Expr::ArrayReduceRight { callback, .. }
            | Expr::ArraySome { callback, .. }
            | Expr::ArrayEvery { callback, .. }
            | Expr::ArrayFind { callback, .. }
            | Expr::ArrayFindIndex { callback, .. }
            | Expr::ArraySort {
                comparator: callback,
                ..
            }
            | Expr::ArrayToSorted {
                comparator: Some(callback),
                ..
            }
            | Expr::ArrayFromMapped {
                map_fn: callback, ..
            }
            | Expr::MapGroupBy {
                key_fn: callback, ..
            } => {
                Self::record_site_expr(scan, owner, callback);
            }
            Expr::LocalGet(id) => {
                self.record_local_use(*id, owner, false);
            }
            Expr::LocalSet(id, _) | Expr::Update { id, .. } => {
                self.record_local_use(*id, owner, true);
            }
            Expr::ScopedTemp { id, .. } => {
                self.ensure_local(*id, owner, Ty::OPEN, scan.hir, scan.facts);
            }
            Expr::Closure {
                func_id,
                params,
                return_type,
                body,
                captures,
                mutable_captures,
                is_async,
                is_generator,
                is_strict,
                construct_only,
                captures_this,
                captures_new_target,
                ..
            } => {
                Self::record_site_expr(scan, owner, expr);
                self.max_func_id = self.max_func_id.max(*func_id);
                let explicit_ret = !matches!(
                    return_type,
                    perry_hir::types::Type::Any | perry_hir::types::Type::Unknown
                );
                let tid = self.add_template(
                    TemplateView {
                        id: TemplateId(0),
                        kind: TemplateKind::Closure {
                            parent: owner,
                            func_id: *func_id,
                        },
                        name: "",
                        func_id: *func_id,
                        params: params.as_slice(),
                        return_type,
                        body: body.as_slice(),
                        captures: captures.as_slice(),
                        type_params: &[],
                        is_async: *is_async,
                        is_generator: *is_generator,
                        is_strict: *is_strict,
                        construct_only: *construct_only,
                        is_exported: false,
                        captures_this: *captures_this,
                        captures_new_target: *captures_new_target,
                    },
                    false,
                    true,
                    explicit_ret,
                );
                scan.raw_sites.resize_with(self.templates.len(), Vec::new);
                self.set_func_template(*func_id, tid);

                for &cap in captures {
                    if let Some(local) = self.local_idx(cap) {
                        self.local_captured.insert(local.0 as usize);
                    }
                }
                for &mcap in mutable_captures {
                    if let Some(local) = self.local_idx(mcap) {
                        self.local_captured.insert(local.0 as usize);
                        self.local_written_from_closure.insert(local.0 as usize);
                    }
                }
                self.scan_params(scan, params, tid);
                self.scan_stmts(scan, body, tid, false);
                return;
            }
            _ => {}
        }
        walk_expr_children(expr, &mut |child| {
            self.scan_expr(scan, child, owner);
        });
    }

    fn record_local_use(&mut self, id: LocalId, reader: TemplateId, is_write: bool) {
        self.max_local_id = self.max_local_id.max(id);
        let Some(local) = self.local_idx(id) else {
            return;
        };
        let declared_owner = self.local_owner.get(local.0 as usize).copied();
        if declared_owner.is_some_and(|owner| owner != reader) {
            self.local_captured.insert(local.0 as usize);
            if is_write {
                self.local_written_from_closure.insert(local.0 as usize);
            }
        }
    }
}

struct ScanCtx<'h, 'a> {
    ctx: &'a Context,
    hir: &'h HirModule,
    facts: &'a SourceFacts,
    types: &'a mut TypeTable,
    raw_sites: &'a mut Vec<Vec<(usize, u32)>>,
}
