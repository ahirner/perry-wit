//! Single-pass boundary indexing over Perry HIR.
pub(crate) use super::ast_scan::{SourceFacts, scan_ast, validate_ast};
use super::bitset::BitSet;
use super::types::{ClassId, IdxVec, LocId, LocalIdx, NameId, TemplateId, Ty, TySpan, TypeTable};
use crate::hir_compat::{self, Locations};
use perry_hir::{
    ir::{Expr, Function, Module as HirModule, Param, Stmt},
    types::{FuncId, LocalId},
    walker::{stmt_any_expr, walk_expr_children},
};
use pliron::context::Context;
use swc_common::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TemplateKind {
    ModuleInit,
    Function,
    ClassConstructor,
    ClassMethod,
    ClassStaticMethod(ClassId),
    ClassGetter(usize),
    ClassSetter(usize),
    Closure,
    PromiseResolve,
    PromiseReject,
}

pub(crate) struct TemplateView<'h> {
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
    pub(crate) uses_this: bool,
    pub(crate) explicit_return: bool,
}

impl<'h> TemplateView<'h> {
    pub(crate) fn display_name(&self) -> String {
        if self.name.is_empty() {
            format!("callback_{}", self.func_id)
        } else {
            hir_compat::display_name(self.name).to_owned()
        }
    }

    pub(crate) fn raw_name(&self) -> String {
        if self.name.is_empty() {
            format!("callback_{}", self.func_id)
        } else {
            self.name.to_owned()
        }
    }

    fn from_function(kind: TemplateKind, f: &'h Function, explicit_return: bool) -> Self {
        Self {
            kind,
            name: f.name.as_str(),
            func_id: f.id,
            params: &f.params,
            return_type: &f.return_type,
            body: &f.body,
            captures: &f.captures,
            type_params: &f.type_params,
            is_async: f.is_async,
            is_generator: f.is_generator,
            is_strict: f.is_strict,
            construct_only: f.construct_only,
            is_exported: f.is_exported,
            captures_this: false,
            captures_new_target: false,
            uses_this: stmts_use_this(&f.body),
            explicit_return,
        }
    }
}

pub(crate) struct ClassInfo {
    pub(crate) hir_idx: usize,
    pub(crate) name_id: NameId,
    pub(crate) ctor: Option<TemplateId>,
    pub(crate) methods: Vec<(NameId, TemplateId)>,
    pub(crate) static_methods: Vec<(NameId, TemplateId)>,
    pub(crate) getters: Vec<(NameId, TemplateId)>,
    pub(crate) setters: Vec<(NameId, TemplateId)>,
    pub(crate) static_field_locs: Vec<(NameId, LocId)>,
}

impl ClassInfo {
    pub(crate) fn find<T: Copy>(table: &[(NameId, T)], name: NameId) -> Option<T> {
        table.iter().find(|&&(k, _)| k == name).map(|&(_, v)| v)
    }
}

#[derive(Default)]
pub(crate) struct ModuleIndex<'h> {
    pub(crate) views: IdxVec<TemplateId, TemplateView<'h>>,
    func_to_template: Vec<Option<TemplateId>>,
    template_site_spans: Vec<TySpan>,
    site_entries: Vec<(usize, u32)>,
    promise_site_templates: Vec<(usize, TemplateId, TemplateId)>,
    pub(crate) classes: IdxVec<ClassId, ClassInfo>,
    pub(crate) class_has_implicit_params: BitSet,
    pub(crate) hir_class_is_literal: BitSet,
    literal_classes: Vec<(NameId, &'h [perry_hir::ir::ClassField])>,
    pub(crate) local_ids: IdxVec<LocalIdx, LocalId>,
    pub(crate) local_owner: Vec<TemplateId>,
    pub(crate) local_declared: Vec<Ty>,
    pub(crate) local_loc: Vec<Option<LocId>>,
    pub(crate) local_value_use: Vec<Option<Span>>,
    pub(crate) local_regex_pattern: Vec<Option<&'h str>>,
    local_to_idx: Vec<Option<LocalIdx>>,
    pub(crate) local_inferred: BitSet,
    pub(crate) local_deferred: BitSet,
    pub(crate) local_for_head: BitSet,
    pub(crate) local_nullish_init: BitSet,
    pub(crate) local_reassigned: BitSet,
    pub(crate) local_param: BitSet,
    pub(crate) local_param_annotated: BitSet,
    pub(crate) local_module: BitSet,
    pub(crate) local_captured: BitSet,
    pub(crate) local_written_from_closure: BitSet,
    pub(crate) local_forward_captured: BitSet,
    pub(crate) exported_obj_methods: BitSet,
    pub(crate) capture_free_templates: BitSet,
    pub(crate) closure_in_loop: BitSet,
    pub(crate) closure_owner_template: Vec<Option<TemplateId>>,
    template_captures: Vec<Vec<LocalId>>,
    template_captured_locals: Vec<Vec<LocalIdx>>,
    local_frame_slot: Vec<Option<u32>>,
    template_frame_counts: Vec<usize>,
    pub(crate) location_types: IdxVec<LocId, Ty>,
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
            func_to_template: Vec::with_capacity(hir.functions.len() + 1),
            local_to_idx: vec![None; max_local_hint as usize + 1],
            max_local_id: max_local_hint,
            ..Self::default()
        };
        for (hir_idx, class) in hir.classes.iter().enumerate() {
            if hir_compat::is_object_literal(&class.name) {
                idx.hir_class_is_literal.insert(hir_idx);
                idx.literal_classes
                    .push((types.intern_name(&class.name), class.fields.as_slice()));
            }
        }
        let init_tid = idx.add_template(TemplateView {
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
            uses_this: false,
            explicit_return: false,
        });
        debug_assert_eq!(init_tid, TemplateId::INIT);
        let mut raw_sites: Vec<Vec<(usize, u32)>> = vec![Vec::new()];
        let mut defined_locals = BitSet::default();
        let mut pending_captures: Vec<(LocalId, bool, bool)> = Vec::new();
        let mut scan = ScanCtx {
            ctx,
            hir,
            facts,
            types,
            raw_sites: &mut raw_sites,
            defined_locals: &mut defined_locals,
            pending_captures: &mut pending_captures,
        };
        idx.scan_stmts(&mut scan, &hir.init, init_tid, true, false);

        for f in &hir.functions {
            idx.register_member(
                &mut scan,
                TemplateKind::Function,
                f,
                locations.has_explicit_return(&f.name),
            );
        }

        for (hir_idx, class) in hir.classes.iter().enumerate() {
            if idx.hir_class_is_literal.contains(hir_idx) {
                continue;
            }
            let cid = idx.classes.push(ClassInfo {
                hir_idx,
                name_id: scan.types.intern_name(&class.name),
                ctor: None,
                methods: Vec::new(),
                static_methods: Vec::new(),
                getters: Vec::new(),
                setters: Vec::new(),
                static_field_locs: Vec::new(),
            });
            let mut static_field_locs = Vec::new();
            for field in &class.static_fields {
                let ty = scan.types.intern_hir(scan.ctx, &field.ty, true);
                let loc = idx.add_location(ty);
                static_field_locs.push((scan.types.intern_name(&field.name), loc));
                if let Some(init) = &field.init {
                    Self::record_site_expr(&mut scan, init_tid, init);
                    idx.scan_expr(&mut scan, init, init_tid, false);
                }
            }
            for field in &class.fields {
                if let Some(init) = &field.init {
                    Self::record_site_expr(&mut scan, init_tid, init);
                    idx.scan_expr(&mut scan, init, init_tid, false);
                }
            }
            if class
                .constructor
                .iter()
                .chain(&class.methods)
                .flat_map(|f| &f.params)
                .any(|p| idx.param_is_unannotated(scan.hir, scan.facts, p))
            {
                idx.class_has_implicit_params.insert(cid.index());
            }
            let ctor = class
                .constructor
                .as_ref()
                .map(|c| idx.register_member(&mut scan, TemplateKind::ClassConstructor, c, false));
            let methods: Vec<_> = class
                .methods
                .iter()
                .map(|m| {
                    let tid = idx.register_member(
                        &mut scan,
                        TemplateKind::ClassMethod,
                        m,
                        locations.has_explicit_return(&format!("{}::{}", class.name, m.name)),
                    );
                    (
                        scan.types.intern_name(hir_compat::method(&m.name).name()),
                        tid,
                    )
                })
                .collect();
            let static_methods: Vec<_> = class
                .static_methods
                .iter()
                .map(|m| {
                    let tid = idx.register_member(
                        &mut scan,
                        TemplateKind::ClassStaticMethod(cid),
                        m,
                        locations.has_explicit_return(&format!("{}::{}", class.name, m.name)),
                    );
                    (
                        scan.types.intern_name(hir_compat::method(&m.name).name()),
                        tid,
                    )
                })
                .collect();
            let getters: Vec<_> = class
                .getters
                .iter()
                .enumerate()
                .map(|(i, (name, g))| {
                    let tid = idx.register_member(
                        &mut scan,
                        TemplateKind::ClassGetter(i),
                        g,
                        locations.has_explicit_return(&format!("{}::get_{name}", class.name)),
                    );
                    (scan.types.intern_name(name), tid)
                })
                .collect();
            let setters: Vec<_> = class
                .setters
                .iter()
                .enumerate()
                .map(|(i, (name, s))| {
                    let tid = idx.register_member(
                        &mut scan,
                        TemplateKind::ClassSetter(i),
                        s,
                        locations.has_explicit_return(&format!("{}::set_{name}", class.name)),
                    );
                    (scan.types.intern_name(name), tid)
                })
                .collect();
            let info = &mut idx.classes[cid];
            info.ctor = ctor;
            info.methods = methods;
            info.static_methods = static_methods;
            info.getters = getters;
            info.setters = setters;
            info.static_field_locs = static_field_locs;
        }

        raw_sites.resize_with(idx.views.len(), Vec::new);
        idx.template_site_spans = Vec::with_capacity(idx.views.len());
        for mut list in raw_sites {
            list.sort_unstable_by_key(|&(addr, _)| addr);
            list.dedup_by_key(|e| e.0);
            for (i, e) in list.iter_mut().enumerate() {
                e.1 = i as u32;
            }
            let (start, len) = (idx.site_entries.len() as u32, list.len() as u32);
            idx.site_entries.extend(list);
            idx.template_site_spans.push(TySpan { start, len });
        }

        for (cap_id, written, forward) in pending_captures {
            if let Some(lidx) = idx.local_idx(cap_id) {
                let li = lidx.index();
                idx.local_captured.insert(li);
                if written {
                    idx.local_written_from_closure.insert(li);
                }
                if forward && !idx.local_module.contains(li) && !idx.local_param.contains(li) {
                    idx.local_forward_captured.insert(li);
                }
            }
        }
        idx.local_frame_slot = vec![None; idx.local_ids.len()];
        idx.template_frame_counts = vec![0; idx.views.len()];
        idx.template_captured_locals = vec![Vec::new(); idx.views.len()];
        for (lidx_i, (&owner, slot_out)) in idx
            .local_owner
            .iter()
            .zip(idx.local_frame_slot.iter_mut())
            .enumerate()
        {
            if !idx.local_module.contains(lidx_i) && !idx.local_param.contains(lidx_i) {
                if !idx.local_captured.contains(lidx_i) {
                    if let Some(count) = idx.template_frame_counts.get_mut(owner.index()) {
                        *slot_out = Some(*count as u32);
                        *count += 1;
                    }
                } else if let Some(list) = idx.template_captured_locals.get_mut(owner.index())
                    && let Some(&lid) = idx.local_ids.as_slice().get(lidx_i)
                    && let Some(lidx) = idx.local_to_idx.get(lid as usize).copied().flatten()
                {
                    list.push(lidx);
                }
            }
        }
        let mut template_captures: Vec<Vec<LocalId>> = idx
            .views
            .iter()
            .map(|v| {
                v.captures
                    .iter()
                    .copied()
                    .filter(|&c| idx.is_non_global_local(c))
                    .collect()
            })
            .collect();
        for (tid, view) in idx.views.iter_enumerated() {
            for &cap_id in view.captures {
                let Some(lidx) = idx.local_idx(cap_id) else {
                    continue;
                };
                if idx.local_module.contains(lidx.index()) {
                    continue;
                }
                let cap_owner = idx.local_owner.get(lidx.index()).copied();
                let mut cur = idx
                    .closure_owner_template
                    .get(tid.index())
                    .copied()
                    .flatten();
                while let Some(parent) = cur {
                    if Some(parent) == cap_owner || parent == TemplateId::INIT {
                        break;
                    }
                    if let Some(list) = template_captures.get_mut(parent.index()) {
                        list.push(cap_id);
                    }
                    cur = idx
                        .closure_owner_template
                        .get(parent.index())
                        .copied()
                        .flatten();
                }
            }
        }
        for (i, caps) in template_captures.iter_mut().enumerate() {
            caps.sort_unstable();
            caps.dedup();
            let is_promise_handle = idx.views.as_slice().get(i).is_some_and(|v| {
                matches!(
                    v.kind,
                    TemplateKind::PromiseResolve | TemplateKind::PromiseReject
                )
            });
            if caps.is_empty() && !is_promise_handle {
                idx.capture_free_templates.insert(i);
            }
        }
        idx.promise_site_templates
            .sort_unstable_by_key(|&(addr, _, _)| addr);
        idx.template_captures = template_captures;
        for stmt in &hir.init {
            if let Stmt::Let {
                id,
                init: Some(init_expr),
                ..
            } = stmt
                && hir
                    .local_source_spans
                    .get(id)
                    .is_some_and(|s| facts.is_exported_var(s.start))
            {
                idx.mark_exported_obj_methods(init_expr);
            }
        }
        idx
    }

    fn mark_exported_obj_methods(&mut self, expr: &Expr) {
        match expr {
            Expr::FuncRef(fid) | Expr::Closure { func_id: fid, .. } => {
                if let Some(tid) = self.template_of_func(*fid) {
                    self.exported_obj_methods.insert(tid.index());
                }
            }
            Expr::Call { callee, args, .. } => {
                if let Expr::Closure { body, .. } = callee.as_ref() {
                    self.mark_exported_obj_methods_stmts(body);
                }
                for arg in args {
                    self.mark_exported_obj_methods(arg);
                }
            }
            _ => walk_expr_children(expr, &mut |child| self.mark_exported_obj_methods(child)),
        }
    }

    fn mark_exported_obj_methods_stmts(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            stmt_any_expr(stmt, &mut |e| {
                self.mark_exported_obj_methods(e);
                false
            });
        }
    }

    pub(crate) fn local_frame_slot(&self, local: LocalIdx) -> Option<usize> {
        self.local_frame_slot
            .get(local.index())?
            .map(|s| s as usize)
    }
    pub(crate) fn template_frame_count(&self, t: TemplateId) -> usize {
        self.template_frame_counts
            .get(t.index())
            .copied()
            .unwrap_or(0)
    }
    pub(crate) fn site_count(&self, t: TemplateId) -> usize {
        self.template_site_spans
            .get(t.index())
            .map_or(0, |s| s.len as usize)
    }
    pub(crate) fn site_of(&self, t: TemplateId, expr: &Expr) -> Option<usize> {
        let entries = self
            .site_entries
            .get(self.template_site_spans.get(t.index())?.range())?;
        let pos = entries
            .binary_search_by_key(&(std::ptr::from_ref(expr) as usize), |&(a, _)| a)
            .ok()?;
        entries.get(pos).map(|&(_, s)| s as usize)
    }
    pub(crate) fn promise_templates_of(&self, expr: &Expr) -> Option<(TemplateId, TemplateId)> {
        let addr = std::ptr::from_ref(expr) as usize;
        let pos = self
            .promise_site_templates
            .binary_search_by_key(&addr, |&(a, _, _)| a)
            .ok()?;
        self.promise_site_templates
            .get(pos)
            .map(|&(_, r, j)| (r, j))
    }
    pub(crate) fn view(&self, id: TemplateId) -> Option<&TemplateView<'h>> {
        self.views.get(id)
    }
    pub(crate) fn class(&self, id: ClassId) -> Option<&ClassInfo> {
        self.classes.get(id)
    }
    pub(crate) fn class_by_name_id(&self, name: NameId) -> Option<ClassId> {
        self.classes
            .iter_enumerated()
            .find_map(|(id, c)| (c.name_id == name).then_some(id))
    }
    pub(crate) fn literal_fields_by_id(
        &self,
        n: NameId,
    ) -> Option<&'h [perry_hir::ir::ClassField]> {
        self.literal_classes
            .iter()
            .find(|&&(k, _)| k == n)
            .map(|&(_, f)| f)
    }
    pub(crate) fn is_empty_object_expr(&self, types: &mut TypeTable, expr: &Expr) -> bool {
        match expr {
            Expr::Object(fields) => fields.is_empty(),
            Expr::New {
                class_name, args, ..
            } if args.is_empty() => {
                let id = types.intern_name(class_name);
                self.literal_fields_by_id(id).is_some_and(<[_]>::is_empty)
            }
            _ => false,
        }
    }
    pub(crate) fn is_param_annotated(&self, id: LocalId) -> bool {
        self.local_idx(id)
            .is_some_and(|li| self.local_param_annotated.contains(li.index()))
    }
    pub(crate) fn is_inferred_local(&self, hir: &HirModule, id: LocalId, lidx: LocalIdx) -> bool {
        self.local_inferred.contains(lidx.index()) || !hir.local_source_spans.contains_key(&id)
    }
    pub(crate) fn is_for_head_local(&self, id: LocalId) -> bool {
        self.local_idx(id)
            .is_some_and(|li| self.local_for_head.contains(li.index()))
    }
    pub(crate) fn is_non_global_local(&self, id: LocalId) -> bool {
        self.local_idx(id)
            .is_some_and(|li| !self.local_module.contains(li.index()))
    }
    pub(crate) fn template_captures(&self, t: TemplateId) -> &[LocalId] {
        self.template_captures
            .get(t.index())
            .map_or(&[], Vec::as_slice)
    }
    pub(crate) fn template_captured_locals(&self, t: TemplateId) -> &[LocalIdx] {
        self.template_captured_locals
            .get(t.index())
            .map_or(&[], Vec::as_slice)
    }
    pub(crate) fn template_of_func(&self, id: FuncId) -> Option<TemplateId> {
        self.func_to_template.get(id as usize).copied().flatten()
    }
    pub(crate) fn local_idx(&self, id: LocalId) -> Option<LocalIdx> {
        self.local_to_idx.get(id as usize).copied().flatten()
    }
    pub(crate) fn local_count(&self) -> usize {
        self.local_ids.len()
    }

    fn set_func_template(&mut self, func_id: FuncId, tid: TemplateId) {
        let idx = func_id as usize;
        if idx >= self.func_to_template.len() {
            self.func_to_template.resize(idx + 1, None);
        }
        if let Some(slot) = self.func_to_template.get_mut(idx) {
            *slot = Some(tid);
        }
    }

    fn add_template(&mut self, view: TemplateView<'h>) -> TemplateId {
        self.closure_owner_template.push(None);
        self.views.push(view)
    }

    fn add_promise_handle_template(
        &mut self,
        scan: &mut ScanCtx<'h, '_>,
        kind: TemplateKind,
    ) -> TemplateId {
        let name = match kind {
            TemplateKind::PromiseResolve => "resolve",
            TemplateKind::PromiseReject => "reject",
            _ => "",
        };
        let tid = self.add_template(TemplateView {
            kind,
            name,
            func_id: 0,
            params: &[],
            return_type: &VOID_HIR,
            body: &[],
            captures: &[],
            type_params: &[],
            is_async: false,
            is_generator: false,
            is_strict: true,
            construct_only: false,
            is_exported: false,
            captures_this: false,
            captures_new_target: false,
            uses_this: false,
            explicit_return: true,
        });
        scan.raw_sites.resize_with(self.views.len(), Vec::new);
        tid
    }

    fn register_member(
        &mut self,
        scan: &mut ScanCtx<'h, '_>,
        kind: TemplateKind,
        func: &'h Function,
        explicit_return: bool,
    ) -> TemplateId {
        self.max_func_id = self.max_func_id.max(func.id);
        let tid = self.add_template(TemplateView::from_function(kind, func, explicit_return));
        scan.raw_sites.resize_with(self.views.len(), Vec::new);
        self.set_func_template(func.id, tid);
        self.scan_params(scan, &func.params, tid);
        self.scan_stmts(scan, &func.body, tid, false, false);
        tid
    }

    fn add_location(&mut self, initial: Ty) -> LocId {
        self.location_types.push(initial)
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
        if let Some(existing) = self.local_to_idx.get(lid).copied().flatten() {
            return existing;
        }
        let idx = self.local_ids.push(id);
        self.local_owner.push(owner);
        self.local_declared.push(declared);
        self.local_loc.push(None);
        self.local_value_use.push(None);
        self.local_regex_pattern.push(None);
        if lid >= self.local_to_idx.len() {
            self.local_to_idx.resize(lid + 1, None);
        }
        if let Some(slot) = self.local_to_idx.get_mut(lid) {
            *slot = Some(idx);
        }
        if let Some(span) = hir.local_source_spans.get(&id) {
            if facts.is_inferred(span.start, span.end) {
                self.local_inferred.insert(idx.index());
            }
            if facts.is_deferred(span.start) {
                self.local_deferred.insert(idx.index());
            }
            if facts.is_for_head_let(span.start) {
                self.local_for_head.insert(idx.index());
            }
            if facts.is_param_annotated(span.start) {
                self.local_param_annotated.insert(idx.index());
            }
            if let Some(use_span) = facts.param_value_use(span.start)
                && let Some(slot) = self.local_value_use.get_mut(idx.index())
            {
                *slot = Some(use_span);
            }
        }
        idx
    }

    fn param_is_unannotated(&self, hir: &HirModule, facts: &SourceFacts, param: &Param) -> bool {
        !hir.local_source_spans
            .get(&param.id)
            .is_some_and(|s| facts.is_param_annotated(s.start))
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
                .is_some_and(|s| scan.facts.is_param_annotated(s.start));
            let ty = scan.types.intern_hir(scan.ctx, &param.ty, explicit);
            let local = self.ensure_local(param.id, owner, ty, scan.hir, scan.facts);
            self.local_param.insert(local.index());
            scan.defined_locals.insert(param.id as usize);
            if let Some(default) = &param.default {
                self.scan_expr(scan, default, owner, false);
            }
        }
    }

    fn scan_stmts(
        &mut self,
        scan: &mut ScanCtx<'h, '_>,
        stmts: &'h [Stmt],
        owner: TemplateId,
        is_module_top: bool,
        in_loop: bool,
    ) {
        for stmt in stmts {
            match stmt {
                Stmt::Let { id, ty, init, .. } => {
                    let explicit = scan
                        .hir
                        .local_source_spans
                        .get(id)
                        .is_some_and(|s| !scan.facts.is_inferred(s.start, s.end));
                    let declared = scan.types.intern_hir(scan.ctx, ty, explicit);
                    let local = self.ensure_local(*id, owner, declared, scan.hir, scan.facts);
                    if is_module_top && !self.local_for_head.contains(local.index()) {
                        self.local_module.insert(local.index());
                        let loc = self.add_location(Ty::BOTTOM);
                        if let Some(slot) = self.local_loc.get_mut(local.index()) {
                            *slot = Some(loc);
                        }
                    }
                    if let Some(init) = init {
                        if matches!(init, Expr::Null | Expr::Undefined) {
                            self.local_nullish_init.insert(local.index());
                        }
                        if let Expr::RegExp { pattern, .. } = init
                            && let Some(slot) = self.local_regex_pattern.get_mut(local.index())
                        {
                            *slot = Some(pattern.as_str());
                        }
                        Self::record_site_expr(scan, owner, init);
                        self.scan_expr(scan, init, owner, in_loop);
                    }
                    scan.defined_locals.insert(*id as usize);
                }
                Stmt::Return(Some(val)) => {
                    Self::record_site_expr(scan, owner, val);
                    self.scan_expr(scan, val, owner, in_loop);
                }
                Stmt::If {
                    condition,
                    then_branch,
                    else_branch,
                } => {
                    self.scan_expr(scan, condition, owner, in_loop);
                    self.scan_stmts(scan, then_branch, owner, false, in_loop);
                    if let Some(b) = else_branch {
                        self.scan_stmts(scan, b, owner, false, in_loop);
                    }
                }
                Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                    self.scan_expr(scan, condition, owner, true);
                    self.scan_stmts(scan, body, owner, false, true);
                }
                Stmt::For {
                    init,
                    condition,
                    update,
                    body,
                } => {
                    if let Some(i) = init {
                        self.scan_stmts(scan, std::slice::from_ref(i), owner, false, in_loop);
                    }
                    if let Some(c) = condition {
                        self.scan_expr(scan, c, owner, true);
                    }
                    if let Some(u) = update {
                        self.scan_expr(scan, u, owner, true);
                    }
                    self.scan_stmts(scan, body, owner, false, true);
                }
                Stmt::Try {
                    body,
                    catch,
                    finally,
                    ..
                } => {
                    self.scan_stmts(scan, body, owner, false, in_loop);
                    if let Some(catch) = catch {
                        if let Some((id, _)) = &catch.param {
                            let err_ty = scan.types.named(scan.ctx, "Error");
                            self.ensure_local(*id, owner, err_ty, scan.hir, scan.facts);
                        }
                        self.scan_stmts(scan, &catch.body, owner, false, in_loop);
                    }
                    if let Some(f) = finally {
                        self.scan_stmts(scan, f, owner, false, in_loop);
                    }
                }
                Stmt::Switch {
                    discriminant,
                    cases,
                } => {
                    self.scan_expr(scan, discriminant, owner, in_loop);
                    for case in cases {
                        if let Some(t) = &case.test {
                            self.scan_expr(scan, t, owner, in_loop);
                        }
                        self.scan_stmts(scan, &case.body, owner, false, in_loop);
                    }
                }
                Stmt::Labeled { body, .. } => {
                    self.scan_stmts(scan, std::slice::from_ref(body), owner, false, in_loop)
                }
                other => {
                    stmt_any_expr(other, &mut |e| {
                        self.scan_expr(scan, e, owner, in_loop);
                        false
                    });
                }
            }
        }
    }

    fn record_site_expr(scan: &mut ScanCtx<'h, '_>, owner: TemplateId, expr: &'h Expr) {
        if let Some(list) = scan.raw_sites.get_mut(owner.index()) {
            list.push((std::ptr::from_ref(expr) as usize, 0));
        }
    }

    fn scan_expr(
        &mut self,
        scan: &mut ScanCtx<'h, '_>,
        expr: &'h Expr,
        owner: TemplateId,
        in_loop: bool,
    ) {
        match expr {
            Expr::Call { args, .. } => {
                Self::record_site_expr(scan, owner, expr);
                for arg in args {
                    Self::record_site_expr(scan, owner, arg);
                }
            }
            Expr::ArraySpread(_) => Self::record_site_expr(scan, owner, expr),
            Expr::Array(elems, _) => {
                Self::record_site_expr(scan, owner, expr);
                for elem in elems {
                    Self::record_site_expr(scan, owner, elem);
                }
            }
            Expr::PropertySet { value, .. }
            | Expr::PutValueSet { value, .. }
            | Expr::IndexSet { value, .. }
            | Expr::ArrayPush { value, .. } => {
                Self::record_site_expr(scan, owner, expr);
                Self::record_site_expr(scan, owner, value);
            }
            Expr::Logical { left, right, .. } => {
                Self::record_site_expr(scan, owner, left);
                Self::record_site_expr(scan, owner, right);
            }
            Expr::Conditional {
                then_expr,
                else_expr,
                ..
            } => {
                Self::record_site_expr(scan, owner, then_expr);
                Self::record_site_expr(scan, owner, else_expr);
            }
            Expr::New {
                class_name, args, ..
            } => {
                Self::record_site_expr(scan, owner, expr);
                for arg in args {
                    Self::record_site_expr(scan, owner, arg);
                }
                if class_name == "Promise" {
                    let resolve_tid =
                        self.add_promise_handle_template(scan, TemplateKind::PromiseResolve);
                    let reject_tid =
                        self.add_promise_handle_template(scan, TemplateKind::PromiseReject);
                    self.promise_site_templates.push((
                        std::ptr::from_ref(expr) as usize,
                        resolve_tid,
                        reject_tid,
                    ));
                }
            }
            Expr::Object(_)
            | Expr::ObjectSpread { .. }
            | Expr::CallSpread { .. }
            | Expr::NewDynamic { .. }
            | Expr::MapNew
            | Expr::MapNewFromArray(_)
            | Expr::SetNew
            | Expr::SetNewFromArray(_)
            | Expr::StaticMethodCall { .. }
            | Expr::Compare { .. }
            | Expr::TypeOf(_)
            | Expr::FuncRef(_) => Self::record_site_expr(scan, owner, expr),
            Expr::ArrayMap { callback, .. }
            | Expr::ArrayFlatMap { callback, .. }
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
            } => Self::record_site_expr(scan, owner, callback),
            Expr::LocalGet(id) => self.record_local_use(*id, owner, false),
            Expr::LocalSet(id, value) => {
                Self::record_site_expr(scan, owner, value);
                self.record_local_use(*id, owner, true);
            }
            Expr::Update { id, .. } => self.record_local_use(*id, owner, true),
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
                let exp_ret = !matches!(
                    return_type,
                    perry_hir::types::Type::Any | perry_hir::types::Type::Unknown
                );
                let tid = self.add_template(TemplateView {
                    kind: TemplateKind::Closure,
                    name: scan
                        .hir
                        .closure_display_names
                        .get(func_id)
                        .map_or("", String::as_str),
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
                    uses_this: stmts_use_this(body),
                    explicit_return: exp_ret,
                });
                scan.raw_sites.resize_with(self.views.len(), Vec::new);
                if let Some(slot) = self.closure_owner_template.get_mut(tid.index()) {
                    *slot = Some(owner);
                }
                if in_loop {
                    self.closure_in_loop.insert(tid.index());
                }
                self.set_func_template(*func_id, tid);
                for &cap in captures {
                    let forward = !scan.defined_locals.contains(cap as usize);
                    scan.pending_captures.push((cap, false, forward));
                }
                for &cap in mutable_captures {
                    let forward = !scan.defined_locals.contains(cap as usize);
                    scan.pending_captures.push((cap, true, forward));
                }
                self.scan_params(scan, params, tid);
                self.scan_stmts(scan, body, tid, false, false);
                return;
            }
            _ => {}
        }
        walk_expr_children(expr, &mut |child| {
            self.scan_expr(scan, child, owner, in_loop)
        });
    }

    fn record_local_use(&mut self, id: LocalId, user_template: TemplateId, is_write: bool) {
        if let Some(lidx) = self.local_idx(id) {
            if is_write {
                self.local_reassigned.insert(lidx.index());
            }
            if self.local_owner.get(lidx.index()).copied() != Some(user_template) {
                self.local_captured.insert(lidx.index());
                if is_write
                    && self
                        .view(user_template)
                        .is_some_and(|v| v.kind == TemplateKind::Closure)
                {
                    self.local_written_from_closure.insert(lidx.index());
                }
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
    defined_locals: &'a mut BitSet,
    pending_captures: &'a mut Vec<(LocalId, bool, bool)>,
}

fn stmts_use_this(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| stmt_any_expr(s, &mut expr_uses_this))
}

fn expr_uses_this(expr: &Expr) -> bool {
    match expr {
        Expr::This => true,
        Expr::Closure {
            is_arrow,
            captures_this,
            body,
            ..
        } => {
            if *captures_this {
                true
            } else if *is_arrow {
                stmts_use_this(body)
            } else {
                false
            }
        }
        _ => {
            let mut found = false;
            walk_expr_children(expr, &mut |child| {
                if !found && expr_uses_this(child) {
                    found = true;
                }
            });
            found
        }
    }
}
