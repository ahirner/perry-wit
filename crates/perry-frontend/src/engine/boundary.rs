//! Single-pass boundary indexing over Perry HIR.
pub(crate) use super::ast_scan::{SourceFacts, scan_ast, validate_ast};
use super::bitset::BitSet;
pub(crate) use super::types::LocalFlags;
use super::types::{
    CaptureSpan, CapturedLocalSpan, ClassId, IdxVec, LocId, LocalIdx, LocalInfo, LocalStorage,
    NameId, Pool, SiteSpan, TemplateId, Ty, TypeTable,
};
use crate::hir_compat::{self, Locations};
use perry_hir::{
    ir::{Expr, Function, Module as HirModule, Param, Stmt},
    types::{FuncId, LocalId},
    walker::{stmt_any_expr, walk_expr_children},
};
use pliron::context::Context;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TemplateKind {
    ModuleInit,
    Function,
    FieldInitializers,
    ClassConstructor,
    ClassMethod,
    ClassStaticMethod(ClassId),
    ClassGetter(usize),
    ClassSetter(usize),
    Closure,
    PromiseResolve(usize),
    PromiseReject(usize),
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
    pub(crate) field_init: Option<TemplateId>,
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

#[derive(Clone, Copy)]
pub(crate) struct IndexedSite {
    address: usize,
    pub(crate) handles: Option<(TemplateId, TemplateId)>,
}

#[derive(Clone, Copy, Default)]
struct TemplateLayout {
    sites: SiteSpan,
    captures: CaptureSpan,
    captured_locals: CapturedLocalSpan,
    closure_owner: Option<TemplateId>,
    frame_slots: u32,
}

#[derive(Default)]
pub(crate) struct ModuleIndex<'h> {
    pub(crate) views: IdxVec<TemplateId, TemplateView<'h>>,
    func_to_template: Vec<Option<TemplateId>>,
    layouts: IdxVec<TemplateId, TemplateLayout>,
    site_pool: Pool<SiteSpan, IndexedSite>,
    capture_pool: Pool<CaptureSpan, LocalId>,
    captured_local_pool: Pool<CapturedLocalSpan, LocalIdx>,
    pub(crate) classes: IdxVec<ClassId, ClassInfo>,
    pub(crate) class_has_implicit_params: BitSet,
    pub(crate) hir_class_is_literal: BitSet,
    literal_classes: Vec<(NameId, &'h [perry_hir::ir::ClassField])>,
    pub(crate) locals: IdxVec<LocalIdx, LocalInfo>,
    local_regex_patterns: Vec<(LocalIdx, &'h str)>,
    local_to_idx: Vec<Option<LocalIdx>>,
    pub(crate) capture_free_templates: BitSet,
    pub(crate) returns_empty_list: BitSet,
    pub(crate) closure_in_loop: BitSet,
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
            captures_this: false,
            captures_new_target: false,
            uses_this: false,
            explicit_return: false,
        });
        debug_assert_eq!(init_tid, TemplateId::INIT);
        let mut raw_sites: Vec<Vec<&'h Expr>> = vec![Vec::new()];
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
                field_init: None,
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
            let mut initializers = class
                .fields
                .iter()
                .filter_map(|field| field.init.as_ref())
                .peekable();
            if initializers.peek().is_some() {
                let owner =
                    idx.add_synthetic_template(TemplateKind::FieldInitializers, &class.name);
                idx.classes[cid].field_init = Some(owner);
                scan.raw_sites.resize_with(idx.views.len(), Vec::new);
                for init in initializers {
                    Self::record_site_expr(&mut scan, owner, init);
                    idx.scan_expr(&mut scan, init, owner, false);
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
        for (template, list) in idx.views.ids().zip(raw_sites) {
            let mut sites: Vec<_> = list
                .iter()
                .map(|&expr| IndexedSite {
                    address: std::ptr::from_ref(expr) as usize,
                    handles: None,
                })
                .collect();
            sites.sort_unstable_by_key(|site| site.address);
            sites.dedup_by_key(|site| site.address);
            for expr in list {
                if matches!(expr, Expr::New { class_name, .. } if class_name == "Promise")
                    && let Ok(site) = sites
                        .binary_search_by_key(&(std::ptr::from_ref(expr) as usize), |s| s.address)
                    && let Some(entry) = sites.get_mut(site)
                    && entry.handles.is_none()
                {
                    entry.handles = Some((
                        idx.add_synthetic_template(TemplateKind::PromiseResolve(site), "resolve"),
                        idx.add_synthetic_template(TemplateKind::PromiseReject(site), "reject"),
                    ));
                }
            }
            idx.layouts[template].sites = idx.site_pool.extend_from_slice(&sites);
        }

        for (cap_id, written, forward) in pending_captures {
            if let Some(lidx) = idx.local_idx(cap_id) {
                let flags = &mut idx.locals[lidx].flags;
                flags.insert(LocalFlags::CAPTURED);
                if written {
                    flags.insert(LocalFlags::WRITTEN_FROM_CLOSURE);
                }
                if forward && !flags.intersects(LocalFlags::MODULE_OR_PARAM) {
                    flags.insert(LocalFlags::FORWARD_CAPTURED);
                }
            }
        }
        let mut captured_locals = idx.views.filled(Vec::new());
        for lidx in idx.locals.ids() {
            let local = &mut idx.locals[lidx];
            if !local.flags.intersects(LocalFlags::MODULE_OR_PARAM) {
                if !local.flags.contains(LocalFlags::CAPTURED) {
                    let count = &mut idx.layouts[local.owner].frame_slots;
                    local.storage = LocalStorage::Frame(*count);
                    *count += 1;
                } else {
                    captured_locals[local.owner].push(lidx);
                }
            }
        }
        for (tid, list) in captured_locals.iter_enumerated() {
            if !list.is_empty() {
                idx.layouts[tid].captured_locals = idx.captured_local_pool.extend_from_slice(list);
            }
        }
        let mut template_captures = idx.views.map(|v| {
            v.captures
                .iter()
                .copied()
                .filter(|&c| idx.is_non_global_local(c))
                .collect::<Vec<_>>()
        });
        for (tid, view) in idx.views.iter_enumerated() {
            for &cap_id in view.captures {
                let Some(lidx) = idx.local_idx(cap_id) else {
                    continue;
                };
                let local = idx.locals[lidx];
                if local.flags.contains(LocalFlags::MODULE) {
                    continue;
                }
                let cap_owner = local.owner;
                let mut cur = idx.layouts[tid].closure_owner;
                while let Some(parent) = cur {
                    if parent == cap_owner || parent == TemplateId::INIT {
                        break;
                    }
                    template_captures[parent].push(cap_id);
                    cur = idx.layouts[parent].closure_owner;
                }
            }
        }
        for (tid, view) in idx.views.iter_enumerated() {
            let caps = &mut template_captures[tid];
            caps.sort_unstable();
            caps.dedup();
            let is_promise_handle = matches!(
                view.kind,
                TemplateKind::PromiseResolve(_) | TemplateKind::PromiseReject(_)
            );
            if caps.is_empty() && !is_promise_handle {
                idx.capture_free_templates.insert(tid.index());
            } else if !caps.is_empty() {
                idx.layouts[tid].captures = idx.capture_pool.extend_from_slice(caps);
            }
        }
        idx
    }

    pub(crate) fn local_regex_pattern(&self, local: LocalIdx) -> Option<&'h str> {
        self.local_regex_patterns
            .iter()
            .find(|&&(k, _)| k == local)
            .map(|&(_, pat)| pat)
    }
    pub(crate) fn template_frame_count(&self, t: TemplateId) -> usize {
        self.layouts[t].frame_slots as usize
    }
    pub(crate) fn closure_owner(&self, t: TemplateId) -> Option<TemplateId> {
        self.layouts[t].closure_owner
    }
    pub(crate) fn sites(&self, template: TemplateId) -> &[IndexedSite] {
        &self.site_pool[self.layouts[template].sites]
    }
    pub(crate) fn site_of(&self, template: TemplateId, expr: &Expr) -> Option<usize> {
        self.sites(template)
            .binary_search_by_key(&(std::ptr::from_ref(expr) as usize), |s| s.address)
            .ok()
    }
    pub(crate) fn view(&self, id: TemplateId) -> &TemplateView<'h> {
        &self.views[id]
    }
    pub(crate) fn class(&self, id: ClassId) -> &ClassInfo {
        &self.classes[id]
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
            .is_some_and(|li| self.locals[li].flags.contains(LocalFlags::PARAM_ANNOTATED))
    }
    pub(crate) fn is_inferred_local(&self, hir: &HirModule, id: LocalId, lidx: LocalIdx) -> bool {
        self.locals[lidx].flags.contains(LocalFlags::INFERRED)
            || !hir.local_source_spans.contains_key(&id)
    }
    pub(crate) fn is_for_head_local(&self, id: LocalId) -> bool {
        self.local_idx(id)
            .is_some_and(|li| self.locals[li].flags.contains(LocalFlags::FOR_HEAD))
    }
    pub(crate) fn is_non_global_local(&self, id: LocalId) -> bool {
        self.local_idx(id)
            .is_some_and(|li| !self.locals[li].flags.contains(LocalFlags::MODULE))
    }
    pub(crate) fn template_captures(&self, t: TemplateId) -> &[LocalId] {
        &self.capture_pool[self.layouts[t].captures]
    }
    pub(crate) fn template_captured_locals(&self, t: TemplateId) -> &[LocalIdx] {
        &self.captured_local_pool[self.layouts[t].captured_locals]
    }
    pub(crate) fn template_of_func(&self, id: FuncId) -> Option<TemplateId> {
        self.func_to_template.get(id as usize).copied().flatten()
    }
    pub(crate) fn local_idx(&self, id: LocalId) -> Option<LocalIdx> {
        self.local_to_idx.get(id as usize).copied().flatten()
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
        self.layouts.push(TemplateLayout::default());
        self.views.push(view)
    }

    fn add_synthetic_template(&mut self, kind: TemplateKind, name: &'h str) -> TemplateId {
        self.add_template(TemplateView {
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
            captures_this: false,
            captures_new_target: false,
            uses_this: false,
            explicit_return: true,
        })
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
        let mut flags = LocalFlags::default();
        if let Some(span) = hir.local_source_spans.get(&id) {
            if facts.is_inferred(span.start, span.end) {
                flags.insert(LocalFlags::INFERRED);
            }
            if facts.is_deferred(span.start) {
                flags.insert(LocalFlags::DEFERRED);
            }
            if facts.is_for_head_let(span.start) {
                flags.insert(LocalFlags::FOR_HEAD);
            }
            if facts.is_param_annotated(span.start) {
                flags.insert(LocalFlags::PARAM_ANNOTATED);
            }
        }
        let idx = self.locals.push(LocalInfo {
            id,
            owner,
            declared,
            storage: LocalStorage::None,
            flags,
        });
        if lid >= self.local_to_idx.len() {
            self.local_to_idx.resize(lid + 1, None);
        }
        if let Some(slot) = self.local_to_idx.get_mut(lid) {
            *slot = Some(idx);
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
            self.locals[local].flags.insert(LocalFlags::PARAM);
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
                    if is_module_top && !self.locals[local].flags.contains(LocalFlags::FOR_HEAD) {
                        let loc = self.add_location(Ty::BOTTOM);
                        let entry = &mut self.locals[local];
                        entry.flags.insert(LocalFlags::MODULE);
                        entry.storage = LocalStorage::Module(loc);
                    }
                    if let Some(init) = init {
                        if matches!(init, Expr::Null | Expr::Undefined) {
                            self.locals[local].flags.insert(LocalFlags::NULLISH_INIT);
                        }
                        if let Expr::RegExp { pattern, .. } = init {
                            self.local_regex_patterns.push((local, pattern.as_str()));
                        }
                        Self::record_site_expr(scan, owner, init);
                        self.scan_expr(scan, init, owner, in_loop);
                    }
                    scan.defined_locals.insert(*id as usize);
                }
                Stmt::Return(Some(val)) => {
                    if matches!(val, Expr::Array(items, _) if items.is_empty()) {
                        self.returns_empty_list.insert(owner.index());
                    }
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
            list.push(expr);
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
                Self::record_site_expr(scan, owner, expr);
                Self::record_site_expr(scan, owner, left);
                Self::record_site_expr(scan, owner, right);
            }
            Expr::Conditional {
                then_expr,
                else_expr,
                ..
            } => {
                Self::record_site_expr(scan, owner, expr);
                Self::record_site_expr(scan, owner, then_expr);
                Self::record_site_expr(scan, owner, else_expr);
            }
            Expr::New { args, .. } => {
                Self::record_site_expr(scan, owner, expr);
                for arg in args {
                    Self::record_site_expr(scan, owner, arg);
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
                    captures_this: *captures_this,
                    captures_new_target: *captures_new_target,
                    uses_this: stmts_use_this(body),
                    explicit_return: exp_ret,
                });
                scan.raw_sites.resize_with(self.views.len(), Vec::new);
                self.layouts[tid].closure_owner = Some(owner);
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
            let user_is_closure =
                is_write && self.view(user_template).kind == TemplateKind::Closure;
            let local = &mut self.locals[lidx];
            if is_write {
                local.flags.insert(LocalFlags::REASSIGNED);
            }
            if local.owner != user_template {
                local.flags.insert(LocalFlags::CAPTURED);
                if user_is_closure {
                    local.flags.insert(LocalFlags::WRITTEN_FROM_CLOSURE);
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
    raw_sites: &'a mut Vec<Vec<&'h Expr>>,
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
