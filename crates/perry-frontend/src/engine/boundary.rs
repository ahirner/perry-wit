//! Single-pass boundary indexing over Perry HIR.
use super::BitSet;
use super::plans::ExprIdx;
use super::solve::SiteResolution;
pub(crate) use super::types::LocalFlags;
use super::types::{
    BindingSpan, CaptureSpan, ClassId, Idx, IdxVec, LocId, LocalIdx, LocalInfo, LocalStorage, Pool,
    TemplateId, Ty, TypeTable,
};
use crate::hir_compat;
use crate::narrowing::Region;
use crate::source::{CallerKind, ConstructorSites};
use perry_hir::{
    ir::{Expr, Function, Param, Stmt},
    types::{FuncId, LocalId},
    walker::{stmt_any_expr, walk_expr_children},
};
use pliron::context::Context;
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub(super) type Origins = IdxVec<TemplateId, Vec<(super::BodyOwner, CallerKind, Option<FuncId>)>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TemplateKind {
    ModuleInit,
    Function,
    FieldInitializers,
    ClassConstructor,
    ClassMethod,
    ClassStaticMethod(ClassId),
    ClassGetter,
    ClassSetter,
    Closure(ExprIdx),
    PromiseResolve(ExprIdx),
    PromiseReject(ExprIdx),
}

impl TemplateKind {
    pub(super) fn is_class_member(self) -> bool {
        matches!(
            self,
            Self::ClassMethod | Self::ClassStaticMethod(_) | Self::ClassGetter | Self::ClassSetter
        )
    }
}

pub(crate) struct TemplateView<'h> {
    pub(crate) kind: TemplateKind,
    pub(crate) name: std::borrow::Cow<'h, str>,
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
            locations.display_function_name(&self.name).to_owned()
        }
    }

    pub(crate) fn raw_name(&self) -> String {
        if self.name.is_empty() {
            format!("callback_{}", self.func_id)
        } else {
            self.name.to_string()
        }
    }

    fn synthetic(kind: TemplateKind, name: &'h str) -> Self {
        Self {
            kind,
            name: name.into(),
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
            explicit_return: !matches!(kind, TemplateKind::ModuleInit),
        }
    }

    fn from_function(kind: TemplateKind, f: &'h Function, explicit_return: bool) -> Self {
        let body = crate::source::authored_body(&f.params, &f.body);
        Self {
            kind,
            name: f.name.as_str().into(),
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
            uses_this: false,
            explicit_return,
        }
    }
}

pub(crate) struct ClassInfo<'h> {
    pub(crate) declaration: &'h perry_hir::ir::Class,
    pub(crate) ty: Ty,
    pub(crate) field_init: Option<TemplateId>,
    pub(crate) ctor: Option<TemplateId>,
    pub(super) implicit_params: bool,
    pub(super) direct_constructor: bool,
    static_field_locs: Vec<(LocId, LocalId)>,
}

impl<'h> ClassInfo<'h> {
    pub(super) fn static_fields(
        &self,
    ) -> impl Iterator<Item = (&'h perry_hir::ir::ClassField, (LocId, LocalId))> {
        self.declaration
            .static_fields
            .iter()
            .zip(self.static_field_locs.iter().copied())
    }

    pub(super) fn static_field(&self, name: &str) -> Option<LocId> {
        self.static_fields()
            .find_map(|(field, (loc, _))| (field.name == name).then_some(loc))
    }
}

#[derive(Clone, Copy)]
pub(super) struct SourceLocal<'h> {
    pub(super) local: LocalIdx,
    pub(super) name: &'h str,
}
#[derive(Clone, Copy, Default)]
struct TemplateLayout {
    bindings: BindingSpan,
    captures: CaptureSpan,
    references: CaptureSpan,
    activation_slots: u32,
    closure_owner: Option<TemplateId>,
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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum GenericOwner {
    Function(TemplateId),
    Class(ClassId),
}

pub(super) struct SourceRegion {
    pub(super) template: TemplateId,
    pub(super) sites: std::ops::Range<super::plans::ExprIdx>,
    exits: u8,
}

#[derive(Default)]
pub(crate) struct ModuleIndex<'h> {
    pub(super) regions: HashMap<Region, SourceRegion>,
    pub(super) type_slots: HashMap<(GenericOwner, crate::shapes::TypeName), usize>,
    external_uses: std::collections::HashSet<(TemplateId, LocalId)>,
    pub(crate) iteration: hir_compat::Iteration<'h>,
    pub(super) annotations: Annotations,
    pub(super) expressions: super::plans::Expressions,
    pub(crate) views: IdxVec<TemplateId, TemplateView<'h>>,
    pub(super) functions: Vec<Option<TemplateId>>,
    layouts: IdxVec<TemplateId, TemplateLayout>,
    pub(super) events: BTreeMap<(TemplateId, ExprIdx), &'h Expr>,
    pub(super) promises: HashMap<TemplateId, Vec<(ExprIdx, TemplateId, TemplateId)>>,
    bindings: Pool<BindingSpan, SourceLocal<'h>>,
    capture_pool: Pool<CaptureSpan, LocalId>,
    pub(crate) classes: indexmap::IndexMap<ClassId, ClassInfo<'h>>,
    pub(super) literal_classes: crate::source::LiteralClasses<'h>,
    pub(crate) locals: IdxVec<LocalIdx, LocalInfo>,
    pub(crate) receiver_members: crate::wit::ReceiverMembers<'h>,
    pub(super) local_to_idx: Vec<Option<LocalIdx>>,
    pub(crate) capture_free_templates: BitSet,
    pub(crate) returns_empty_list: BitSet,
    pub(crate) closure_in_loop: BitSet,
    pub(crate) location_types: IdxVec<LocId, Ty>,
    pub(crate) max_func_id: FuncId,
}

static VOID_HIR: perry_hir::types::Type = perry_hir::types::Type::Void;

mod build;

impl<'h> ModuleIndex<'h> {
    pub(crate) fn closure_owner(&self, t: TemplateId) -> Option<TemplateId> {
        self.layouts[t].closure_owner
    }
    pub(super) fn bindings(&self, template: TemplateId) -> &[SourceLocal<'h>] {
        &self.bindings[self.layouts[template].bindings]
    }
    pub(crate) fn view(&self, id: TemplateId) -> &TemplateView<'h> {
        &self.views[id]
    }
    pub(crate) fn class(&self, id: ClassId) -> &ClassInfo<'h> {
        &self.classes[&id]
    }
    pub(crate) fn class_named(&self, name: &str) -> Option<ClassId> {
        self.classes
            .iter()
            .find_map(|(&id, c)| (c.declaration.name == name).then_some(id))
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
    pub(crate) fn is_non_global_local(&self, id: LocalId) -> bool {
        self.local_idx(id)
            .is_some_and(|li| !self.locals[li].flags.contains(LocalFlags::MODULE))
    }
    pub(crate) fn template_captures(&self, t: TemplateId) -> &[LocalId] {
        &self.capture_pool[self.layouts[t].captures]
    }
    pub(crate) fn closure_references(&self, template: TemplateId) -> &[LocalId] {
        &self.capture_pool[self.layouts[template].references]
    }

    pub(super) fn activation_slots(&self, t: TemplateId) -> usize {
        self.layouts[t].activation_slots as usize
    }
    pub(crate) fn template_of_func(&self, id: FuncId) -> Option<TemplateId> {
        self.functions.get(id as usize).copied().flatten()
    }
    pub(super) fn method_template(&self, methods: &[Function], name: &str) -> Option<TemplateId> {
        let method = methods
            .iter()
            .find(|m| hir_compat::method(&m.name).name() == name)?;
        self.template_of_func(method.id)
    }
    pub(crate) fn local_idx(&self, id: LocalId) -> Option<LocalIdx> {
        self.local_to_idx.get(id as usize).copied().flatten()
    }

    fn set_func_template(&mut self, id: FuncId, template: TemplateId) {
        let index = id as usize;
        if let Some(slot) = self.functions.get_mut(index) {
            *slot = Some(template);
        } else {
            self.functions.resize(index, None);
            self.functions.push(Some(template));
        }
    }

    fn add_template(&mut self, view: TemplateView<'h>) -> TemplateId {
        self.layouts.push(TemplateLayout::default());
        self.views.push(view)
    }

    fn register_type_params(
        &mut self,
        ctx: &Context,
        owner: GenericOwner,
        params: &[perry_hir::types::TypeParam],
    ) {
        for (index, p) in params.iter().enumerate() {
            let name = crate::shapes::TypeName::new(ctx, p.name.clone());
            self.type_slots.insert((owner, name), index);
        }
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
        self.set_func_template(func.id, tid);
        self.scan_params(scan, &func.params, tid, &[]);
        self.scan_stmts(scan, self.view(tid).body, tid, false);
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
        flags: LocalFlags,
    ) -> LocalIdx {
        let lid = id as usize;
        if let Some(existing) = self.local_to_idx.get(lid).copied().flatten() {
            return existing;
        }
        let storage = if flags.contains(LocalFlags::MODULE) {
            LocalStorage::Module(self.add_location(Ty::BOTTOM))
        } else {
            let layout = &mut self.layouts[owner];
            let slot = layout.activation_slots;
            layout.activation_slots += 1;
            LocalStorage::Activation(slot)
        };
        let idx = self.locals.push(LocalInfo {
            id,
            owner,
            declared,
            storage,
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

    pub(super) fn param_local(&self, param: &Param) -> perry_wit_diagnostics::Result<LocalIdx> {
        self.local_idx(param.id).ok_or_else(|| {
            perry_wit_diagnostics::internal!("missing indexed parameter {}", param.name)
        })
    }

    fn scan_params(
        &mut self,
        scan: &mut ScanCtx<'h, '_>,
        params: &'h [Param],
        owner: TemplateId,
        authored_types: &[perry_hir::types::Type],
    ) {
        self.register_type_params(
            scan.ctx,
            GenericOwner::Function(owner),
            self.view(owner).type_params,
        );
        for (param, authored) in params.iter().zip(authored_types) {
            let ty = scan.types.intern_hir(scan.ctx, authored, true);
            self.annotations.0.insert(std::ptr::from_ref(&param.ty), ty);
        }
        for param in params {
            let mut flags = hir_compat::declaration_flags(scan.declarations, param.id);
            let ty = self.annotations.register(
                scan,
                &param.ty,
                flags.contains(LocalFlags::PARAM_ANNOTATED),
            );
            flags.insert(LocalFlags::PARAM);
            self.ensure_local(param.id, owner, ty, flags);
        }
        for param in params {
            scan.defined_locals.insert(param.id as usize);
            if let Some(default) = &param.default {
                self.scan_expr(scan, default, owner, false);
            }
        }
    }

    pub(super) fn has_exit(&self, body: &[Stmt], mask: u8) -> bool {
        self.regions
            .get(&Region::Body(body))
            .map_or(crate::completion::FALLTHROUGH, |r| r.exits)
            & mask
            != 0
    }

    fn record_region(
        &mut self,
        region: Region,
        template: TemplateId,
        start: super::plans::ExprIdx,
        exits: u8,
    ) {
        self.regions.insert(
            region,
            SourceRegion {
                template,
                sites: start..self.expressions.end(),
                exits,
            },
        );
    }

    fn scan_stmts(
        &mut self,
        scan: &mut ScanCtx<'h, '_>,
        stmts: &'h [Stmt],
        owner: TemplateId,
        in_loop: bool,
    ) -> u8 {
        use crate::completion::*;
        let start = self.expressions.end();
        let mut exits = FALLTHROUGH;
        for stmt in stmts {
            let next = match stmt {
                Stmt::Let {
                    id,
                    ty,
                    init,
                    mutable,
                    name,
                    ..
                } => {
                    self.iteration.bind(*id, name, init.as_ref());
                    let flags = hir_compat::declaration_flags(scan.declarations, *id);
                    let declared = self.annotations.register(scan, ty, flags.is_explicit());
                    let local = self.ensure_local(*id, owner, declared, flags);
                    if scan.declarations.for_of(Some(stmt)).is_none() {
                        scan.bindings.push((owner, SourceLocal { local, name }));
                    }
                    if let Some(init) = init {
                        if matches!(init, Expr::Null | Expr::Undefined) {
                            self.locals[local].flags.insert(LocalFlags::NULLISH_INIT);
                        }
                        if !mutable {
                            scan.strings.bind(*id, init);
                        }
                        self.scan_expr(scan, init, owner, in_loop);
                    }
                    scan.defined_locals.insert(*id as usize);
                    FALLTHROUGH
                }
                Stmt::Return(Some(val)) => {
                    if matches!(val, Expr::Array(items, _) if items.is_empty()) {
                        self.returns_empty_list.insert(owner.index());
                    }
                    self.scan_expr(scan, val, owner, in_loop);
                    if matches!(val, Expr::Undefined) {
                        RETURN | EMPTY_RETURN
                    } else {
                        RETURN
                    }
                }
                Stmt::Return(None) => RETURN | EMPTY_RETURN,
                Stmt::Break => BREAK,
                Stmt::Continue => CONTINUE,
                Stmt::LabeledBreak(_) | Stmt::LabeledContinue(_) => LABELLED,
                Stmt::Throw(value) => {
                    self.scan_expr(scan, value, owner, in_loop);
                    RETURN
                }
                Stmt::If {
                    condition,
                    then_branch,
                    else_branch,
                } => {
                    self.scan_test(scan, condition, owner, in_loop);
                    self.scan_stmts(scan, then_branch, owner, in_loop)
                        | else_branch
                            .as_ref()
                            .map_or(FALLTHROUGH, |b| self.scan_stmts(scan, b, owner, in_loop))
                }
                Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                    self.scan_test(scan, condition, owner, true);
                    loop_exits(
                        self.scan_stmts(scan, body, owner, true),
                        matches!(condition, Expr::Bool(true)),
                    )
                }
                Stmt::For {
                    init,
                    condition,
                    update,
                    body,
                } => {
                    if let Some(i) = init {
                        self.scan_stmts(scan, std::slice::from_ref(i), owner, in_loop);
                    }
                    if let Some(c) = condition {
                        self.scan_test(scan, c, owner, true);
                    }
                    if let Some(u) = update {
                        self.scan_test(scan, u, owner, true);
                    }
                    let infinite = scan.declarations.for_of(init.as_deref()).is_none()
                        && condition
                            .as_ref()
                            .is_none_or(|c| matches!(c, Expr::Bool(true)));
                    loop_exits(self.scan_stmts(scan, body, owner, true), infinite)
                }
                Stmt::Try {
                    body,
                    catch,
                    finally,
                    ..
                } => {
                    let mut exits = self.scan_stmts(scan, body, owner, in_loop);
                    if let Some(catch) = catch {
                        if let Some((id, _)) = &catch.param {
                            let err_ty = scan.types.named(scan.ctx, "Error");
                            self.ensure_local(
                                *id,
                                owner,
                                err_ty,
                                hir_compat::declaration_flags(scan.declarations, *id),
                            );
                        }
                        exits |= self.scan_stmts(scan, &catch.body, owner, in_loop);
                    }
                    let cleanup = finally
                        .as_ref()
                        .map_or(FALLTHROUGH, |f| self.scan_stmts(scan, f, owner, in_loop));
                    sequence(cleanup, exits)
                }
                Stmt::Switch {
                    discriminant,
                    cases,
                } => {
                    self.scan_expr(scan, discriminant, owner, in_loop);
                    let mut branches = FALLTHROUGH;
                    let mut has_default = false;
                    for case in cases {
                        has_default |= case.test.is_none();
                        if let Some(t) = &case.test {
                            self.scan_expr(scan, t, owner, in_loop);
                        }
                        branches = (branches & !FALLTHROUGH)
                            | self.scan_stmts(scan, &case.body, owner, in_loop);
                    }
                    if !has_default || branches & BREAK != 0 {
                        branches |= FALLTHROUGH;
                    }
                    branches & !BREAK
                }
                Stmt::Labeled { body, .. } => {
                    FALLTHROUGH
                        | (self.scan_stmts(scan, std::slice::from_ref(body), owner, in_loop)
                            & EMPTY_RETURN)
                }
                Stmt::ReleaseBoxes(ids) => {
                    self.external_uses.extend(ids.iter().map(|&id| (owner, id)));
                    FALLTHROUGH
                }
                other => {
                    stmt_any_expr(other, &mut |e| {
                        self.scan_expr(scan, e, owner, in_loop);
                        false
                    });
                    FALLTHROUGH
                }
            };
            exits = sequence(exits, next);
        }
        if !stmts.is_empty() {
            self.iteration.index_elements(stmts);
            self.record_region(Region::Body(stmts), owner, start, exits);
        }
        exits
    }

    fn scan_test(
        &mut self,
        scan: &mut ScanCtx<'h, '_>,
        expr: &'h Expr,
        owner: TemplateId,
        in_loop: bool,
    ) {
        let start = self.expressions.end();
        self.scan_expr(scan, expr, owner, in_loop);
        self.record_region(
            Region::Expression(expr),
            owner,
            start,
            crate::completion::FALLTHROUGH,
        );
    }

    fn scan_expr(
        &mut self,
        scan: &mut ScanCtx<'h, '_>,
        expr: &'h Expr,
        owner: TemplateId,
        in_loop: bool,
    ) {
        if owner != TemplateId::INIT
            && let Expr::LocalGet(id)
            | Expr::LocalSet(id, _)
            | Expr::Update { id, .. }
            | Expr::ArrayPush { array_id: id, .. }
            | Expr::ArrayPushSpread { array_id: id, .. }
            | Expr::ArrayUnshift { array_id: id, .. }
            | Expr::ArraySplice { array_id: id, .. }
            | Expr::ArrayCopyWithin { array_id: id, .. }
            | Expr::ArrayPop(id)
            | Expr::ArrayShift(id)
            | Expr::SetAdd { set_id: id, .. } = expr
            && self
                .local_idx(*id)
                .is_none_or(|i| self.locals[i].owner != owner)
        {
            self.external_uses.insert((owner, *id));
        }
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
        if let Some(method) = crate::methods::StaticMethod::select(expr, scan.types) {
            self.expressions
                .declared
                .insert(site, SiteResolution::Static(method));
        }
        if let Some(id) = self.receiver_members.bind(expr, scan.decls, scan.locations) {
            self.expressions
                .declared
                .insert(site, SiteResolution::ResourceCall(id));
        }
        if let Expr::RegExp { pattern, flags } = expr {
            let literal = perry_mir::RegexLiteral::intern(scan.ctx, pattern.clone(), flags.clone());
            self.expressions
                .declared
                .insert(site, SiteResolution::Regex(Some(literal)));
        } else if let Some(args) = self.expressions.regexp(expr) {
            scan.regex_constructors.push((site, args));
        }
        match expr {
            Expr::Call { .. }
            | Expr::CallSpread { .. }
            | Expr::StaticMethodCall { .. }
            | Expr::FuncRef(_)
            | Expr::Closure { .. }
            | Expr::LocalSet(..)
            | Expr::Update { .. } => {
                self.events.insert((owner, site), expr);
            }
            Expr::New { class_name, .. } if class_name == "Promise" => {
                scan.promise_sites.insert((owner, site));
            }
            _ => {}
        }
        match expr {
            Expr::Logical { left, right, .. } => {
                self.scan_test(scan, left, owner, in_loop);
                self.scan_expr(scan, right, owner, in_loop);
                return;
            }
            Expr::Conditional {
                condition,
                then_expr,
                else_expr,
            } => {
                self.scan_test(scan, condition, owner, in_loop);
                self.scan_expr(scan, then_expr, owner, in_loop);
                self.scan_expr(scan, else_expr, owner, in_loop);
                return;
            }
            Expr::This => self.views[owner].uses_this = true,
            Expr::LocalGet(id) => self.record_local_use(*id, owner, false),
            Expr::LocalSet(id, _) => {
                self.record_local_use(*id, owner, true);
            }
            Expr::Update { id, .. } => {
                self.record_local_use(*id, owner, true);
            }
            Expr::ScopedTemp { id, .. } => {
                self.ensure_local(
                    *id,
                    owner,
                    Ty::OPEN,
                    hir_compat::declaration_flags(scan.declarations, *id),
                );
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
                is_arrow,
                captures_this,
                captures_new_target,
                ..
            } => {
                self.max_func_id = self.max_func_id.max(*func_id);
                let signature = scan.declarations.closure_signatures.get(func_id);
                let (result, params_ty) = signature
                    .and_then(|alias| match &alias.ty {
                        perry_hir::types::Type::Tuple(types) => types.split_last(),
                        _ => None,
                    })
                    .unwrap_or((return_type, &[]));
                let exp_ret = !matches!(
                    result,
                    perry_hir::types::Type::Any | perry_hir::types::Type::Unknown
                );
                let body = crate::source::authored_body(params, body);
                let tid = self.add_template(TemplateView {
                    kind: TemplateKind::Closure(site),
                    name: scan
                        .closure_names
                        .get(func_id)
                        .map_or("", String::as_str)
                        .into(),
                    func_id: *func_id,
                    params: params.as_slice(),
                    return_type: result,
                    body,
                    captures: captures.as_slice(),
                    type_params: signature.map_or(&[], |s| s.type_params.as_slice()),
                    is_async: *is_async,
                    is_generator: *is_generator,

                    captures_this: *captures_this,
                    captures_new_target: *captures_new_target,
                    uses_this: false,
                    explicit_return: exp_ret,
                });
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
                self.scan_params(scan, params, tid, params_ty);
                self.scan_stmts(scan, body, tid, false);
                let lexical_this = *is_arrow && (*captures_this || self.views[tid].uses_this);
                self.views[tid].captures_this = lexical_this;
                self.views[owner].uses_this |= lexical_this;
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
                is_write && matches!(self.view(user_template).kind, TemplateKind::Closure(_));
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
    strings: crate::source::ConstantStrings<'h>,
    regex_constructors: Vec<(ExprIdx, crate::source::RegExpArguments<'h>)>,
    ctx: &'a Context,
    declarations: &'h crate::source::ModuleDeclarations,
    closure_names: &'h HashMap<FuncId, String>,
    types: &'a mut TypeTable,
    promise_sites: BTreeSet<(TemplateId, ExprIdx)>,
    bindings: Vec<(TemplateId, SourceLocal<'h>)>,
    defined_locals: BitSet,
    pending_captures: Vec<(LocalId, bool, bool)>,
}
