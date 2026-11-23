//! Final monomorphic HIR emission phase.
//!
//! Borrows original templates from `&HirModule`, applies every resolved type and call/class/closure
//! rewrite from [`Solved`], and outputs concrete monomorphic `init`, `functions`, and `classes`.
use super::bitset::BitSet;
use super::boundary::{ModuleIndex, TemplateKind};
use super::solve::{SiteResolution, Solved};
use super::types::{CellKind, ClassInstId, InstanceId, TemplateId, Ty, TyKind, TypeTable};
use crate::hir_compat::{Locations, Metadata};
use perry_hir::{
    ir::{Class, ClassField, Expr, Function, Module as HirModule, Param, Stmt},
    types::{FuncId, LocalId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};
use pliron::context::Context;
use std::collections::HashMap;

/// Owned monomorphic HIR outputs produced from a read-only borrow of the source [`HirModule`].
pub(crate) struct Emitted {
    pub(crate) init: Vec<Stmt>,
    pub(crate) functions: Vec<Function>,
    pub(crate) classes: Vec<Class>,
    pub(crate) remapped_locals: Vec<(LocalId, LocalId)>,
    pub(crate) instantiations: Vec<(String, String, u32, Option<String>)>,
    pub(crate) inline_callbacks: Vec<FuncId>,
    pub(crate) class_origins: Vec<(String, String)>,
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
        for fid in self.inline_callbacks {
            metadata.inline_callbacks.insert(fid);
        }
        for (emitted, origin) in self.class_origins {
            metadata.class_origins.insert(emitted, origin);
        }
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
    let mut emitter = Emitter {
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
    };

    let (init, functions, classes) = emitter.build_all()?;
    Ok(Emitted {
        init,
        functions,
        classes,
        remapped_locals: emitter.remapped_locals,
        instantiations: emitter.instantiations,
        inline_callbacks: emitter.inline_callbacks,
        class_origins: emitter.class_origins,
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
}

impl<'h> Emitter<'h, '_> {
    fn class_inst_name(&self, id: ClassInstId) -> Option<String> {
        self.solved
            .class_insts
            .get(id.0 as usize)
            .map(|c| c.emitted_name.clone())
    }

    fn materialize(&self, ty: Ty) -> HirType {
        self.types.materialize_hir_with(
            self.ctx,
            ty,
            &self.solved.locations,
            &self.solved.dict_reads,
            &|cid| self.class_inst_name(cid),
        )
    }

    fn is_resolved(&self, ty: Ty) -> bool {
        !self
            .types
            .is_open_with(ty, &self.solved.locations, &self.solved.dict_reads)
    }

    fn annotate_local(&self, err: Diagnostic, id: LocalId) -> Diagnostic {
        match self.hir.local_source_spans.get(&id) {
            Some(span) => self.locations.annotate(
                err,
                swc_common::Span::new(
                    swc_common::BytePos(span.start),
                    swc_common::BytePos(span.end),
                ),
            ),
            None => err,
        }
    }

    fn attach_inst_chain(&self, mut err: Diagnostic, mut inst_id: InstanceId) -> Diagnostic {
        let mut depth = 0usize;
        while depth < 16
            && let Some(inst) = self.solved.instances.get(inst_id.0 as usize)
        {
            if inst.template == TemplateId(0) {
                break;
            }
            let Some(caller) = inst.caller else {
                break;
            };
            if inst.call_byte_offset != 0
                && let Some(view) = self.index.view(inst.template)
            {
                let msg = format!("while instantiating '{}'", view.display_name());
                err = match self.locations.call_span(inst.call_byte_offset) {
                    Some(note_span) => err.with_spanned_note(note_span, msg),
                    None => err.with_note(msg),
                };
            }
            inst_id = caller;
            depth += 1;
        }
        err
    }

    fn site_of(&self, inst_id: InstanceId, expr: &Expr) -> Option<&SiteResolution> {
        let template = self.solved.instances.get(inst_id.0 as usize)?.template;
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
                && let Some(inst) = self.solved.instances.get(inst_id.0 as usize)
                && self.index.local_owner.get(lidx.0 as usize) == Some(&inst.template)
            {
                return self
                    .solved
                    .inst_frame_locals
                    .get(inst_id.0 as usize)
                    .and_then(|frame| frame.get(slot))
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
        // 1. Rewrite module init (`InstanceId(0)`).
        let init_inst = InstanceId(0);
        let new_init = self.rewrite_stmts(init_inst, &self.hir.init, None)?;

        // 2. Emit every reached top-level function & lifted closure instance.
        // Only remap locals on a 2nd+ specialization of the same template (so exported and
        // single-instance functions keep original LocalIds and captures match directly).
        let mut seen_templates = BitSet::default();
        let mut inst_remaps: Vec<Option<HashMap<LocalId, LocalId>>> =
            vec![None; self.solved.instances.len()];
        let mut new_functions = Vec::new();
        for inst_i in 1..self.solved.instances.len() {
            if !self.solved.live_instances.contains(inst_i) {
                continue;
            }
            let inst_id = InstanceId(inst_i as u32);
            let Some(inst) = self.solved.instances.get(inst_i) else {
                continue;
            };
            let Some(view) = self.index.view(inst.template) else {
                continue;
            };
            match view.kind {
                TemplateKind::Function { .. } | TemplateKind::Closure { .. } => {
                    let keep_original = view.is_exported
                        || self
                            .index
                            .template_is_obj_method
                            .contains(inst.template.0 as usize);
                    let remap_locals =
                        !keep_original && !seen_templates.insert(inst.template.0 as usize);
                    let inherited = inst
                        .capture_owner
                        .and_then(|owner| inst_remaps.get(owner.0 as usize))
                        .and_then(|opt| opt.as_ref())
                        .cloned();
                    let (func, remap) =
                        self.emit_function(inst_id, inst, view, remap_locals, inherited)?;
                    if !remap.is_empty()
                        && let Some(slot) = inst_remaps.get_mut(inst_i)
                    {
                        *slot = Some(remap);
                    }
                    if inst.is_inline_callback || inst.captures_non_global {
                        self.inline_callbacks.push(func.id);
                    }
                    new_functions.push(func);
                }
                _ => {}
            }
        }

        // Retain unreached generator functions so lowering's exact unsupported diagnostic fires.
        // Also retain unreached concrete top-level functions when no exported functions exist,
        // so WIT export binding can report `MISSING_EXPORT` with the declaration span and help.
        let keep_unexported_candidates = new_functions.is_empty();
        for func in &self.hir.functions {
            if new_functions.iter().any(|f| f.id == func.id) {
                continue;
            }
            let concrete_candidate = keep_unexported_candidates
                && func.type_params.is_empty()
                && func.params.iter().all(|p| !matches!(p.ty, HirType::Any))
                && !matches!(func.return_type, HirType::Any);
            if func.is_generator || concrete_candidate {
                new_functions.push(func.clone());
            }
        }

        // 3. Emit classes: keep `__AnonShape_*` object-literal classes as-is, emit concrete
        // monomorphic classes for every instantiated user class, and also emit static-only
        // classes that had static methods or fields reached without `new`.
        let mut new_classes: Vec<Class> = self
            .hir
            .classes
            .iter()
            .enumerate()
            .filter(|&(i, _)| self.index.hir_class_is_literal.contains(i))
            .map(|(_, c)| c.clone())
            .collect();
        let mut emitted_class_ids = BitSet::default();
        for cinst_i in 0..self.solved.class_insts.len() {
            let Some(cinst) = self.solved.class_insts.get(cinst_i) else {
                continue;
            };
            emitted_class_ids.insert(cinst.class.0 as usize);
            let Some(orig) = self
                .index
                .class_hir_idx
                .get(cinst.class.0 as usize)
                .and_then(|&i| self.hir.classes.get(i))
            else {
                continue;
            };
            if cinst.emitted_name != orig.name {
                self.class_origins
                    .push((cinst.emitted_name.clone(), orig.name.clone()));
            }
            new_classes.push(self.emit_instantiated_class(init_inst, cinst_i, cinst, orig)?);
        }
        for (cid_i, &hir_idx) in self.index.class_hir_idx.iter().enumerate() {
            if emitted_class_ids.contains(cid_i) {
                continue;
            }
            let Some(orig) = self.hir.classes.get(hir_idx) else {
                continue;
            };
            let static_methods = self.emit_named_methods(
                self.solved
                    .class_static_methods
                    .get(cid_i)
                    .map_or(&[], Vec::as_slice),
            )?;
            if static_methods.is_empty()
                && orig.static_fields.is_empty()
                && orig.extends_name.is_none()
            {
                continue;
            }
            let mut cls = orig.clone();
            cls.type_params.clear();
            cls.constructor = None;
            cls.methods.clear();
            cls.getters.clear();
            cls.setters.clear();
            cls.static_fields = self.emit_static_fields(init_inst, cid_i, orig)?;
            cls.static_methods = static_methods;
            new_classes.push(cls);
        }

        Ok((new_init, new_functions, new_classes))
    }

    fn emit_instantiated_class(
        &mut self,
        init_inst: InstanceId,
        cinst_i: usize,
        cinst: &super::solve::ClassInst,
        orig: &'h Class,
    ) -> Result<Class> {
        let mut fields = Vec::with_capacity(orig.fields.len());
        for (f_idx, field) in orig.fields.iter().enumerate() {
            let resolved = self.solved.locations[(cinst.field_loc_base + f_idx as u32) as usize];
            let mut f = field.clone();
            if self.is_resolved(resolved) {
                f.ty = self.materialize(resolved);
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
                f.init = Some(self.rewrite_expr(init_inst, init, None)?);
            }
            fields.push(f);
        }
        let constructor = match cinst.ctor_inst {
            Some(ctor_id)
                if let Some(inst) = self.solved.instances.get(ctor_id.0 as usize)
                    && let Some(view) = self.index.view(inst.template) =>
            {
                Some(self.emit_function(ctor_id, inst, view, false, None)?.0)
            }
            _ => orig.constructor.clone(),
        };
        let mut cls = orig.clone();
        cls.name = cinst.emitted_name.clone();
        cls.type_params.clear();
        cls.fields = fields;
        cls.static_fields = self.emit_static_fields(init_inst, cinst.class.0 as usize, orig)?;
        cls.constructor = constructor;
        cls.methods = self.emit_named_methods(
            self.solved
                .class_inst_methods
                .get(cinst_i)
                .map_or(&[], Vec::as_slice),
        )?;
        cls.static_methods = self.emit_named_methods(
            self.solved
                .class_static_methods
                .get(cinst.class.0 as usize)
                .map_or(&[], Vec::as_slice),
        )?;
        cls.getters = self.emit_accessors(
            self.solved
                .class_inst_getters
                .get(cinst_i)
                .map_or(&[], Vec::as_slice),
            &orig.getters,
        )?;
        cls.setters = self.emit_accessors(
            self.solved
                .class_inst_setters
                .get(cinst_i)
                .map_or(&[], Vec::as_slice),
            &orig.setters,
        )?;
        Ok(cls)
    }

    fn emit_static_fields(
        &mut self,
        init_inst: InstanceId,
        cid_i: usize,
        orig: &'h Class,
    ) -> Result<Vec<ClassField>> {
        let mut out = Vec::with_capacity(orig.static_fields.len());
        let static_tys = self.solved.class_static_field_tys.get(cid_i);
        for (f_idx, field) in orig.static_fields.iter().enumerate() {
            let mut f = field.clone();
            if let Some(&ty) = static_tys.and_then(|tys| tys.get(f_idx))
                && self.is_resolved(ty)
            {
                f.ty = self.materialize(ty);
            }
            if let Some(init) = &field.init {
                f.init = Some(self.rewrite_expr(init_inst, init, None)?);
            }
            out.push(f);
        }
        Ok(out)
    }

    fn emit_named_methods(&mut self, ids: &[InstanceId]) -> Result<Vec<Function>> {
        let mut out = Vec::with_capacity(ids.len());
        for &inst_id in ids {
            let inst = &self.solved.instances[inst_id.0 as usize];
            let Some(view) = self.index.view(inst.template) else {
                continue;
            };
            let (mut m, _) = self.emit_function(inst_id, inst, view, false, None)?;
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
        for &(acc_idx, inst_id) in ids {
            let inst = &self.solved.instances[inst_id.0 as usize];
            let Some(view) = self.index.view(inst.template) else {
                continue;
            };
            let (f, _) = self.emit_function(inst_id, inst, view, false, None)?;
            let prop = orig
                .get(acc_idx)
                .map_or(view.name, |(n, _)| n.as_str())
                .to_owned();
            out.push((prop, f));
        }
        Ok(out)
    }

    fn emit_function(
        &mut self,
        inst_id: InstanceId,
        inst: &super::solve::Instance,
        view: &super::boundary::TemplateView<'h>,
        remap_locals: bool,
        inherited_remap: Option<HashMap<LocalId, LocalId>>,
    ) -> Result<(Function, HashMap<LocalId, LocalId>)> {
        let mut remap = inherited_remap.unwrap_or_default();
        let has_inherited = !remap.is_empty();
        let mut params = Vec::with_capacity(view.params.len());
        let mut static_func_params: Vec<(LocalId, FuncId)> = Vec::new();

        for (p_idx, param) in view.params.iter().enumerate() {
            let param_loc = *self
                .solved
                .param_locs
                .get(inst.param_locs.range())
                .and_then(|slice| slice.get(p_idx))
                .ok_or_else(|| {
                    Diagnostic::internal("emitted function missing parameter location slot")
                })?;
            let param_ty = *self
                .solved
                .locations
                .get(param_loc.0 as usize)
                .ok_or_else(|| Diagnostic::internal("parameter location out of range"))?;
            if let TyKind::Func(ident) = self.types.kind(param_ty) {
                // Static function parameter: bind to its specialized instance and drop from runtime params.
                if let Some(target_inst) = self.find_func_identity_instance(ident) {
                    static_func_params.push((param.id, target_inst));
                }
                continue;
            }
            let mut new_param: Param = param.clone();
            if self.is_resolved(param_ty) {
                new_param.ty = self.materialize(param_ty);
            } else {
                let annotated = self
                    .index
                    .local_idx(param.id)
                    .is_some_and(|li| self.index.local_param_annotated.contains(li.0 as usize));
                if !annotated
                    && !view.is_exported
                    && !self
                        .index
                        .template_is_obj_method
                        .contains(inst.template.0 as usize)
                    && matches!(param.ty, HirType::Any | HirType::Unknown)
                {
                    let err = diag!(
                        codes::MISSING_TYPE_ANNOTATION,
                        "cannot infer the type of parameter '{}' of '{}'",
                        param.name,
                        view.display_name()
                    )
                    .with_help(
                        "Add a concrete type annotation to this binding, parameter, or function return.",
                    );
                    let err = self.annotate_local(err, param.id);
                    return Err(self.attach_inst_chain(err, inst_id));
                }
            }
            if remap_locals {
                new_param.id = self.fresh_local(param.id, &mut remap)?;
            }
            if let Some(default) = &param.default {
                new_param.default =
                    Some(self.rewrite_expr(inst_id, default, Some(&static_func_params))?);
            }
            params.push(new_param);
        }

        let ret_ty = self.solved.locations[inst.return_loc.0 as usize];
        let return_type = if self.is_resolved(ret_ty) {
            self.materialize(ret_ty)
        } else {
            view.return_type.clone()
        };

        let mut body = self.rewrite_stmts(inst_id, view.body, Some(&static_func_params))?;
        if remap_locals {
            self.remap_declared_locals(&mut body, &mut remap)?;
        }
        if remap_locals || has_inherited {
            for param in &mut params {
                if let Some(default) = &mut param.default {
                    perry_hir::analysis::remap_local_ids_in_expr(default, &remap);
                }
            }
            perry_hir::analysis::remap_local_ids_in_stmts(&mut body, &remap);
        }

        let mut captures: Vec<LocalId> = view.captures.to_vec();
        if let Some(extra_caps) = self.solved.inst_captures.get(inst_id.0 as usize) {
            for &cap in extra_caps {
                if !captures.contains(&cap) {
                    captures.push(cap);
                }
            }
        }
        if !remap.is_empty() {
            for cap in &mut captures {
                if let Some(&mapped) = remap.get(cap) {
                    *cap = mapped;
                }
            }
        }
        captures.sort_unstable();
        captures.dedup();

        if !view.is_exported && inst.caller.is_some() {
            let caller_name = inst
                .caller
                .and_then(|c| self.solved.instances.get(c.0 as usize))
                .filter(|c| c.template != TemplateId(0))
                .map(|c| c.emitted_name.clone());
            let template_display = if view.name.is_empty() {
                format!("callback_{}", view.func_id)
            } else {
                view.name.to_owned()
            };
            self.instantiations.push((
                inst.emitted_name.clone(),
                template_display,
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

    fn find_func_identity_instance(&self, ident: super::types::FuncIdentity) -> Option<FuncId> {
        self.solved
            .template_instances
            .get(ident.template.0 as usize)
            .and_then(|ids| {
                ids.iter().find_map(|&inst_id| {
                    let inst = self.solved.instances.get(inst_id.0 as usize)?;
                    (inst.capture_owner == ident.capture_owner).then_some(inst.emitted_func_id)
                })
            })
            .or_else(|| self.index.view(ident.template).map(|v| v.func_id))
    }

    fn remap_declared_locals(
        &mut self,
        stmts: &mut [Stmt],
        remap: &mut HashMap<LocalId, LocalId>,
    ) -> Result<()> {
        for stmt in stmts {
            match stmt {
                Stmt::Let { id, .. } => {
                    *id = self.fresh_local(*id, remap)?;
                }
                Stmt::If {
                    then_branch,
                    else_branch,
                    ..
                } => {
                    self.remap_declared_locals(then_branch, remap)?;
                    if let Some(else_branch) = else_branch {
                        self.remap_declared_locals(else_branch, remap)?;
                    }
                }
                Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
                    self.remap_declared_locals(body, remap)?;
                }
                Stmt::For { init, body, .. } => {
                    if let Some(init) = init {
                        self.remap_declared_locals(std::slice::from_mut(init), remap)?;
                    }
                    self.remap_declared_locals(body, remap)?;
                }
                Stmt::Try {
                    body,
                    catch,
                    finally,
                    ..
                } => {
                    self.remap_declared_locals(body, remap)?;
                    if let Some(catch) = catch {
                        if let Some((id, _)) = &mut catch.param {
                            *id = self.fresh_local(*id, remap)?;
                        }
                        self.remap_declared_locals(&mut catch.body, remap)?;
                    }
                    if let Some(finally) = finally {
                        self.remap_declared_locals(finally, remap)?;
                    }
                }
                Stmt::Switch { cases, .. } => {
                    for case in cases {
                        self.remap_declared_locals(&mut case.body, remap)?;
                    }
                }
                Stmt::Labeled { body, .. } => {
                    self.remap_declared_locals(std::slice::from_mut(body), remap)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn rewrite_stmts(
        &mut self,
        inst_id: InstanceId,
        stmts: &'h [Stmt],
        func_params: Option<&[(LocalId, FuncId)]>,
    ) -> Result<Vec<Stmt>> {
        let mut out = Vec::with_capacity(stmts.len());
        for stmt in stmts {
            out.push(self.rewrite_stmt(inst_id, stmt, func_params)?);
        }
        Ok(out)
    }

    fn rewrite_stmt(
        &mut self,
        inst_id: InstanceId,
        stmt: &'h Stmt,
        func_params: Option<&[(LocalId, FuncId)]>,
    ) -> Result<Stmt> {
        match stmt {
            Stmt::Let {
                id,
                name,
                ty,
                mutable,
                init,
            } => {
                let resolved = self.local_ty(inst_id, *id);
                let is_inferred = !self.hir.local_source_spans.contains_key(id)
                    || self
                        .index
                        .local_idx(*id)
                        .is_none_or(|lidx| self.index.local_inferred.contains(lidx.0 as usize));
                // Rewrite static function-alias initializers (`const again = verify`) to
                // `Expr::FuncRef(...)` so lowering skips slot allocation instead of reading
                // a non-slot lexical function local.
                if let Some(t) = resolved
                    && let TyKind::Func(ident) = self.types.kind(t)
                {
                    let target_fid = self.find_func_identity_instance(ident).ok_or_else(|| {
                        Diagnostic::internal(format!(
                            "unresolved function identity for binding '{name}'"
                        ))
                    })?;
                    return Ok(Stmt::Let {
                        id: *id,
                        name: name.clone(),
                        ty: ty.clone(),
                        mutable: *mutable,
                        init: Some(Expr::FuncRef(target_fid)),
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
                        let err = diag!(
                            codes::MISSING_TYPE_ANNOTATION,
                            "an empty array literal needs a declared element type"
                        )
                        .with_help(
                            "Add a concrete element type annotation to this binding (e.g. `const xs: number[] = []`).",
                        );
                        let err = self.annotate_local(err, *id);
                        return Err(self.attach_inst_chain(err, inst_id));
                    }
                    _ => ty.clone(),
                };
                Ok(Stmt::Let {
                    id: *id,
                    name: name.clone(),
                    ty: new_ty,
                    mutable: *mutable,
                    init: init
                        .as_ref()
                        .map(|e| self.rewrite_expr(inst_id, e, func_params))
                        .transpose()?,
                })
            }
            Stmt::Expr(e) => Ok(Stmt::Expr(self.rewrite_expr(inst_id, e, func_params)?)),
            Stmt::Throw(e) => Ok(Stmt::Throw(self.rewrite_expr(inst_id, e, func_params)?)),
            Stmt::Return(v) => Ok(Stmt::Return(
                v.as_ref()
                    .map(|e| self.rewrite_expr(inst_id, e, func_params))
                    .transpose()?,
            )),
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => Ok(Stmt::If {
                condition: self.rewrite_expr(inst_id, condition, func_params)?,
                then_branch: self.rewrite_stmts(inst_id, then_branch, func_params)?,
                else_branch: else_branch
                    .as_deref()
                    .map(|b| self.rewrite_stmts(inst_id, b, func_params))
                    .transpose()?,
            }),
            Stmt::While { condition, body } => Ok(Stmt::While {
                condition: self.rewrite_expr(inst_id, condition, func_params)?,
                body: self.rewrite_stmts(inst_id, body, func_params)?,
            }),
            Stmt::DoWhile { condition, body } => Ok(Stmt::DoWhile {
                condition: self.rewrite_expr(inst_id, condition, func_params)?,
                body: self.rewrite_stmts(inst_id, body, func_params)?,
            }),
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => Ok(Stmt::For {
                init: init
                    .as_deref()
                    .map(|s| self.rewrite_stmt(inst_id, s, func_params).map(Box::new))
                    .transpose()?,
                condition: condition
                    .as_ref()
                    .map(|e| self.rewrite_expr(inst_id, e, func_params))
                    .transpose()?,
                update: update
                    .as_ref()
                    .map(|e| self.rewrite_expr(inst_id, e, func_params))
                    .transpose()?,
                body: self.rewrite_stmts(inst_id, body, func_params)?,
            }),
            Stmt::Try {
                body,
                catch,
                finally,
                forof_completion,
            } => Ok(Stmt::Try {
                body: self.rewrite_stmts(inst_id, body, func_params)?,
                catch: catch
                    .as_ref()
                    .map(|c| {
                        Ok::<_, Diagnostic>(perry_hir::ir::CatchClause {
                            param: c.param.clone(),
                            body: self.rewrite_stmts(inst_id, &c.body, func_params)?,
                        })
                    })
                    .transpose()?,
                finally: finally
                    .as_deref()
                    .map(|f| self.rewrite_stmts(inst_id, f, func_params))
                    .transpose()?,
                forof_completion: *forof_completion,
            }),
            Stmt::Switch {
                discriminant,
                cases,
            } => Ok(Stmt::Switch {
                discriminant: self.rewrite_expr(inst_id, discriminant, func_params)?,
                cases: cases
                    .iter()
                    .map(|c| {
                        Ok(perry_hir::ir::SwitchCase {
                            test: c
                                .test
                                .as_ref()
                                .map(|t| self.rewrite_expr(inst_id, t, func_params))
                                .transpose()?,
                            body: self.rewrite_stmts(inst_id, &c.body, func_params)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            }),
            Stmt::Labeled { label, body } => Ok(Stmt::Labeled {
                label: label.clone(),
                body: Box::new(self.rewrite_stmt(inst_id, body, func_params)?),
            }),
            other => Ok(other.clone()),
        }
    }

    fn rewrite_expr(
        &mut self,
        inst_id: InstanceId,
        expr: &'h Expr,
        func_params: Option<&[(LocalId, FuncId)]>,
    ) -> Result<Expr> {
        if let Some(site) = self.site_of(inst_id, expr).cloned() {
            match site {
                SiteResolution::FoldBool(b) => return Ok(Expr::Bool(b)),
                SiteResolution::Callback {
                    target,
                    keep_inline_closure,
                } => {
                    let target_inst = self
                        .solved
                        .instances
                        .get(target.0 as usize)
                        .ok_or_else(|| Diagnostic::internal("missing callback target instance"))?;
                    if !keep_inline_closure {
                        return Ok(Expr::FuncRef(target_inst.emitted_func_id));
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
                            *return_type = self.materialize(
                                self.solved.locations[target_inst.return_loc.0 as usize],
                            );
                            let plocs = self
                                .solved
                                .param_locs
                                .get(target_inst.param_locs.range())
                                .unwrap_or(&[]);
                            for (p, &loc) in params.iter_mut().zip(plocs) {
                                let ty = self.solved.locations[loc.0 as usize];
                                if self.is_resolved(ty) {
                                    p.ty = self.materialize(ty);
                                }
                            }
                            *out_body = self.rewrite_stmts(target, body, None)?;
                        }
                        return Ok(out);
                    }
                }
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
                SiteResolution::DirectCall {
                    target,
                    strip_arg_mask,
                } => {
                    let fid = self
                        .solved
                        .instances
                        .get(target.0 as usize)
                        .ok_or_else(|| Diagnostic::internal("missing direct call target instance"))?
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
                            *out_args = self.rewrite_filtered_args(
                                inst_id,
                                args,
                                strip_arg_mask,
                                func_params,
                            )?;
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
                            for (orig_a, out_a) in args.iter().zip(out_args.iter_mut()) {
                                match (orig_a, out_a) {
                                    (
                                        perry_hir::ir::CallArg::Expr(e),
                                        perry_hir::ir::CallArg::Expr(o),
                                    )
                                    | (
                                        perry_hir::ir::CallArg::Spread(e),
                                        perry_hir::ir::CallArg::Spread(o),
                                    ) => *o = self.rewrite_expr(inst_id, e, func_params)?,
                                    _ => {}
                                }
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
                        .solved
                        .instances
                        .get(target.0 as usize)
                        .ok_or_else(|| Diagnostic::internal("missing method call target instance"))?
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
                            *out_args = self.rewrite_filtered_args(
                                inst_id,
                                args,
                                strip_arg_mask,
                                func_params,
                            )?;
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
                                **out_obj = self.rewrite_expr(inst_id, object, func_params)?;
                                *property = method_name;
                            }
                            *out_args = self.rewrite_filtered_args(
                                inst_id,
                                args,
                                strip_arg_mask,
                                func_params,
                            )?;
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
                            *out_args = self.rewrite_filtered_args(
                                inst_id,
                                args,
                                strip_arg_mask,
                                func_params,
                            )?;
                        }
                        return Ok(out);
                    }
                }
            }
        }

        if let Expr::LocalGet(id) = expr
            && let Some(fps) = func_params
            && let Some((_, target_fid)) = fps.iter().find(|(lid, _)| lid == id)
        {
            return Ok(Expr::FuncRef(*target_fid));
        }

        // Default recursive structural rewrite of child expressions.
        let mut cloned = expr.clone();
        self.rewrite_expr_children_in_place(inst_id, expr, &mut cloned, func_params)?;
        Ok(cloned)
    }

    fn rewrite_filtered_args(
        &mut self,
        inst_id: InstanceId,
        args: &'h [Expr],
        strip_arg_mask: u64,
        func_params: Option<&[(LocalId, FuncId)]>,
    ) -> Result<Vec<Expr>> {
        let mut out = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            if i < 64 && (strip_arg_mask & (1u64 << i)) != 0 {
                continue;
            }
            out.push(self.rewrite_expr(inst_id, arg, func_params)?);
        }
        Ok(out)
    }

    fn rewrite_expr_children_in_place(
        &mut self,
        inst_id: InstanceId,
        orig: &'h Expr,
        out: &mut Expr,
        func_params: Option<&[(LocalId, FuncId)]>,
    ) -> Result<()> {
        let mut orig_children: Vec<&'h Expr> = Vec::new();
        perry_hir::walker::walk_expr_children(orig, &mut |child| orig_children.push(child));
        let mut idx = 0usize;
        let mut err = None;
        perry_hir::walker::walk_expr_children_mut(out, &mut |child| {
            if err.is_none()
                && let Some(&orig_child) = orig_children.get(idx)
            {
                idx += 1;
                match self.rewrite_expr(inst_id, orig_child, func_params) {
                    Ok(rewritten) => *child = rewritten,
                    Err(e) => err = Some(e),
                }
            }
        });
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}
