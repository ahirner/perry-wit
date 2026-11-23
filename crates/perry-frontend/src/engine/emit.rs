//! Final monomorphic HIR emission phase.
use super::bitset::BitSet;
use super::boundary::{ModuleIndex, TemplateKind, TemplateView};
use super::solve::{ClassInst, Instance, SiteResolution, Solved};
use super::types::{
    CellKind, ClassInstId, FuncIdentity, InstanceId, LocId, TemplateId, Ty, TyKind, TypeTable,
};
use crate::hir_compat::{Locations, Metadata};
use perry_hir::{
    ir::{CallArg, CatchClause, Class, Expr, Function, Module as HirModule, Stmt, SwitchCase},
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
    pub(crate) json_storage_bindings: Vec<LocalId>,
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
        for (inst_name, template_name, offset, caller) in self.instantiations {
            metadata
                .locations
                .record_instantiation(inst_name, template_name, offset, caller);
        }
        metadata.inline_callbacks.extend(self.inline_callbacks);
        metadata.class_origins.extend(self.class_origins);
        metadata
            .json_storage_bindings
            .extend(self.json_storage_bindings);
        metadata.sparse_arrays |= self.deletes_array_elements;
    }
}

pub(crate) fn build(
    ctx: &Context,
    hir: &HirModule,
    index: &ModuleIndex<'_>,
    locations: &Locations<'_>,
    types: &TypeTable,
    solved: &Solved,
) -> Result<Emitted> {
    let mut e = Emitter {
        ctx,
        hir,
        index,
        locations,
        types,
        solved,
        next_local: index.max_local_id.saturating_add(1),
        remapped_locals: Vec::new(),
        instantiations: Vec::new(),
        inline_callbacks: Vec::new(),
        class_origins: Vec::new(),
        json_storage_bindings: Vec::new(),
    };
    let (init, functions, classes) = e.build_all()?;
    Ok(Emitted {
        init,
        functions,
        classes,
        remapped_locals: e.remapped_locals,
        instantiations: e.instantiations,
        inline_callbacks: e.inline_callbacks,
        class_origins: e.class_origins,
        json_storage_bindings: e.json_storage_bindings,
        deletes_array_elements: solved.deletes_array_elements,
    })
}

struct Emitter<'h, 'a> {
    ctx: &'a Context,
    hir: &'h HirModule,
    index: &'a ModuleIndex<'h>,
    locations: &'a Locations<'a>,
    types: &'a TypeTable,
    solved: &'a Solved,
    next_local: LocalId,
    remapped_locals: Vec<(LocalId, LocalId)>,
    instantiations: Vec<(String, String, u32, Option<String>)>,
    inline_callbacks: Vec<FuncId>,
    class_origins: Vec<(String, String)>,
    json_storage_bindings: Vec<LocalId>,
}

struct RewriteCtx<'a> {
    inst_id: InstanceId,
    func_params: &'a [(LocalId, FuncId)],
    remap: Option<&'a mut HashMap<LocalId, LocalId>>,
}
impl RewriteCtx<'_> {
    fn plain(inst_id: InstanceId) -> Self {
        Self {
            inst_id,
            func_params: &[],
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
        self.solved
            .class_insts
            .get(id)
            .map(|c| c.emitted_name.clone())
    }
    fn materialize(&self, ty: Ty) -> HirType {
        self.types.materialize_hir_with(
            self.ctx,
            ty,
            self.solved.locations.as_slice(),
            &self.solved.dict_reads,
            &|cid| self.class_inst_name(cid),
        )
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

    fn site_of(&self, inst_id: InstanceId, expr: &Expr) -> Option<&SiteResolution> {
        let template = self.solved.instances.get(inst_id)?.template;
        let local_site = self.index.site_of(template, expr)?;
        self.solved
            .inst_sites
            .get(inst_id.0 as usize)?
            .get(local_site)?
            .as_ref()
    }

    fn local_ty(&self, inst_id: InstanceId, id: LocalId) -> Option<Ty> {
        if let Some(lidx) = self.index.local_idx(id) {
            if self.index.local_module.contains(lidx.0 as usize) {
                return self
                    .solved
                    .module_local_tys
                    .get(lidx.0 as usize)
                    .copied()
                    .flatten();
            }
            if let Some(slot) = self.index.local_frame_slot(lidx)
                && let Some(inst) = self.solved.instances.get(inst_id)
                && self.index.local_owner.get(lidx.0 as usize) == Some(&inst.template)
            {
                return self
                    .solved
                    .inst_frame_locals
                    .get(inst_id.0 as usize)?
                    .get(slot)
                    .copied();
            }
        }
        self.solved
            .inst_local_tys
            .get(inst_id.0 as usize)?
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
        let init_inst = InstanceId(0);
        let new_init = self.rewrite_stmts(&mut RewriteCtx::plain(init_inst), &self.hir.init)?;

        let mut seen_templates = BitSet::default();
        let mut inst_remaps: Vec<Option<HashMap<LocalId, LocalId>>> =
            vec![None; self.solved.instances.len()];
        let mut new_functions = Vec::new();
        for (inst_id, inst) in self.solved.instances.iter_enumerated().skip(1) {
            if !self.solved.live_instances.contains(inst_id.0 as usize) {
                continue;
            }
            let Some(view) = self.index.view(inst.template) else {
                continue;
            };
            if !matches!(view.kind, TemplateKind::Function | TemplateKind::Closure) {
                continue;
            }
            let keep_original = view.is_exported || view.is_obj_method();
            let remap_locals = !keep_original && !seen_templates.insert(inst.template.0 as usize);
            let inherited = inst
                .capture_owner
                .and_then(|o| inst_remaps.get(o.0 as usize))
                .and_then(Option::as_ref)
                .cloned();
            let (func, remap) = self.emit_function(inst_id, inst, view, remap_locals, inherited)?;
            if !remap.is_empty()
                && let Some(slot) = inst_remaps.get_mut(inst_id.0 as usize)
            {
                *slot = Some(remap);
            }
            if inst.is_inline_callback || inst.captures_non_global {
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
            emitted_classes.insert(cinst.class.0 as usize);
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
                cinst.class.0 as usize,
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
                    f.ty = self.materialize(ty);
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
        cls.fields = orig.fields.iter().enumerate().map(|(f_idx, field)| {
            let resolved = self.solved.locations[LocId(cinst.field_loc_base + f_idx as u32)];
            let mut f = field.clone();
            if self.is_resolved(resolved) { f.ty = self.materialize(resolved); }
            else if matches!(self.types.kind(resolved), TyKind::Cell { kind: CellKind::Array, .. }) {
                return Err(diag!(codes::MISSING_TYPE_ANNOTATION, "an empty array literal needs a declared element type")
                    .with_help(format!("Add a concrete element type annotation to field '{}'.", field.name)));
            } else if resolved == Ty::BOTTOM && matches!(field.ty, HirType::Any | HirType::Unknown) {
                return Err(diag!(codes::MISSING_TYPE_ANNOTATION, "the value type cannot be inferred in field '{}'", field.name)
                    .with_help("Add a concrete type annotation to this binding, parameter, or function return."));
            }
            f.init = self.rewrite_opt_expr(&init_ctx, field.init.as_ref())?;
            Ok(f)
        }).collect::<Result<Vec<_>>>()?;
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
        let mut remap = inherited.unwrap_or_default();
        let has_inherited = !remap.is_empty();
        let mut params = Vec::with_capacity(view.params.len());
        let mut static_func_params: Vec<(LocalId, FuncId)> = Vec::new();

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
            if let TyKind::Func(ident) = self.types.kind(param_ty) {
                if let Some(fid) = self.find_func_identity_instance(ident) {
                    static_func_params.push((param.id, fid));
                }
                continue;
            }
            let mut new_param = param.clone();
            if self.is_resolved(param_ty) {
                new_param.ty = self.materialize(param_ty);
            } else if !self.index.is_param_annotated(param.id)
                && !view.is_exported
                && !view.is_obj_method()
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
                remap: None,
            };
            new_param.default = self.rewrite_opt_expr(&param_ctx, param.default.as_ref())?;
            params.push(new_param);
        }

        let ret_ty = self.solved.locations[inst.return_loc];
        let return_type = if self.is_resolved(ret_ty) {
            self.materialize(ret_ty)
        } else {
            view.return_type.clone()
        };
        let mut rctx = RewriteCtx {
            inst_id,
            func_params: &static_func_params,
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

        let mut captures = view.captures.to_vec();
        if let Some(extra) = self.solved.inst_captures.get(inst_id.0 as usize) {
            captures.extend(extra.iter().copied());
        }
        if !remap.is_empty() {
            for cap in &mut captures {
                if let Some(&m) = remap.get(cap) {
                    *cap = m;
                }
            }
        }
        captures.sort_unstable();
        captures.dedup();

        if !view.is_exported && inst.caller.is_some() {
            let caller_name = inst
                .caller
                .and_then(|c| self.solved.instances.get(c))
                .filter(|c| c.template != TemplateId(0))
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
                is_exported: view.is_exported,
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
                (self.solved.live_instances.contains(id.0 as usize)
                    && inst.template == ident.template
                    && inst.capture_owner == ident.capture_owner)
                    .then_some(inst.emitted_func_id)
            })
            .or_else(|| self.index.view(ident.template).map(|v| v.func_id))
    }

    fn rewrite_opt_expr(
        &mut self,
        rctx: &RewriteCtx<'_>,
        expr: Option<&'h Expr>,
    ) -> Result<Option<Expr>> {
        expr.map(|e| self.rewrite_expr(rctx, e)).transpose()
    }
    fn rewrite_stmts(&mut self, rctx: &mut RewriteCtx<'_>, stmts: &'h [Stmt]) -> Result<Vec<Stmt>> {
        stmts.iter().map(|s| self.rewrite_stmt(rctx, s)).collect()
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
                let is_inferred = !self.hir.local_source_spans.contains_key(id)
                    || self
                        .index
                        .local_idx(*id)
                        .is_none_or(|li| self.index.local_inferred.contains(li.0 as usize));
                let is_deferred = self
                    .index
                    .local_idx(*id)
                    .is_some_and(|li| self.index.local_deferred.contains(li.0 as usize));
                if let Some(t) = resolved
                    && let TyKind::Func(ident) = self.types.kind(t)
                {
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
                let new_ty = match resolved {
                    Some(t)
                        if self.is_resolved(t)
                            && (is_inferred
                                || matches!(ty, HirType::Any | HirType::Unknown)
                                || matches!(ty, HirType::Array(e) if matches!(**e, HirType::Any | HirType::Unknown))
                                || matches!(self.types.kind(t), TyKind::ClassInst(_))) =>
                    {
                        if is_inferred
                            && is_deferred
                            && self.types.needs_json_storage(
                                self.ctx,
                                t,
                                self.solved.locations.as_slice(),
                                &self.solved.dict_reads,
                            )
                        {
                            self.json_storage_bindings.push(out_id);
                        }
                        self.materialize(t)
                    }
                    Some(t)
                        if is_inferred
                            && matches!(
                                self.types.kind(t),
                                TyKind::Cell {
                                    kind: CellKind::Array,
                                    ..
                                }
                            ) =>
                    {
                        let err = diag!(codes::MISSING_TYPE_ANNOTATION, "an empty array literal needs a declared element type")
                            .with_help("Add a concrete element type annotation to this binding (e.g. `const xs: number[] = []`).");
                        let err = self.annotate_local(err, *id);
                        return Err(self.attach_inst_chain(err, rctx.inst_id));
                    }
                    _ => ty.clone(),
                };
                Ok(Stmt::Let {
                    id: out_id,
                    name: name.clone(),
                    ty: new_ty,
                    mutable: *mutable,
                    init: self.rewrite_opt_expr(rctx, init.as_ref())?,
                })
            }
            Stmt::Expr(e) => Ok(Stmt::Expr(self.rewrite_expr(rctx, e)?)),
            Stmt::Throw(e) => Ok(Stmt::Throw(self.rewrite_expr(rctx, e)?)),
            Stmt::Return(v) => Ok(Stmt::Return(self.rewrite_opt_expr(rctx, v.as_ref())?)),
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => Ok(Stmt::If {
                condition: self.rewrite_expr(rctx, condition)?,
                then_branch: self.rewrite_stmts(rctx, then_branch)?,
                else_branch: else_branch
                    .as_deref()
                    .map(|b| self.rewrite_stmts(rctx, b))
                    .transpose()?,
            }),
            Stmt::While { condition, body } => Ok(Stmt::While {
                condition: self.rewrite_expr(rctx, condition)?,
                body: self.rewrite_stmts(rctx, body)?,
            }),
            Stmt::DoWhile { condition, body } => Ok(Stmt::DoWhile {
                condition: self.rewrite_expr(rctx, condition)?,
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

    fn rewrite_expr(&mut self, rctx: &RewriteCtx<'_>, expr: &'h Expr) -> Result<Expr> {
        if let Some(site) = self.site_of(rctx.inst_id, expr).cloned() {
            match site {
                SiteResolution::FoldBool(b) => return Ok(Expr::Bool(b)),
                SiteResolution::CollectionNew { is_map, type_args } => {
                    return Ok(Expr::New {
                        class_name: if is_map { "\0perry_Map" } else { "\0perry_Set" }.into(),
                        type_args: type_args.into_iter().map(|t| self.materialize(t)).collect(),
                        args: Vec::new(),
                        byte_offset: 0,
                        byte_end: 0,
                        cap_args_appended: 0,
                    });
                }
                SiteResolution::Callback {
                    target,
                    keep_inline_closure,
                } => {
                    let ti = self.instance(target, "missing callback target instance")?;
                    if !keep_inline_closure {
                        return Ok(Expr::FuncRef(ti.emitted_func_id));
                    }
                    if let Expr::Closure { body, .. } = expr {
                        let mut out = expr.clone();
                        if let Expr::Closure {
                            params,
                            return_type,
                            body: out_body,
                            ..
                        } = &mut out
                        {
                            *return_type = self.materialize(self.solved.locations[ti.return_loc]);
                            for (p, &loc) in params.iter_mut().zip(
                                self.solved
                                    .param_locs
                                    .get(ti.param_locs.range())
                                    .unwrap_or(&[]),
                            ) {
                                let ty = self.solved.locations[loc];
                                if self.is_resolved(ty) {
                                    p.ty = self.materialize(ty);
                                }
                            }
                            *out_body = self.rewrite_stmts(&mut RewriteCtx::plain(target), body)?;
                        }
                        return Ok(out);
                    }
                }
                SiteResolution::DirectCall {
                    target,
                    strip_arg_mask,
                } => {
                    let fid = self
                        .instance(target, "missing direct call target instance")?
                        .emitted_func_id;
                    let mut out = expr.clone();
                    match (expr, &mut out) {
                        (
                            Expr::Call { args, .. },
                            Expr::Call {
                                callee,
                                args: out_args,
                                type_args,
                                ..
                            },
                        ) => {
                            **callee = Expr::FuncRef(fid);
                            *out_args = self.rewrite_filtered_args(rctx, args, strip_arg_mask)?;
                            type_args.clear();
                            return Ok(out);
                        }
                        (
                            Expr::CallSpread { args, .. },
                            Expr::CallSpread {
                                callee,
                                args: out_args,
                                ..
                            },
                        ) => {
                            **callee = Expr::FuncRef(fid);
                            for (a, o) in args.iter().zip(out_args.iter_mut()) {
                                let (
                                    CallArg::Expr(e) | CallArg::Spread(e),
                                    CallArg::Expr(dst) | CallArg::Spread(dst),
                                ) = (a, o);
                                *dst = self.rewrite_expr(rctx, e)?;
                            }
                            return Ok(out);
                        }
                        _ => {}
                    }
                }
                SiteResolution::MethodCall {
                    target,
                    strip_arg_mask,
                } => {
                    let method_name = self
                        .instance(target, "missing method call target instance")?
                        .emitted_method_name
                        .clone();
                    let mut out = expr.clone();
                    match (expr, &mut out) {
                        (
                            Expr::StaticMethodCall { args, .. },
                            Expr::StaticMethodCall {
                                method_name: out_name,
                                args: out_args,
                                ..
                            },
                        ) => {
                            *out_name = method_name;
                            *out_args = self.rewrite_filtered_args(rctx, args, strip_arg_mask)?;
                            return Ok(out);
                        }
                        (
                            Expr::Call { callee, args, .. },
                            Expr::Call {
                                callee: out_callee,
                                args: out_args,
                                type_args,
                                ..
                            },
                        ) if matches!(callee.as_ref(), Expr::PropertyGet { .. }) => {
                            if let (
                                Expr::PropertyGet { object, .. },
                                Expr::PropertyGet {
                                    object: out_obj,
                                    property,
                                    ..
                                },
                            ) = (callee.as_ref(), out_callee.as_mut())
                            {
                                **out_obj = self.rewrite_expr(rctx, object)?;
                                *property = method_name;
                            }
                            *out_args = self.rewrite_filtered_args(rctx, args, strip_arg_mask)?;
                            type_args.clear();
                            return Ok(out);
                        }
                        _ => {}
                    }
                }
                SiteResolution::ClassNew {
                    class_inst,
                    strip_arg_mask,
                } => {
                    if let Expr::New { args, .. } = expr {
                        let name = self
                            .class_inst_name(class_inst)
                            .ok_or_else(|| Diagnostic::internal("missing class instance name"))?;
                        let mut out = expr.clone();
                        if let Expr::New {
                            class_name,
                            type_args,
                            args: out_args,
                            ..
                        } = &mut out
                        {
                            *class_name = name;
                            type_args.clear();
                            *out_args = self.rewrite_filtered_args(rctx, args, strip_arg_mask)?;
                        }
                        return Ok(out);
                    }
                }
            }
        }
        if let Expr::LocalGet(id) = expr
            && let Some((_, fid)) = rctx.func_params.iter().find(|(lid, _)| lid == id)
        {
            return Ok(Expr::FuncRef(*fid));
        }
        let mut cloned = expr.clone();
        let mut orig_children: Vec<&'h Expr> = Vec::new();
        perry_hir::walker::walk_expr_children(expr, &mut |child| orig_children.push(child));
        let (mut idx, mut err) = (0usize, None);
        perry_hir::walker::walk_expr_children_mut(&mut cloned, &mut |child| {
            if err.is_none()
                && let Some(&orig) = orig_children.get(idx)
            {
                idx += 1;
                match self.rewrite_expr(rctx, orig) {
                    Ok(r) => *child = r,
                    Err(e) => err = Some(e),
                }
            }
        });
        match err {
            Some(e) => Err(e),
            None => Ok(cloned),
        }
    }

    fn rewrite_filtered_args(
        &mut self,
        rctx: &RewriteCtx<'_>,
        args: &'h [Expr],
        mask: u64,
    ) -> Result<Vec<Expr>> {
        args.iter()
            .enumerate()
            .filter(|&(i, _)| i >= 64 || (mask & (1u64 << i)) == 0)
            .map(|(_, arg)| self.rewrite_expr(rctx, arg))
            .collect()
    }
}

fn slice_at<T>(lists: &[Vec<T>], idx: usize) -> &[T] {
    lists.get(idx).map_or(&[], Vec::as_slice)
}
