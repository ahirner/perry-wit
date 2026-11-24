//! Final monomorphic HIR emission phase.
mod async_helpers;
mod expressions;
mod funcs;
mod resources;
use super::bitset::BitSet;
use super::boundary::{ModuleIndex, TemplateKind, TemplateView};
use super::solve::{
    ClassInst, FuncCoercion, Instance, Receiver, SiteEntry, SiteResolution, Solved,
};
use super::types::{
    CaptureOwner, CellKind, ClassInstId, FuncIdentity, InstanceId, TemplateId, Ty, TyKind,
    TypeTable, box_hir, func_tag,
};
use crate::hir_compat::{Locations, Metadata};
use perry_hir::{
    ir::{
        BinaryOp, CallArg, CatchClause, Class, CompareOp, Expr, Function, Module as HirModule,
        Stmt, SwitchCase, UnaryOp, UpdateOp,
    },
    types::{FuncId, LocalId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};
use pliron::context::Context;
use std::collections::HashMap;

pub(crate) struct Emitted {
    pub(crate) init: Vec<Stmt>,
    pub(crate) functions: Vec<Function>,
    pub(crate) classes: Vec<Class>,
    pub(crate) remapped_locals: Vec<(LocalId, LocalId)>,
    pub(crate) instantiations: Vec<(String, String, u32, Option<String>)>,
    pub(crate) inline_callbacks: Vec<FuncId>,
    pub(crate) class_origins: Vec<(String, String)>,
    pub(crate) json_eligible_bindings: Vec<LocalId>,
    pub(crate) boxed_locals: Vec<LocalId>,
    pub(crate) deletes_array_elements: bool,
}

impl Emitted {
    pub(crate) fn apply(self, hir: &mut HirModule, metadata: &mut Metadata<'_>) {
        hir.init = self.init;
        hir.functions = self.functions;
        hir.classes = self.classes;
        for (old_id, new_id) in self.remapped_locals {
            if let Some(&span) = hir.local_source_spans.get(&old_id) {
                hir.local_source_spans.insert(new_id, span);
            }
            if metadata.deferred_bindings.contains(&old_id) {
                metadata.deferred_bindings.insert(new_id);
            }
            if metadata.inferred_bindings.contains(&old_id) {
                metadata.inferred_bindings.insert(new_id);
            }
        }
        for id in self.boxed_locals {
            metadata.deferred_bindings.remove(&id);
        }
        for (inst_name, template_name, offset, caller) in self.instantiations {
            metadata
                .locations
                .record_instantiation(inst_name, template_name, offset, caller);
        }
        metadata.inline_callbacks.extend(self.inline_callbacks);
        metadata.class_origins.extend(self.class_origins);
        metadata
            .json_eligible_bindings
            .extend(self.json_eligible_bindings);
        metadata.sparse_arrays |= self.deletes_array_elements;
    }
}

pub(crate) fn build(
    ctx: &Context,
    hir: &HirModule,
    index: &ModuleIndex<'_>,
    metadata: &Metadata<'_>,
    types: &TypeTable,
    solved: &Solved,
) -> Result<Emitted> {
    let max_inst_fid = solved
        .instances
        .iter()
        .map(|i| i.emitted_func_id)
        .max()
        .unwrap_or(0);
    let mut e = Emitter {
        ctx,
        hir,
        index,
        locations: &metadata.locations,
        resource_members: &metadata.resource_members,
        resource_functions: vec![None; metadata.resource_members.len()],
        types,
        solved,
        next_local: index.max_local_id.saturating_add(1),
        next_func_id: index.max_func_id.max(max_inst_fid).saturating_add(1),
        next_class_id: hir
            .classes
            .iter()
            .map(|c| c.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1),
        remapped_locals: Vec::new(),
        instantiations: Vec::new(),
        inline_callbacks: Vec::new(),
        class_origins: Vec::new(),
        json_eligible_bindings: Vec::new(),
        boxed_locals: Vec::new(),
        emitted_env_classes: std::collections::HashSet::new(),
        emitted_timer_class: false,
        synth_classes: Vec::new(),
        synth_functions: Vec::new(),
        helper_fids: vec![None; solved.helpers.len()],
    };
    let (init, mut functions, mut classes) = e.build_all()?;
    functions.extend(e.synth_functions);
    classes.extend(e.synth_classes);
    Ok(Emitted {
        init,
        functions,
        classes,
        remapped_locals: e.remapped_locals,
        instantiations: e.instantiations,
        inline_callbacks: e.inline_callbacks,
        class_origins: e.class_origins,
        json_eligible_bindings: e.json_eligible_bindings,
        boxed_locals: e.boxed_locals,
        deletes_array_elements: solved.deletes_array_elements,
    })
}

type EnvField = (LocalId, String, Ty, bool);

struct Emitter<'h, 'a> {
    ctx: &'a Context,
    hir: &'h HirModule,
    index: &'a ModuleIndex<'h>,
    locations: &'a Locations<'a>,
    resource_members: &'a [crate::wit::ResourceMember],
    resource_functions: Vec<Option<FuncId>>,
    types: &'a TypeTable,
    solved: &'a Solved,
    next_local: LocalId,
    next_func_id: FuncId,
    next_class_id: u32,
    remapped_locals: Vec<(LocalId, LocalId)>,
    instantiations: Vec<(String, String, u32, Option<String>)>,
    inline_callbacks: Vec<FuncId>,
    class_origins: Vec<(String, String)>,
    json_eligible_bindings: Vec<LocalId>,
    boxed_locals: Vec<LocalId>,
    emitted_env_classes: std::collections::HashSet<FuncIdentity>,
    emitted_timer_class: bool,
    synth_classes: Vec<Class>,
    synth_functions: Vec<Function>,
    helper_fids: Vec<Option<FuncId>>,
}

struct RewriteCtx<'a> {
    inst_id: InstanceId,
    func_params: &'a [(LocalId, FuncId)],
    self_local: Option<LocalId>,
    env_local: Option<LocalId>,
    env_captures: &'a [(LocalId, String, bool)],
    boxed_param_map: &'a [(LocalId, LocalId)],
    void_return: bool,
    has_expected: bool,
    remap: Option<&'a mut HashMap<LocalId, LocalId>>,
}
impl RewriteCtx<'_> {
    fn plain(inst_id: InstanceId) -> Self {
        Self {
            inst_id,
            func_params: &[],
            self_local: None,
            env_local: None,
            env_captures: &[],
            boxed_param_map: &[],
            void_return: false,
            has_expected: false,
            remap: None,
        }
    }
    fn with_expected(&self, has_expected: bool) -> RewriteCtx<'_> {
        RewriteCtx {
            inst_id: self.inst_id,
            func_params: self.func_params,
            self_local: self.self_local,
            env_local: self.env_local,
            env_captures: self.env_captures,
            boxed_param_map: self.boxed_param_map,
            void_return: self.void_return,
            has_expected,
            remap: None,
        }
    }
}

impl<'h> Emitter<'h, '_> {
    fn instance(&self, id: InstanceId, msg: &str) -> Result<&Instance> {
        self.solved
            .instances
            .get(id)
            .ok_or_else(|| Diagnostic::internal(msg))
    }
    fn class_inst_name(&self, id: ClassInstId) -> Option<String> {
        Some(self.solved.class_insts[id].emitted_name.clone())
    }
    fn is_resolved(&self, ty: Ty) -> bool {
        !self.types.is_open_with(
            ty,
            self.solved.locations.as_slice(),
            &self.solved.dict_reads,
        )
    }

    fn annotate_local(&self, err: Diagnostic, id: LocalId) -> Diagnostic {
        match self.hir.local_source_spans.get(&id) {
            Some(s) => self.locations.annotate(
                err,
                swc_common::Span::new(swc_common::BytePos(s.start), swc_common::BytePos(s.end)),
            ),
            None => err,
        }
    }

    fn attach_inst_chain(&self, err: Diagnostic, inst_id: InstanceId) -> Diagnostic {
        super::solve::attach_inst_chain(
            &self.solved.instances,
            self.index,
            self.locations,
            err,
            inst_id,
        )
    }

    fn site_entry(&self, inst_id: InstanceId, expr: &Expr) -> Option<&SiteEntry> {
        let template = self.solved.instances.get(inst_id)?.template;
        let local_site = self.index.site_of(template, expr)?;
        self.solved.inst_sites.get(inst_id.index())?.get(local_site)
    }

    fn local_ty(&self, inst_id: InstanceId, id: LocalId) -> Option<Ty> {
        if let Some(lidx) = self.index.local_idx(id) {
            if self.index.local_module.contains(lidx.index()) {
                return self
                    .solved
                    .module_local_tys
                    .get(lidx.index())
                    .copied()
                    .flatten();
            }
            if let Some(slot) = self.index.local_frame_slot(lidx)
                && let Some(inst) = self.solved.instances.get(inst_id)
                && self.index.local_owner.get(lidx.index()) == Some(&inst.template)
            {
                return self
                    .solved
                    .inst_frame_locals
                    .get(inst_id.index())?
                    .get(slot)
                    .copied();
            }
        }
        self.solved
            .inst_local_tys
            .get(inst_id.index())?
            .iter()
            .find(|&&(lid, _)| lid == id)
            .map(|&(_, ty)| ty)
    }

    fn fresh_local(
        &mut self,
        old: LocalId,
        remap: &mut HashMap<LocalId, LocalId>,
    ) -> Result<LocalId> {
        if let Some(&existing) = remap.get(&old) {
            return Ok(existing);
        }
        let next = self.next_local;
        self.next_local = next
            .checked_add(1)
            .ok_or_else(|| Diagnostic::internal("local identifier limit"))?;
        remap.insert(old, next);
        self.remapped_locals.push((old, next));
        Ok(next)
    }

    fn build_all(&mut self) -> Result<(Vec<Stmt>, Vec<Function>, Vec<Class>)> {
        let init_inst = self.solved.init_inst;
        let new_init = self.rewrite_stmts(&mut RewriteCtx::plain(init_inst), &self.hir.init)?;

        let mut seen_templates = BitSet::default();
        let mut inst_remaps: Vec<Option<HashMap<LocalId, LocalId>>> =
            vec![None; self.solved.instances.len()];
        let mut new_functions = Vec::new();
        for (inst_id, inst) in self.solved.instances.iter_enumerated().skip(1) {
            if !self.solved.live_instances.contains(inst_id.index()) {
                continue;
            }
            let Some(view) = self.index.view(inst.template) else {
                continue;
            };
            if !matches!(
                view.kind,
                TemplateKind::Function
                    | TemplateKind::Closure
                    | TemplateKind::PromiseResolve(_)
                    | TemplateKind::PromiseReject(_)
            ) {
                continue;
            }
            let remap_locals = !seen_templates.insert(inst.template.index());
            let inherited = inst
                .capture_owner
                .instance()
                .and_then(|o| inst_remaps.get(o.index()))
                .and_then(Option::as_ref)
                .cloned();
            let (func, remap) = self.emit_function(inst_id, inst, view, remap_locals, inherited)?;
            if !remap.is_empty()
                && let Some(slot) = inst_remaps.get_mut(inst_id.index())
            {
                *slot = Some(remap);
            }
            if !self.is_env_template(inst.template)
                && (inst.is_inline_callback || inst.captures_non_global)
            {
                self.inline_callbacks.push(func.id);
            }
            new_functions.push(func);
        }

        let keep_unexported = new_functions.is_empty();
        for func in &self.hir.functions {
            if new_functions.iter().any(|f| f.id == func.id) {
                continue;
            }
            let concrete = keep_unexported
                && func.type_params.is_empty()
                && func.params.iter().all(|p| !matches!(p.ty, HirType::Any))
                && !matches!(func.return_type, HirType::Any);
            if func.is_generator || concrete {
                new_functions.push(func.clone());
            }
        }

        let mut new_classes: Vec<Class> = self
            .hir
            .classes
            .iter()
            .enumerate()
            .filter(|&(i, _)| self.index.hir_class_is_literal.contains(i))
            .map(|(_, c)| c.clone())
            .collect();
        let mut emitted_classes = BitSet::default();
        for (cinst_i, cinst) in self.solved.class_insts.iter().enumerate() {
            emitted_classes.insert(cinst.class.index());
            let Some(orig) = self
                .index
                .class(cinst.class)
                .and_then(|c| self.hir.classes.get(c.hir_idx))
            else {
                continue;
            };
            if cinst.emitted_name != orig.name {
                self.class_origins
                    .push((cinst.emitted_name.clone(), orig.name.clone()));
            }
            new_classes.push(self.emit_class(
                init_inst,
                cinst.class.index(),
                Some((cinst_i, cinst)),
                orig,
            )?);
        }
        for (cid_i, cinfo) in self.index.classes.iter().enumerate() {
            if emitted_classes.contains(cid_i) {
                continue;
            }
            let Some(orig) = self.hir.classes.get(cinfo.hir_idx) else {
                continue;
            };
            let has_statics = self
                .solved
                .class_static_methods
                .get(cid_i)
                .is_some_and(|v| !v.is_empty());
            if has_statics || !orig.static_fields.is_empty() || orig.extends_name.is_some() {
                new_classes.push(self.emit_class(init_inst, cid_i, None, orig)?);
            }
        }
        Ok((new_init, new_functions, new_classes))
    }

    fn emit_class(
        &mut self,
        init_inst: InstanceId,
        cid_i: usize,
        instantiated: Option<(usize, &ClassInst)>,
        orig: &'h Class,
    ) -> Result<Class> {
        let mut cls = orig.clone();
        cls.type_params.clear();
        let init_ctx = RewriteCtx::plain(init_inst);
        let static_tys = self.solved.class_static_field_tys.get(cid_i);
        cls.static_fields = orig
            .static_fields
            .iter()
            .enumerate()
            .map(|(f_idx, field)| {
                let mut f = field.clone();
                if let Some(&ty) = static_tys.and_then(|tys| tys.get(f_idx))
                    && self.is_resolved(ty)
                {
                    f.ty = self.materialize(ty)?;
                }
                f.init = self.rewrite_opt_expr(&init_ctx, field.init.as_ref())?;
                Ok(f)
            })
            .collect::<Result<Vec<_>>>()?;
        cls.static_methods =
            self.emit_named_methods(slice_at(&self.solved.class_static_methods, cid_i))?;
        let Some((cinst_i, cinst)) = instantiated else {
            cls.constructor = None;
            cls.methods.clear();
            cls.getters.clear();
            cls.setters.clear();
            return Ok(cls);
        };
        cls.name = cinst.emitted_name.clone();
        let field_locs = self
            .solved
            .field_locs
            .get(cinst.field_locs.range())
            .ok_or_else(|| Diagnostic::internal("invalid class field range"))?;
        let mut fields = Vec::with_capacity(orig.fields.len());
        for (f_idx, field) in orig.fields.iter().enumerate() {
            let resolved = field_locs
                .get(f_idx)
                .map_or(Ty::OPEN, |&loc| self.solved.locations[loc]);
            if self.is_zero_sized_func(resolved) {
                continue;
            }
            let mut f = field.clone();
            if self.is_resolved(resolved) {
                f.ty = self.materialize(resolved)?;
            } else if matches!(
                self.types.kind(resolved),
                TyKind::Cell {
                    kind: CellKind::Array,
                    ..
                }
            ) {
                return Err(diag!(
                    codes::MISSING_TYPE_ANNOTATION,
                    "an empty array literal needs a declared element type"
                )
                .with_help(format!(
                    "Add a concrete element type annotation to field '{}'.",
                    field.name
                )));
            } else if resolved == Ty::BOTTOM && matches!(field.ty, HirType::Any | HirType::Unknown)
            {
                return Err(diag!(
                    codes::MISSING_TYPE_ANNOTATION,
                    "the value type cannot be inferred in field '{}'",
                    field.name
                )
                .with_help(
                    "Add a concrete type annotation to this binding, parameter, or function return.",
                ));
            }
            if let Some(init) = &field.init {
                let owner = cinst
                    .field_init
                    .ok_or_else(|| Diagnostic::internal("missing field initializer instance"))?;
                f.init = Some(self.rewrite_expr(&RewriteCtx::plain(owner), init)?);
            }
            fields.push(f);
        }
        cls.fields = fields;
        cls.constructor = match cinst.ctor_inst {
            Some(ctor_id)
                if let Some(inst) = self.solved.instances.get(ctor_id)
                    && let Some(view) = self.index.view(inst.template) =>
            {
                Some(self.emit_function(ctor_id, inst, view, false, None)?.0)
            }
            _ => orig.constructor.clone(),
        };
        cls.methods =
            self.emit_named_methods(slice_at(&self.solved.class_inst_methods, cinst_i))?;
        cls.getters = self.emit_accessors(
            slice_at(&self.solved.class_inst_getters, cinst_i),
            &orig.getters,
        )?;
        cls.setters = self.emit_accessors(
            slice_at(&self.solved.class_inst_setters, cinst_i),
            &orig.setters,
        )?;
        Ok(cls)
    }

    fn emit_named_methods(&mut self, ids: &[InstanceId]) -> Result<Vec<Function>> {
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            let inst = &self.solved.instances[id];
            let Some(view) = self.index.view(inst.template) else {
                continue;
            };
            let (mut m, _) = self.emit_function(id, inst, view, false, None)?;
            m.name = inst.emitted_method_name.clone();
            out.push(m);
        }
        Ok(out)
    }

    fn emit_accessors(
        &mut self,
        ids: &[(usize, InstanceId)],
        orig: &[(String, Function)],
    ) -> Result<Vec<(String, Function)>> {
        let mut out = Vec::with_capacity(ids.len());
        for &(acc_idx, id) in ids {
            let inst = &self.solved.instances[id];
            let Some(view) = self.index.view(inst.template) else {
                continue;
            };
            let (f, _) = self.emit_function(id, inst, view, false, None)?;
            out.push((
                orig.get(acc_idx)
                    .map_or(view.name, |(n, _)| n.as_str())
                    .to_owned(),
                f,
            ));
        }
        Ok(out)
    }

    fn emit_function(
        &mut self,
        inst_id: InstanceId,
        inst: &Instance,
        view: &TemplateView<'h>,
        remap_locals: bool,
        inherited: Option<HashMap<LocalId, LocalId>>,
    ) -> Result<(Function, HashMap<LocalId, LocalId>)> {
        if matches!(
            view.kind,
            TemplateKind::PromiseResolve(_) | TemplateKind::PromiseReject(_)
        ) {
            return Ok((
                self.emit_promise_handle_function(inst, view)?,
                HashMap::new(),
            ));
        }
        let mut remap = inherited.unwrap_or_default();
        let has_inherited = !remap.is_empty();
        let mut params = Vec::with_capacity(view.params.len() + 2);
        let mut static_func_params: Vec<(LocalId, FuncId)> = Vec::new();
        let is_env_fn = self.is_env_template(inst.template);
        let self_ident = FuncIdentity {
            template: inst.template,
            capture_owner: inst.capture_owner,
        };
        let (env_local, env_captures) = if is_env_fn {
            let env_class = self.ensure_env_class(self_ident)?;
            let fields = self.env_layout(self_ident)?;
            let env_pid = self.fresh_synthetic_local()?;
            let cap_entries: Vec<(LocalId, String, bool)> = fields
                .into_iter()
                .map(|(cap_id, fname, _, boxed)| (cap_id, fname, boxed))
                .collect();
            params.push(crate::hir_compat::resource_classes::synth_param(
                env_pid,
                "__env".to_owned(),
                HirType::Named(env_class),
            ));
            (Some(env_pid), cap_entries)
        } else {
            (None, Vec::new())
        };
        let self_local = match inst.receiver {
            Some(Receiver::Object(obj_ty)) => {
                let sid = self.fresh_synthetic_local()?;
                params.push(crate::hir_compat::resource_classes::synth_param(
                    sid,
                    "__self".to_owned(),
                    self.materialize(obj_ty)?,
                ));
                Some(sid)
            }
            _ => None,
        };

        let mut boxed_param_map: Vec<(LocalId, LocalId)> = Vec::new();
        let mut boxed_param_inits: Vec<Stmt> = Vec::new();
        for (p_idx, param) in view.params.iter().enumerate() {
            let param_loc = *self
                .solved
                .param_locs
                .get(inst.param_locs.range())
                .and_then(|s| s.get(p_idx))
                .ok_or_else(|| {
                    Diagnostic::internal("emitted function missing parameter location slot")
                })?;
            let param_ty = *self
                .solved
                .locations
                .get(param_loc)
                .ok_or_else(|| Diagnostic::internal("parameter location out of range"))?;
            if let TyKind::Func(ident) = self.types.kind(param_ty)
                && !self.is_env_template(ident.template)
            {
                if let Some(fid) = self.find_func_identity_instance(ident) {
                    static_func_params.push((param.id, fid));
                }
                continue;
            }
            let mut new_param = param.clone();
            if self.is_resolved(param_ty) {
                new_param.ty = self.materialize(param_ty)?;
            } else if !self.index.is_param_annotated(param.id)
                && inst.witness.is_none()
                && matches!(param.ty, HirType::Any | HirType::Unknown)
            {
                let err = diag!(codes::MISSING_TYPE_ANNOTATION, "cannot infer the type of parameter '{}' of '{}'", param.name, view.display_name())
                    .with_help("Add a concrete type annotation to this binding, parameter, or function return.");
                let err = self.annotate_local(err, param.id);
                return Err(self.attach_inst_chain(err, inst_id));
            }
            if remap_locals {
                new_param.id = self.fresh_local(param.id, &mut remap)?;
            }
            let param_ctx = RewriteCtx {
                inst_id,
                func_params: &static_func_params,
                self_local,
                env_local,
                env_captures: &env_captures,
                boxed_param_map: &boxed_param_map,
                void_return: false,
                has_expected: false,
                remap: None,
            };
            new_param.default = self.rewrite_opt_expr(&param_ctx, param.default.as_ref())?;
            if self.is_boxed_local(param.id) {
                let box_id = self.fresh_synthetic_local()?;
                boxed_param_map.push((param.id, box_id));
                let init_expr = Self::emit_box_record(Expr::LocalGet(new_param.id));
                boxed_param_inits.push(Stmt::Let {
                    id: box_id,
                    name: format!("__box_{}", param.name),
                    ty: box_hir(new_param.ty.clone()),
                    mutable: false,
                    init: Some(init_expr),
                });
            }
            params.push(new_param);
        }

        let ret_ty = self.solved.locations[inst.return_loc];
        let void_return = self.is_zero_sized_func(ret_ty);
        let return_type = if void_return {
            HirType::Void
        } else if self.is_resolved(ret_ty) {
            self.materialize(ret_ty)?
        } else if self
            .solved
            .self_nested_locs
            .contains(inst.return_loc.index())
        {
            let err = diag!(
                codes::MISSING_TYPE_ANNOTATION,
                "cannot infer return type of '{}': inferred type nests itself",
                view.display_name()
            )
            .with_help("Add a concrete return type annotation to this function.");
            let err = match self.locations.function_span(view.name) {
                Some(span) => err.with_span(span),
                None => err,
            };
            return Err(self.attach_inst_chain(err, inst_id));
        } else {
            view.return_type.clone()
        };
        let mut rctx = RewriteCtx {
            inst_id,
            func_params: &static_func_params,
            self_local,
            env_local,
            env_captures: &env_captures,
            boxed_param_map: &boxed_param_map,
            void_return,
            has_expected: false,
            remap: remap_locals.then_some(&mut remap),
        };
        let mut body = self.rewrite_stmts(&mut rctx, view.body)?;
        if remap_locals || has_inherited {
            for p in &mut params {
                if let Some(d) = &mut p.default {
                    perry_hir::analysis::remap_local_ids_in_expr(d, &remap);
                }
            }
            perry_hir::analysis::remap_local_ids_in_stmts(&mut body, &remap);
        }
        if !boxed_param_inits.is_empty() {
            boxed_param_inits.extend(body);
            body = boxed_param_inits;
        }

        let mut captures = if is_env_fn {
            Vec::new()
        } else {
            let mut caps = view.captures.to_vec();
            if let Some(extra) = self.solved.inst_captures.get(inst_id.index()) {
                caps.extend(extra.iter().copied());
            }
            if !remap.is_empty() {
                for cap in &mut caps {
                    if let Some(&m) = remap.get(cap) {
                        *cap = m;
                    }
                }
            }
            caps.sort_unstable();
            caps.dedup();
            caps
        };
        captures.retain(|c| !self.is_boxed_local(*c) || !is_env_fn);

        if inst.witness.is_none() && inst.caller.is_some() {
            let caller_name = inst
                .caller
                .and_then(|c| self.solved.instances.get(c))
                .filter(|c| c.template != TemplateId::INIT)
                .map(|c| c.emitted_name.clone());
            self.instantiations.push((
                inst.emitted_name.clone(),
                view.raw_name(),
                inst.call_byte_offset,
                caller_name,
            ));
        }

        Ok((
            Function {
                id: inst.emitted_func_id,
                name: inst.emitted_name.clone(),
                type_params: Vec::new(),
                params,
                return_type,
                body,
                captures,
                is_async: view.is_async,
                is_generator: view.is_generator,
                is_strict: view.is_strict,
                construct_only: view.construct_only,
                is_exported: inst.witness.is_some(),
                decorators: Vec::new(),
                was_plain_async: false,
                was_unrolled: false,
            },
            remap,
        ))
    }

    fn find_func_identity_instance(&self, ident: FuncIdentity) -> Option<FuncId> {
        self.solved
            .instances
            .iter_enumerated()
            .find_map(|(id, inst)| {
                (self.solved.live_instances.contains(id.index())
                    && inst.template == ident.template
                    && inst.capture_owner == ident.capture_owner)
                    .then_some(inst.emitted_func_id)
            })
            .or_else(|| self.index.view(ident.template).map(|v| v.func_id))
    }

    fn rewrite_condition(&mut self, rctx: &RewriteCtx<'_>, expr: &'h Expr) -> Result<Expr> {
        match expr {
            Expr::Logical {
                op: op @ (perry_hir::ir::LogicalOp::And | perry_hir::ir::LogicalOp::Or),
                left,
                right,
            } => {
                return Ok(Expr::Logical {
                    op: *op,
                    left: Box::new(self.rewrite_condition(rctx, left)?),
                    right: Box::new(self.rewrite_condition(rctx, right)?),
                });
            }
            Expr::Sequence(parts) if let Some((last, prefix)) = parts.split_last() => {
                let mut values = Vec::with_capacity(parts.len());
                for effect in prefix {
                    values.push(self.rewrite_expr(rctx, effect)?);
                }
                values.push(self.rewrite_condition(rctx, last)?);
                return Ok(Expr::Sequence(values));
            }
            _ => {}
        }
        let rewritten = self.rewrite_expr(rctx, expr)?;
        Ok(match rewritten {
            Expr::FuncRef(_) => Expr::Bool(true),
            other => other,
        })
    }

    fn rewrite_opt_expr(
        &mut self,
        rctx: &RewriteCtx<'_>,
        expr: Option<&'h Expr>,
    ) -> Result<Option<Expr>> {
        expr.map(|e| self.rewrite_expr(rctx, e)).transpose()
    }
    fn rewrite_stmts(&mut self, rctx: &mut RewriteCtx<'_>, stmts: &'h [Stmt]) -> Result<Vec<Stmt>> {
        let mut out = Vec::with_capacity(stmts.len());
        for stmt in stmts {
            if let Stmt::Let { id, name, ty, .. } = stmt
                && self.is_forward_boxed(*id)
            {
                let out_id = match &mut rctx.remap {
                    Some(r) => self.fresh_local(*id, r)?,
                    None => *id,
                };
                let inner_ty = self.resolve_let_ty(rctx.inst_id, *id, out_id, name, ty)?;
                let box_ty = box_hir(Self::admitting_undefined_hir(inner_ty));
                self.boxed_locals.push(out_id);
                out.push(Stmt::Let {
                    id: out_id,
                    name: name.clone(),
                    ty: box_ty,
                    mutable: false,
                    init: Some(Self::emit_box_record(Expr::Undefined)),
                });
            }
        }
        let mut pending_for_boxes: Vec<LocalId> = Vec::new();
        for stmt in stmts {
            match stmt {
                Stmt::Let { id, .. }
                    if self.index.is_for_head_local(*id) && self.is_boxed_local(*id) =>
                {
                    let rewritten = self.rewrite_stmt(rctx, stmt)?;
                    if let Stmt::Let { id: out_id, .. } = &rewritten {
                        pending_for_boxes.push(*out_id);
                    }
                    out.push(rewritten);
                }
                Stmt::For {
                    init,
                    condition,
                    update,
                    body,
                } => {
                    let mut box_ids = std::mem::take(&mut pending_for_boxes);
                    let rewritten_init = init
                        .as_deref()
                        .map(|s| {
                            let r = self.rewrite_stmt(rctx, s)?;
                            if let (Stmt::Let { id: orig_id, .. }, Stmt::Let { id: out_id, .. }) =
                                (s, &r)
                                && self.index.is_for_head_local(*orig_id)
                                && self.is_boxed_local(*orig_id)
                            {
                                box_ids.push(*out_id);
                            }
                            Ok::<_, Diagnostic>(Box::new(r))
                        })
                        .transpose()?;
                    let rewritten_cond = self.rewrite_opt_expr(rctx, condition.as_ref())?;
                    let mut rewritten_update = self.rewrite_opt_expr(rctx, update.as_ref())?;
                    let rewritten_body = self.rewrite_stmts(rctx, body)?;
                    if !box_ids.is_empty() {
                        let mut seq = Vec::with_capacity(box_ids.len() + 1);
                        for box_id in box_ids {
                            let fresh =
                                Self::emit_box_record(Self::prop_get(Expr::LocalGet(box_id), "v"));
                            seq.push(Expr::LocalSet(box_id, Box::new(fresh)));
                        }
                        if let Some(u) = rewritten_update {
                            seq.push(u);
                        }
                        rewritten_update = Some(Expr::Sequence(seq));
                    }
                    out.push(Stmt::For {
                        init: rewritten_init,
                        condition: rewritten_cond,
                        update: rewritten_update,
                        body: rewritten_body,
                    });
                }
                _ => {
                    pending_for_boxes.clear();
                    out.push(self.rewrite_stmt(rctx, stmt)?);
                }
            }
        }
        Ok(out)
    }

    fn resolve_let_ty(
        &mut self,
        inst_id: InstanceId,
        id: LocalId,
        out_id: LocalId,
        name: &str,
        ty: &HirType,
    ) -> Result<HirType> {
        let resolved = self.local_ty(inst_id, id);
        let is_inferred = !self.hir.local_source_spans.contains_key(&id)
            || self
                .index
                .local_idx(id)
                .is_none_or(|li| self.index.local_inferred.contains(li.index()));
        let is_deferred = self
            .index
            .local_idx(id)
            .is_some_and(|li| self.index.local_deferred.contains(li.index()));
        match resolved {
            Some(t)
                if self.is_resolved(t)
                    && (is_inferred
                        || matches!(ty, HirType::Any | HirType::Unknown)
                        || matches!(ty, HirType::Array(e) if matches!(**e, HirType::Any | HirType::Unknown))
                        || matches!(self.types.kind(t), TyKind::ClassInst(_))) =>
            {
                if is_inferred
                    && is_deferred
                    && self.types.is_json_encodable(
                        self.ctx,
                        t,
                        self.solved.locations.as_slice(),
                        &self.solved.dict_reads,
                    )
                {
                    self.json_eligible_bindings.push(out_id);
                }
                self.materialize(t)
            }
            Some(t)
                if matches!(
                    self.types.kind(t),
                    TyKind::Cell {
                        kind: CellKind::Dict,
                        ..
                    }
                ) =>
            {
                self.materialize(t)
            }
            Some(t)
                if let TyKind::Cell { loc, kind } = self.types.kind(t)
                    && kind.is_array()
                    && !self.is_resolved(t) =>
            {
                let mut err = diag!(
                    codes::MISSING_TYPE_ANNOTATION,
                    "an empty array literal needs a declared element type"
                )
                .with_help(format!(
                    "Add a concrete element type annotation to '{name}'."
                ));
                if self.solved.self_nested_locs.contains(loc.index()) {
                    err = err.with_note("its inferred element type nests itself");
                }
                let err = self.annotate_local(err, id);
                Err(self.attach_inst_chain(err, inst_id))
            }
            Some(Ty::OPEN)
                if is_inferred
                    && self.index.local_idx(id).is_some_and(|lidx| {
                        self.solved.self_nested_locals.contains(lidx.index())
                    }) =>
            {
                let err = diag!(
                    codes::MISSING_TYPE_ANNOTATION,
                    "cannot infer type of '{name}': inferred type nests itself"
                )
                .with_help("Add a concrete type annotation to this binding.");
                let err = self.annotate_local(err, id);
                Err(self.attach_inst_chain(err, inst_id))
            }
            _ => Ok(ty.clone()),
        }
    }

    fn rewrite_stmt(&mut self, rctx: &mut RewriteCtx<'_>, stmt: &'h Stmt) -> Result<Stmt> {
        match stmt {
            Stmt::Let {
                id,
                name,
                ty,
                mutable,
                init,
            } => {
                let out_id = match &mut rctx.remap {
                    Some(r) => self.fresh_local(*id, r)?,
                    None => *id,
                };
                let resolved = self.local_ty(rctx.inst_id, *id);
                if let Some(t) = resolved
                    && let TyKind::Func(ident) = self.types.kind(t)
                    && !self.is_env_template(ident.template)
                {
                    if let Some(init_expr) = init
                        && !is_pure_func_expr(init_expr)
                    {
                        return Ok(Stmt::Expr(self.rewrite_expr(rctx, init_expr)?));
                    }
                    let fid = self.find_func_identity_instance(ident).ok_or_else(|| {
                        Diagnostic::internal(format!(
                            "unresolved function identity for binding '{name}'"
                        ))
                    })?;
                    return Ok(Stmt::Let {
                        id: out_id,
                        name: name.clone(),
                        ty: ty.clone(),
                        mutable: *mutable,
                        init: Some(Expr::FuncRef(fid)),
                    });
                }
                if self.is_forward_boxed(*id) {
                    let init_ctx = rctx.with_expected(true);
                    return match self.rewrite_opt_expr(&init_ctx, init.as_ref())? {
                        Some(val) => Ok(Stmt::Expr(Expr::PropertySet {
                            object: Box::new(Expr::LocalGet(out_id)),
                            property: "v".into(),
                            value: Box::new(val),
                            byte_offset: 0,
                            byte_end: 0,
                        })),
                        None => Ok(Stmt::Expr(Expr::Undefined)),
                    };
                }
                let new_ty = self.resolve_let_ty(rctx.inst_id, *id, out_id, name, ty)?;
                let init_has_expected = !matches!(new_ty, HirType::Any | HirType::Unknown);
                let init_ctx = rctx.with_expected(init_has_expected);
                let rewritten_init = self.rewrite_opt_expr(&init_ctx, init.as_ref())?;
                if self.is_boxed_local(*id) {
                    self.boxed_locals.push(out_id);
                    let undef_box = self.is_undef_boxed(*id);
                    let (box_inner_ty, init_val) =
                        if undef_box && matches!(init, None | Some(Expr::Undefined)) {
                            (Self::admitting_undefined_hir(new_ty), Expr::Undefined)
                        } else {
                            let v = match rewritten_init {
                                Some(v) => v,
                                None => self.default_box_init(&new_ty, *id, rctx.inst_id)?,
                            };
                            let t = if undef_box {
                                Self::admitting_undefined_hir(new_ty)
                            } else {
                                new_ty
                            };
                            (t, v)
                        };
                    return Ok(Stmt::Let {
                        id: out_id,
                        name: name.clone(),
                        ty: box_hir(box_inner_ty),
                        mutable: self.index.is_for_head_local(*id),
                        init: Some(Self::emit_box_record(init_val)),
                    });
                }
                Ok(Stmt::Let {
                    id: out_id,
                    name: name.clone(),
                    ty: new_ty,
                    mutable: *mutable,
                    init: rewritten_init,
                })
            }
            Stmt::Expr(e) => Ok(Stmt::Expr(self.rewrite_expr(rctx, e)?)),
            Stmt::Throw(e) => Ok(Stmt::Throw(self.rewrite_expr(rctx, e)?)),
            Stmt::Return(v) if rctx.void_return => {
                if let Some(e) = v
                    && !is_pure_func_expr(e)
                {
                    let eff = self.rewrite_expr(rctx, e)?;
                    return Ok(Stmt::Return(Some(Expr::Sequence(vec![
                        eff,
                        Expr::Undefined,
                    ]))));
                }
                Ok(Stmt::Return(None))
            }
            Stmt::Return(v) => {
                let ret_ctx = rctx.with_expected(true);
                Ok(Stmt::Return(self.rewrite_opt_expr(&ret_ctx, v.as_ref())?))
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => Ok(Stmt::If {
                condition: self.rewrite_condition(rctx, condition)?,
                then_branch: self.rewrite_stmts(rctx, then_branch)?,
                else_branch: else_branch
                    .as_deref()
                    .map(|b| self.rewrite_stmts(rctx, b))
                    .transpose()?,
            }),
            Stmt::While { condition, body } => Ok(Stmt::While {
                condition: self.rewrite_condition(rctx, condition)?,
                body: self.rewrite_stmts(rctx, body)?,
            }),
            Stmt::DoWhile { condition, body } => Ok(Stmt::DoWhile {
                condition: self.rewrite_condition(rctx, condition)?,
                body: self.rewrite_stmts(rctx, body)?,
            }),
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => Ok(Stmt::For {
                init: init
                    .as_deref()
                    .map(|s| self.rewrite_stmt(rctx, s).map(Box::new))
                    .transpose()?,
                condition: self.rewrite_opt_expr(rctx, condition.as_ref())?,
                update: self.rewrite_opt_expr(rctx, update.as_ref())?,
                body: self.rewrite_stmts(rctx, body)?,
            }),
            Stmt::Try {
                body,
                catch,
                finally,
                forof_completion,
            } => Ok(Stmt::Try {
                body: self.rewrite_stmts(rctx, body)?,
                catch: catch
                    .as_ref()
                    .map(|c| {
                        let param = match (&c.param, &mut rctx.remap) {
                            (Some((id, name)), Some(r)) => {
                                Some((self.fresh_local(*id, r)?, name.clone()))
                            }
                            (other, _) => other.clone(),
                        };
                        Ok::<_, Diagnostic>(CatchClause {
                            param,
                            body: self.rewrite_stmts(rctx, &c.body)?,
                        })
                    })
                    .transpose()?,
                finally: finally
                    .as_deref()
                    .map(|f| self.rewrite_stmts(rctx, f))
                    .transpose()?,
                forof_completion: *forof_completion,
            }),
            Stmt::Switch {
                discriminant,
                cases,
            } => Ok(Stmt::Switch {
                discriminant: self.rewrite_expr(rctx, discriminant)?,
                cases: cases
                    .iter()
                    .map(|c| {
                        Ok(SwitchCase {
                            test: self.rewrite_opt_expr(rctx, c.test.as_ref())?,
                            body: self.rewrite_stmts(rctx, &c.body)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            }),
            Stmt::Labeled { label, body } => Ok(Stmt::Labeled {
                label: label.clone(),
                body: Box::new(self.rewrite_stmt(rctx, body)?),
            }),
            other => Ok(other.clone()),
        }
    }

    fn default_box_init(&self, ty: &HirType, id: LocalId, inst_id: InstanceId) -> Result<Expr> {
        match ty {
            HirType::Void => Ok(Expr::Undefined),
            HirType::Null => Ok(Expr::Null),
            HirType::Union(members) if members.contains(&HirType::Void) => Ok(Expr::Undefined),
            HirType::Union(members) if members.contains(&HirType::Null) => Ok(Expr::Null),
            HirType::Number | HirType::Int32 => Ok(Expr::Number(0.0)),
            HirType::Boolean => Ok(Expr::Bool(false)),
            HirType::String => Ok(Expr::String(String::new())),
            _ => {
                let err = self.annotate_local(
                    Diagnostic::unsupported(
                        "captured local declared without an initializer has no placeholder value",
                    ),
                    id,
                );
                Err(self.attach_inst_chain(err, inst_id))
            }
        }
    }
}

fn sequence(mut effects: Vec<Expr>, value: Expr) -> Expr {
    if effects.is_empty() {
        return value;
    }
    effects.push(value);
    Expr::Sequence(effects)
}

fn is_pure_func_expr(expr: &Expr) -> bool {
    match expr {
        Expr::FuncRef(_) | Expr::Closure { .. } | Expr::LocalGet(_) | Expr::This => true,
        Expr::PropertyGet { object, .. } => is_pure_func_expr(object),
        _ => false,
    }
}

fn slice_at<T>(lists: &[Vec<T>], idx: usize) -> &[T] {
    lists.get(idx).map_or(&[], Vec::as_slice)
}
