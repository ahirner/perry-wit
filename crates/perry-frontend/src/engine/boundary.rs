//! Single-pass boundary indexing over Perry HIR.
use super::bitset::BitSet;
use super::solve::{MemberTarget, SiteResolution};
pub(crate) use super::types::LocalFlags;
use super::types::{
    BindingSpan, CaptureSpan, CapturedLocalSpan, ClassId, Idx, IdxVec, LocId, LocalIdx, LocalInfo,
    LocalStorage, NameId, Pool, SiteSpan, TemplateId, Ty, TypeTable,
};
use crate::hir_compat;
use crate::source::{ConstructorSites, SourceModule};
use perry_hir::{
    ir::{Expr, Function, Param, Stmt},
    types::{FuncId, LocalId},
    walker::{stmt_any_expr, walk_expr_children},
};
use pliron::context::Context;
use std::collections::HashMap;

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

    pub(crate) captures_this: bool,
    pub(crate) captures_new_target: bool,
    pub(crate) uses_this: bool,
    pub(crate) explicit_return: bool,
}

impl<'h> TemplateView<'h> {
    pub(crate) fn display_name(&self, locations: &hir_compat::Locations) -> String {
        if self.name.is_empty() {
            let raw = self.raw_name();
            hir_compat::function_label(locations.display_function_name(&raw)).to_owned()
        } else {
            locations.display_function_name(self.name).to_owned()
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
        let body = crate::source::authored_body(&f.params, &f.body);
        Self {
            kind,
            name: f.name.as_str(),
            func_id: f.id,
            params: &f.params,
            return_type: &f.return_type,
            body,
            captures: &f.captures,
            type_params: &f.type_params,
            is_async: f.is_async,
            is_generator: f.is_generator,

            captures_this: false,
            captures_new_target: false,
            uses_this: stmts_use_this(body),
            explicit_return,
        }
    }
}

pub(crate) struct ClassInfo<'h> {
    pub(crate) declaration: &'h perry_hir::ir::Class,
    pub(crate) ty: Ty,
    pub(crate) name_id: NameId,
    pub(crate) field_init: Option<TemplateId>,
    pub(crate) ctor: Option<TemplateId>,
    pub(crate) static_field_locs: Vec<(NameId, LocId)>,
}

impl ClassInfo<'_> {
    pub(super) fn static_field(&self, name: NameId) -> Option<LocId> {
        self.static_field_locs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|&(_, loc)| loc)
    }
}

#[derive(Clone, Copy)]
pub(super) struct SourceLocal<'h> {
    pub(super) local: LocalIdx,
    pub(super) name: &'h str,
    pub(super) ty: &'h perry_hir::types::Type,
}
#[derive(Clone, Copy)]
pub(crate) struct IndexedSite<'h> {
    pub(super) expr: &'h Expr,
    pub(super) index: super::plans::ExprIdx,
    pub(crate) handles: Option<(TemplateId, TemplateId)>,
}

#[derive(Clone, Copy, Default)]
struct TemplateLayout {
    sites: SiteSpan,
    bindings: BindingSpan,
    captures: CaptureSpan,
    captured_locals: CapturedLocalSpan,
    closure_owner: Option<TemplateId>,
    frame_slots: u32,
}

#[derive(Clone, Copy)]
enum Declaration<'h> {
    Function,
    Closure(&'h Expr),
}

#[derive(Default)]
pub(crate) struct LexicalFunctions<'h>(Vec<Option<(TemplateId, Declaration<'h>)>>);

impl<'h> LexicalFunctions<'h> {
    pub(crate) fn closure(&self, id: FuncId) -> Option<&'h Expr> {
        match self.0.get(id as usize)?.as_ref()?.1 {
            Declaration::Closure(expr) => Some(expr),
            Declaration::Function => None,
        }
    }
}

#[derive(Default)]
pub(crate) struct Annotations(HashMap<*const perry_hir::types::Type, Ty>);

impl Annotations {
    fn register(
        &mut self,
        scan: &mut ScanCtx<'_, '_>,
        ty: &perry_hir::types::Type,
        explicit: bool,
    ) -> Ty {
        let declared = *self
            .0
            .entry(std::ptr::from_ref(ty))
            .or_insert_with(|| scan.types.intern_hir(scan.ctx, ty, true));
        if explicit {
            declared
        } else {
            scan.types.intern_hir(scan.ctx, ty, false)
        }
    }

    pub(crate) fn get(&self, ty: &perry_hir::types::Type) -> perry_wit_diagnostics::Result<Ty> {
        self.0.get(&std::ptr::from_ref(ty)).copied().ok_or_else(|| {
            perry_wit_diagnostics::Diagnostic::internal("missing source type annotation")
        })
    }
}

#[derive(Default)]
pub(crate) struct ModuleIndex<'h> {
    pub(crate) iteration: hir_compat::Iteration<'h>,
    pub(super) annotations: Annotations,
    pub(super) expressions: super::plans::Expressions,
    pub(crate) views: IdxVec<TemplateId, TemplateView<'h>>,
    pub(super) functions: LexicalFunctions<'h>,
    layouts: IdxVec<TemplateId, TemplateLayout>,
    site_pool: Pool<SiteSpan, IndexedSite<'h>>,
    bindings: Pool<BindingSpan, SourceLocal<'h>>,
    capture_pool: Pool<CaptureSpan, LocalId>,
    captured_local_pool: Pool<CapturedLocalSpan, LocalIdx>,
    pub(crate) classes: IdxVec<ClassId, ClassInfo<'h>>,
    pub(crate) class_has_implicit_params: BitSet,
    pub(super) literal_classes: crate::source::LiteralClasses<'h>,
    pub(crate) locals: IdxVec<LocalIdx, LocalInfo>,
    pub(crate) receiver_members: crate::wit::ReceiverMembers<'h>,
    pub(crate) regexes: crate::source::RegexSites<'h>,
    pub(super) local_to_idx: Vec<Option<LocalIdx>>,
    pub(crate) capture_free_templates: BitSet,
    pub(crate) returns_empty_list: BitSet,
    pub(crate) closure_in_loop: BitSet,
    pub(crate) location_types: IdxVec<LocId, Ty>,
    pub(crate) max_func_id: FuncId,
    pub(crate) max_local_id: LocalId,
}

static VOID_HIR: perry_hir::types::Type = perry_hir::types::Type::Void;

mod build;

impl<'h> ModuleIndex<'h> {
    pub(crate) fn template_frame_count(&self, t: TemplateId) -> usize {
        self.layouts[t].frame_slots as usize
    }
    pub(crate) fn closure_owner(&self, t: TemplateId) -> Option<TemplateId> {
        self.layouts[t].closure_owner
    }
    pub(crate) fn sites(&self, template: TemplateId) -> &[IndexedSite<'h>] {
        &self.site_pool[self.layouts[template].sites]
    }
    pub(super) fn bindings(&self, template: TemplateId) -> &[SourceLocal<'h>] {
        &self.bindings[self.layouts[template].bindings]
    }
    pub(crate) fn site_of(&self, template: TemplateId, expr: &Expr) -> Option<usize> {
        self.sites(template)
            .binary_search_by_key(&self.expressions.index(expr)?, |s| s.index)
            .ok()
    }
    pub(crate) fn view(&self, id: TemplateId) -> &TemplateView<'h> {
        &self.views[id]
    }
    pub(crate) fn class(&self, id: ClassId) -> &ClassInfo<'h> {
        &self.classes[id]
    }
    pub(crate) fn class_by_name_id(&self, name: NameId) -> Option<ClassId> {
        self.classes
            .iter_enumerated()
            .find_map(|(id, c)| (c.name_id == name).then_some(id))
    }
    pub(crate) fn is_empty_object_expr(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Object(fields) => fields.is_empty(),
            Expr::New {
                class_name, args, ..
            } if args.is_empty() => self
                .literal_classes
                .get(class_name.as_str())
                .is_some_and(|fields| fields.is_empty()),
            _ => false,
        }
    }
    pub(crate) fn is_param_annotated(&self, id: LocalId) -> bool {
        self.local_idx(id)
            .is_some_and(|li| self.locals[li].flags.contains(LocalFlags::PARAM_ANNOTATED))
    }
    pub(crate) fn is_inferred_local(
        &self,
        hir: &SourceModule,
        id: LocalId,
        lidx: LocalIdx,
    ) -> bool {
        self.locals[lidx].flags.contains(LocalFlags::INFERRED)
            || !hir.declarations.locals.contains_key(&id)
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
        Some(self.functions.0.get(id as usize)?.as_ref()?.0)
    }
    pub(super) fn method_template(&self, methods: &[Function], name: &str) -> Option<TemplateId> {
        let method = methods
            .iter()
            .find(|m| hir_compat::method(&m.name).name() == name)?;
        self.template_of_func(method.id)
    }
    pub(super) fn accessor_template(
        &self,
        accessors: &[(String, Function)],
        name: &str,
    ) -> Option<TemplateId> {
        let (_, accessor) = accessors.iter().find(|(property, _)| property == name)?;
        self.template_of_func(accessor.id)
    }
    pub(crate) fn local_idx(&self, id: LocalId) -> Option<LocalIdx> {
        self.local_to_idx.get(id as usize).copied().flatten()
    }

    fn set_func_template(
        &mut self,
        func_id: FuncId,
        tid: TemplateId,
        declaration: Declaration<'h>,
    ) {
        let idx = func_id as usize;
        if idx >= self.functions.0.len() {
            self.functions.0.resize(idx + 1, None);
        }
        if let Some(slot) = self.functions.0.get_mut(idx) {
            *slot = Some((tid, declaration));
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
        self.annotations.register(scan, &func.return_type, true);
        self.max_func_id = self.max_func_id.max(func.id);
        let tid = self.add_template(TemplateView::from_function(kind, func, explicit_return));
        scan.raw_sites.resize_with(self.views.len(), Vec::new);
        self.set_func_template(func.id, tid, Declaration::Function);
        self.scan_params(scan, &func.params, tid);
        self.scan_stmts(scan, self.view(tid).body, tid, false, false);
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
        declarations: &crate::source::ModuleDeclarations,
    ) -> LocalIdx {
        self.max_local_id = self.max_local_id.max(id);
        let lid = id as usize;
        if let Some(existing) = self.local_to_idx.get(lid).copied().flatten() {
            return existing;
        }
        let flags = declarations
            .locals
            .get(&id)
            .map(|binding| binding.flags)
            .unwrap_or_default();
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

    fn param_is_unannotated(
        &self,
        declarations: &crate::source::ModuleDeclarations,
        param: &Param,
    ) -> bool {
        !declarations
            .locals
            .get(&param.id)
            .is_some_and(|s| s.flags.contains(LocalFlags::PARAM_ANNOTATED))
            && matches!(
                param.ty,
                perry_hir::types::Type::Any | perry_hir::types::Type::Unknown
            )
    }

    fn scan_params(&mut self, scan: &mut ScanCtx<'h, '_>, params: &'h [Param], owner: TemplateId) {
        for param in params {
            let explicit = scan
                .declarations
                .locals
                .get(&param.id)
                .is_some_and(|s| s.flags.contains(LocalFlags::PARAM_ANNOTATED));
            let ty = self.annotations.register(scan, &param.ty, explicit);
            let local = self.ensure_local(param.id, owner, ty, scan.declarations);
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
                Stmt::Let {
                    id,
                    ty,
                    init,
                    mutable,
                    name,
                    ..
                } => {
                    self.iteration.bind(*id, name, init.as_ref());
                    let explicit = scan
                        .declarations
                        .locals
                        .get(id)
                        .is_some_and(|s| !s.flags.contains(LocalFlags::INFERRED));
                    let declared = self.annotations.register(scan, ty, explicit);
                    let local = self.ensure_local(*id, owner, declared, scan.declarations);
                    if scan.declarations.for_of(Some(stmt)).is_none() {
                        scan.bindings.push((owner, SourceLocal { local, name, ty }));
                    }
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
                        let regex = matches!(init, Expr::RegExp { .. })
                            || scan.constructors.regexp(init).is_some();
                        scan.regexes.bind(*id, init, regex, !mutable);
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
                            self.ensure_local(*id, owner, err_ty, scan.declarations);
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
        let site = self.expressions.register(expr, scan.constructors);
        match expr {
            Expr::JsonParseTyped { ty, .. }
            | Expr::Closure {
                return_type: ty, ..
            } => {
                self.annotations.register(scan, ty, true);
            }
            _ if let Some(constructor) = self.expressions.collection(expr)
                && !constructor.types.is_empty() =>
            {
                let args: Vec<_> = constructor
                    .types
                    .iter()
                    .map(|ty| scan.types.intern_hir(scan.ctx, ty, true))
                    .collect();
                let base = crate::shapes::TypeName::new(scan.ctx, constructor.kind.name().into());
                self.expressions.declared.insert(
                    site,
                    SiteResolution::CollectionNew(scan.types.generic(base, &args)),
                );
            }
            _ => {}
        }
        if let Some(id) = self.receiver_members.bind(expr, scan.decls, scan.locations) {
            self.expressions
                .declared
                .insert(site, SiteResolution::MemberCall(MemberTarget::Resource(id)));
        }
        if matches!(expr, Expr::RegExp { .. }) || self.expressions.regexp(expr).is_some() {
            scan.regexes.sites.push(expr);
        }
        match expr {
            Expr::Call { args, .. }
            | Expr::StaticMethodCall { args, .. }
            | Expr::NativeMethodCall { args, .. }
            | Expr::New { args, .. } => {
                Self::record_site_expr(scan, owner, expr);
                for arg in args {
                    Self::record_site_expr(scan, owner, arg);
                }
            }
            Expr::ArraySpread(_) | Expr::CallSpread { .. } => {
                Self::record_site_expr(scan, owner, expr);
                walk_expr_children(expr, &mut |value| {
                    Self::record_site_expr(scan, owner, value)
                });
            }
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
            Expr::Object(fields) => {
                Self::record_site_expr(scan, owner, expr);
                for (_, value) in fields {
                    Self::record_site_expr(scan, owner, value);
                }
            }
            Expr::ObjectSpread { .. }
            | Expr::ArrayFrom(..)
            | Expr::NewDynamic { .. }
            | Expr::MapNew
            | Expr::MapNewFromArray(_)
            | Expr::SetNew
            | Expr::SetNewFromArray(_)
            | Expr::Compare { .. }
            | Expr::TypeOf(_)
            | Expr::FuncRef(_) => Self::record_site_expr(scan, owner, expr),
            expr if let Some((_, _, callback, _)) = crate::methods::Call::array_callback(expr) => {
                Self::record_site_expr(scan, owner, callback)
            }
            Expr::ArrayFromMapped {
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
                self.ensure_local(*id, owner, Ty::OPEN, scan.declarations);
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
                let body = crate::source::authored_body(params, body);
                let tid = self.add_template(TemplateView {
                    kind: TemplateKind::Closure,
                    name: scan.closure_names.get(func_id).map_or("", String::as_str),
                    func_id: *func_id,
                    params: params.as_slice(),
                    return_type,
                    body,
                    captures: captures.as_slice(),
                    type_params: &[],
                    is_async: *is_async,
                    is_generator: *is_generator,

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
                self.set_func_template(*func_id, tid, Declaration::Closure(expr));
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
    constructors: &'a crate::source::Constructors,
    decls: &'a crate::shapes::Declarations<'h>,
    locations: &'a hir_compat::Locations<'a>,
    regexes: crate::source::RegexInputs<'h>,
    ctx: &'a Context,
    declarations: &'a crate::source::ModuleDeclarations,
    closure_names: &'h HashMap<FuncId, String>,
    types: &'a mut TypeTable,
    raw_sites: &'a mut Vec<Vec<&'h Expr>>,
    bindings: Vec<(TemplateId, SourceLocal<'h>)>,
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
