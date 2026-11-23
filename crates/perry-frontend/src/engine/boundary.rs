//! Single-pass boundary indexing over Perry HIR.
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
    pub(crate) explicit_return: bool,
}

impl<'h> TemplateView<'h> {
    pub(crate) fn is_obj_method(&self) -> bool {
        self.name.starts_with("__obj_method_")
    }
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
    pub(crate) views: Vec<TemplateView<'h>>,
    func_to_template: Vec<Option<TemplateId>>,
    template_site_spans: Vec<TySpan>,
    site_entries: Vec<(usize, u32)>,
    pub(crate) classes: Vec<ClassInfo>,
    pub(crate) class_has_implicit_params: BitSet,
    pub(crate) hir_class_is_literal: BitSet,
    literal_classes: Vec<(NameId, &'h [perry_hir::ir::ClassField])>,
    pub(crate) local_ids: Vec<LocalId>,
    pub(crate) local_owner: Vec<TemplateId>,
    pub(crate) local_declared: Vec<Ty>,
    pub(crate) local_loc: Vec<Option<LocId>>,
    pub(crate) local_value_use: Vec<Option<Span>>,
    pub(crate) local_regex_pattern: Vec<Option<&'h str>>,
    local_to_idx: Vec<Option<LocalIdx>>,
    pub(crate) local_inferred: BitSet,
    pub(crate) local_deferred: BitSet,
    pub(crate) local_param: BitSet,
    pub(crate) local_param_annotated: BitSet,
    pub(crate) local_module: BitSet,
    pub(crate) local_captured: BitSet,
    pub(crate) local_written_from_closure: BitSet,
    local_frame_slot: Vec<Option<u32>>,
    template_frame_counts: Vec<usize>,
    pub(crate) location_types: Vec<Ty>,
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
            views: Vec::with_capacity(1 + hir.functions.len()),
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
            explicit_return: false,
        });
        let mut raw_sites: Vec<Vec<(usize, u32)>> = vec![Vec::new()];
        let mut scan = ScanCtx {
            ctx,
            hir,
            facts,
            types,
            raw_sites: &mut raw_sites,
        };
        idx.scan_stmts(&mut scan, &hir.init, init_tid, true);

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
            let cid = ClassId(idx.classes.len() as u32);
            let mut info = ClassInfo {
                hir_idx,
                name_id: scan.types.intern_name(&class.name),
                ctor: None,
                methods: Vec::new(),
                static_methods: Vec::new(),
                getters: Vec::new(),
                setters: Vec::new(),
                static_field_locs: Vec::new(),
            };
            for field in &class.static_fields {
                let ty = scan.types.intern_hir(scan.ctx, &field.ty, true);
                let loc = idx.add_location(ty);
                info.static_field_locs
                    .push((scan.types.intern_name(&field.name), loc));
                if let Some(init) = &field.init {
                    idx.scan_expr(&mut scan, init, init_tid);
                }
            }
            for field in &class.fields {
                if let Some(init) = &field.init {
                    idx.scan_expr(&mut scan, init, init_tid);
                }
            }
            if class
                .constructor
                .iter()
                .chain(&class.methods)
                .flat_map(|f| &f.params)
                .any(|p| idx.param_is_unannotated(scan.hir, scan.facts, p))
            {
                idx.class_has_implicit_params.insert(cid.0 as usize);
            }
            if let Some(ctor) = &class.constructor {
                info.ctor = Some(idx.register_member(
                    &mut scan,
                    TemplateKind::ClassConstructor,
                    ctor,
                    false,
                ));
            }
            for m in &class.methods {
                let tid = idx.register_member(
                    &mut scan,
                    TemplateKind::ClassMethod,
                    m,
                    locations.has_explicit_return(&format!("{}::{}", class.name, m.name)),
                );
                info.methods.push((
                    scan.types.intern_name(hir_compat::method(&m.name).name()),
                    tid,
                ));
            }
            for m in &class.static_methods {
                let tid = idx.register_member(
                    &mut scan,
                    TemplateKind::ClassStaticMethod(cid),
                    m,
                    locations.has_explicit_return(&format!("{}::{}", class.name, m.name)),
                );
                info.static_methods.push((
                    scan.types.intern_name(hir_compat::method(&m.name).name()),
                    tid,
                ));
            }
            for (i, (name, g)) in class.getters.iter().enumerate() {
                let tid = idx.register_member(
                    &mut scan,
                    TemplateKind::ClassGetter(i),
                    g,
                    locations.has_explicit_return(&format!("{}::get_{name}", class.name)),
                );
                info.getters.push((scan.types.intern_name(name), tid));
            }
            for (i, (name, s)) in class.setters.iter().enumerate() {
                let tid = idx.register_member(
                    &mut scan,
                    TemplateKind::ClassSetter(i),
                    s,
                    locations.has_explicit_return(&format!("{}::set_{name}", class.name)),
                );
                info.setters.push((scan.types.intern_name(name), tid));
            }
            idx.classes.push(info);
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

        idx.local_frame_slot = vec![None; idx.local_ids.len()];
        idx.template_frame_counts = vec![0; idx.views.len()];
        for (lidx, (&owner, slot_out)) in idx
            .local_owner
            .iter()
            .zip(idx.local_frame_slot.iter_mut())
            .enumerate()
        {
            if !idx.local_module.contains(lidx)
                && !idx.local_captured.contains(lidx)
                && !idx.local_param.contains(lidx)
                && let Some(count) = idx.template_frame_counts.get_mut(owner.0 as usize)
            {
                *slot_out = Some(*count as u32);
                *count += 1;
            }
        }
        idx
    }

    pub(crate) fn local_frame_slot(&self, local: LocalIdx) -> Option<usize> {
        self.local_frame_slot
            .get(local.0 as usize)?
            .map(|s| s as usize)
    }
    pub(crate) fn template_frame_count(&self, t: TemplateId) -> usize {
        self.template_frame_counts
            .get(t.0 as usize)
            .copied()
            .unwrap_or(0)
    }
    pub(crate) fn site_count(&self, t: TemplateId) -> usize {
        self.template_site_spans
            .get(t.0 as usize)
            .map_or(0, |s| s.len as usize)
    }
    pub(crate) fn site_of(&self, t: TemplateId, expr: &Expr) -> Option<usize> {
        let entries = self
            .site_entries
            .get(self.template_site_spans.get(t.0 as usize)?.range())?;
        let pos = entries
            .binary_search_by_key(&(std::ptr::from_ref(expr) as usize), |&(a, _)| a)
            .ok()?;
        entries.get(pos).map(|&(_, s)| s as usize)
    }
    pub(crate) fn view(&self, id: TemplateId) -> Option<&TemplateView<'h>> {
        self.views.get(id.0 as usize)
    }
    pub(crate) fn class(&self, id: ClassId) -> Option<&ClassInfo> {
        self.classes.get(id.0 as usize)
    }
    pub(crate) fn class_by_name_id(&self, name: NameId) -> Option<ClassId> {
        self.classes
            .iter()
            .position(|c| c.name_id == name)
            .map(|i| ClassId(i as u32))
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
            .is_some_and(|li| self.local_param_annotated.contains(li.0 as usize))
    }
    pub(crate) fn is_inferred_local(&self, hir: &HirModule, id: LocalId, lidx: LocalIdx) -> bool {
        self.local_inferred.contains(lidx.0 as usize) || !hir.local_source_spans.contains_key(&id)
    }
    pub(crate) fn is_non_global_local(&self, id: LocalId) -> bool {
        self.local_idx(id)
            .is_some_and(|li| !self.local_module.contains(li.0 as usize))
    }
    pub(crate) fn captures_local(&self, captures: &[LocalId]) -> bool {
        captures.iter().any(|&c| self.is_non_global_local(c))
    }
    pub(crate) fn template_of_func(&self, id: FuncId) -> Option<TemplateId> {
        self.func_to_template.get(id as usize).copied().flatten()
    }
    pub(crate) fn local_idx(&self, id: LocalId) -> Option<LocalIdx> {
        self.local_to_idx.get(id as usize).copied().flatten()
    }
    pub(crate) fn template_count(&self) -> usize {
        self.views.len()
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
        let id = TemplateId(self.views.len() as u32);
        self.views.push(view);
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
        let tid = self.add_template(TemplateView::from_function(kind, func, explicit_return));
        scan.raw_sites.resize_with(self.views.len(), Vec::new);
        self.set_func_template(func.id, tid);
        self.scan_params(scan, &func.params, tid);
        self.scan_stmts(scan, &func.body, tid, false);
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
        if let Some(existing) = self.local_to_idx.get(lid).copied().flatten() {
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
        if let Some(slot) = self.local_to_idx.get_mut(lid) {
            *slot = Some(idx);
        }
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
            self.local_param.insert(local.0 as usize);
            if let Some(default) = &param.default {
                self.scan_expr(scan, default, owner);
            }
        }
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
                        .is_some_and(|s| !scan.facts.is_inferred(s.start, s.end));
                    let declared = scan.types.intern_hir(scan.ctx, ty, explicit);
                    let local = self.ensure_local(*id, owner, declared, scan.hir, scan.facts);
                    if is_module_top {
                        self.local_module.insert(local.0 as usize);
                        let loc = self.add_location(if !explicit || scan.types.is_open(declared) {
                            Ty::BOTTOM
                        } else {
                            declared
                        });
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
                    if let Some(b) = else_branch {
                        self.scan_stmts(scan, b, owner, false);
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
                    if let Some(i) = init {
                        self.scan_stmts(scan, std::slice::from_ref(i), owner, false);
                    }
                    if let Some(c) = condition {
                        self.scan_expr(scan, c, owner);
                    }
                    if let Some(u) = update {
                        self.scan_expr(scan, u, owner);
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
                    if let Some(f) = finally {
                        self.scan_stmts(scan, f, owner, false);
                    }
                }
                Stmt::Switch {
                    discriminant,
                    cases,
                } => {
                    self.scan_expr(scan, discriminant, owner);
                    for case in cases {
                        if let Some(t) = &case.test {
                            self.scan_expr(scan, t, owner);
                        }
                        self.scan_stmts(scan, &case.body, owner, false);
                    }
                }
                Stmt::Labeled { body, .. } => {
                    self.scan_stmts(scan, std::slice::from_ref(body), owner, false)
                }
                other => {
                    stmt_any_expr(other, &mut |e| {
                        self.scan_expr(scan, e, owner);
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
            Expr::Array(items, _) if items.is_empty() => Self::record_site_expr(scan, owner, expr),
            Expr::CallSpread { .. }
            | Expr::New { .. }
            | Expr::NewDynamic { .. }
            | Expr::MapNew
            | Expr::MapNewFromArray(_)
            | Expr::SetNew
            | Expr::SetNewFromArray(_)
            | Expr::StaticMethodCall { .. }
            | Expr::Compare { .. }
            | Expr::FuncRef(_) => Self::record_site_expr(scan, owner, expr),
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
            } => Self::record_site_expr(scan, owner, callback),
            Expr::LocalGet(id) => self.record_local_use(*id, owner, false),
            Expr::LocalSet(id, _) | Expr::Update { id, .. } => {
                self.record_local_use(*id, owner, true)
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
                let exp_ret = !matches!(
                    return_type,
                    perry_hir::types::Type::Any | perry_hir::types::Type::Unknown
                );
                let tid = self.add_template(TemplateView {
                    kind: TemplateKind::Closure,
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
                    explicit_return: exp_ret,
                });
                scan.raw_sites.resize_with(self.views.len(), Vec::new);
                self.set_func_template(*func_id, tid);
                for &cap in captures {
                    if let Some(l) = self.local_idx(cap) {
                        self.local_captured.insert(l.0 as usize);
                    }
                }
                for &cap in mutable_captures {
                    if let Some(l) = self.local_idx(cap) {
                        self.local_captured.insert(l.0 as usize);
                        self.local_written_from_closure.insert(l.0 as usize);
                    }
                }
                self.scan_params(scan, params, tid);
                self.scan_stmts(scan, body, tid, false);
                return;
            }
            _ => {}
        }
        walk_expr_children(expr, &mut |child| self.scan_expr(scan, child, owner));
    }

    fn record_local_use(&mut self, id: LocalId, user_template: TemplateId, is_write: bool) {
        if let Some(lidx) = self.local_idx(id)
            && self.local_owner.get(lidx.0 as usize).copied() != Some(user_template)
        {
            self.local_captured.insert(lidx.0 as usize);
            if is_write
                && self
                    .view(user_template)
                    .is_some_and(|v| v.kind == TemplateKind::Closure)
            {
                self.local_written_from_closure.insert(lidx.0 as usize);
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
