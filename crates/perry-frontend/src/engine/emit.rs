//! Final monomorphic HIR emission phase.
//!
//! Borrows original templates from `&HirModule`, applies every resolved type and call/class/closure
//! rewrite from [`Solved`], and outputs concrete monomorphic `init`, `functions`, and `classes`.
use super::bitset::BitSet;
use super::boundary::{ModuleIndex, TemplateKind};
use super::solve::{SiteResolution, Solved};
use super::types::{ClassId, ClassInstId, InstanceId, LocId, TemplateId, Ty, TyKind, TypeTable};
use crate::hir_compat::{self, Metadata};
use perry_hir::{
    ir::{Class, ClassField, Expr, Function, Module as HirModule, Param, Stmt},
    types::{FuncId, LocalId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result};
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
    types: &TypeTable,
    solved: &Solved,
) -> Result<Emitted> {
    let mut emitter = Emitter {
        ctx,
        hir,
        index,
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
        self.types
            .materialize_hir(self.ctx, ty, &|cid| self.class_inst_name(cid))
    }

    /// True when a deferred inferred union cannot be lowered as a finite value union (e.g.
    /// `number[] | string`) yet every member supports dynamic JSON boxing (excluding native
    /// byte arrays like `Uint8Array`, class instances, and opaque handles).
    fn needs_json_union_box(&self, ty: Ty) -> bool {
        let TyKind::Union(span) = self.types.kind(ty) else {
            return false;
        };
        let members = self.types.union_slice(span);
        let mut has_non_value_union_member = false;
        let mut object_alternatives = 0u32;
        for &member in members {
            match self.types.kind(member) {
                TyKind::Null
                | TyKind::Void
                | TyKind::Number
                | TyKind::Int32
                | TyKind::String
                | TyKind::Boolean => {}
                TyKind::Object { .. } => {
                    object_alternatives += 1;
                }
                TyKind::Array(elem) => {
                    if !self.is_json_boxable(elem) {
                        return false;
                    }
                    has_non_value_union_member = true;
                }
                TyKind::Tuple(tspan) => {
                    if !self
                        .types
                        .tuple_slice(tspan)
                        .iter()
                        .all(|&t| self.is_json_boxable(t))
                    {
                        return false;
                    }
                    has_non_value_union_member = true;
                }
                _ => return false,
            }
        }
        has_non_value_union_member || object_alternatives > 1
    }

    fn is_json_boxable(&self, ty: Ty) -> bool {
        match self.types.kind(ty) {
            TyKind::Null
            | TyKind::Void
            | TyKind::Number
            | TyKind::Int32
            | TyKind::String
            | TyKind::Boolean
            | TyKind::Dynamic => true,
            TyKind::Array(elem) => self.is_json_boxable(elem),
            TyKind::Tuple(span) => self
                .types
                .tuple_slice(span)
                .iter()
                .all(|&t| self.is_json_boxable(t)),
            TyKind::Object { props, index } => {
                index.is_none_or(|t| self.is_json_boxable(t))
                    && self
                        .types
                        .object_slice(props)
                        .iter()
                        .all(|p| self.is_json_boxable(p.ty))
            }
            TyKind::Union(span) => self
                .types
                .union_slice(span)
                .iter()
                .all(|&t| self.is_json_boxable(t)),
            _ => false,
        }
    }

    fn site_of(&self, inst_id: InstanceId, expr: &Expr) -> Option<&SiteResolution> {
        let addr = std::ptr::from_ref(expr) as usize;
        let idx = self
            .solved
            .sites
            .binary_search_by_key(&(inst_id.0, addr), |(i, a, _)| (i.0, *a))
            .ok()?;
        self.solved.sites.get(idx).map(|(_, _, res)| res)
    }

    fn local_ty(&self, inst_id: InstanceId, id: LocalId) -> Option<Ty> {
        if let Some((_, ty)) = self
            .solved
            .module_bindings
            .iter()
            .find(|(lid, _)| *lid == id)
        {
            return Some(*ty);
        }
        self.solved
            .instance_locals
            .iter()
            .find(|(i, lid, _)| *i == inst_id && *lid == id)
            .map(|(_, _, ty)| *ty)
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
        let mut inst_remaps: Vec<HashMap<LocalId, LocalId>> =
            vec![HashMap::new(); self.solved.instances.len()];
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
                    let keep_original = view.is_exported || view.name.starts_with("__obj_method_");
                    let remap_locals =
                        !keep_original && !seen_templates.insert(inst.template.0 as usize);
                    let inherited = inst
                        .capture_owner
                        .and_then(|owner| inst_remaps.get(owner.0 as usize))
                        .cloned();
                    let (func, remap) =
                        self.emit_function(inst_id, inst, view, remap_locals, inherited)?;
                    if let Some(slot) = inst_remaps.get_mut(inst_i) {
                        *slot = remap;
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
        let mut new_classes = Vec::with_capacity(self.hir.classes.len());
        for class in &self.hir.classes {
            if hir_compat::is_object_literal(&class.name) {
                new_classes.push(class.clone());
            }
        }
        let mut emitted_class_ids = BitSet::default();
        for cinst_i in 0..self.solved.class_insts.len() {
            let cinst_id = ClassInstId(cinst_i as u32);
            let Some(cinst) = self.solved.class_insts.get(cinst_i) else {
                continue;
            };
            emitted_class_ids.insert(cinst.class.0 as usize);
            let Some(&hir_idx) = self.index.class_hir_idx.get(cinst.class.0 as usize) else {
                continue;
            };
            let Some(orig) = self.hir.classes.get(hir_idx) else {
                continue;
            };
            if cinst.emitted_name != orig.name {
                self.class_origins
                    .push((cinst.emitted_name.clone(), orig.name.clone()));
            }
            let mut fields = Vec::with_capacity(orig.fields.len());
            for (f_idx, field) in orig.fields.iter().enumerate() {
                let loc = LocId(cinst.field_loc_base + f_idx as u32);
                let resolved = self
                    .solved
                    .locations
                    .get(loc.0 as usize)
                    .copied()
                    .unwrap_or(Ty::OPEN);
                let mut f: ClassField = field.clone();
                if !self.types.is_open(resolved) {
                    f.ty = self.materialize(resolved);
                }
                if let Some(init) = &field.init {
                    f.init = Some(self.rewrite_expr(init_inst, init, None)?);
                }
                fields.push(f);
            }
            let mut static_fields = Vec::with_capacity(orig.static_fields.len());
            for (f_idx, field) in orig.static_fields.iter().enumerate() {
                let mut f = field.clone();
                if let Some((_, _, ty)) = self
                    .solved
                    .static_fields
                    .iter()
                    .find(|(c, i, _)| *c == cinst.class && *i == f_idx)
                    && !self.types.is_open(*ty)
                {
                    f.ty = self.materialize(*ty);
                }
                if let Some(init) = &field.init {
                    f.init = Some(self.rewrite_expr(init_inst, init, None)?);
                }
                static_fields.push(f);
            }

            let constructor = match cinst.ctor_inst {
                Some(ctor_id) => {
                    let inst = self.solved.instances.get(ctor_id.0 as usize);
                    let view = inst.and_then(|i| self.index.view(i.template));
                    match (inst, view) {
                        (Some(inst), Some(view)) => {
                            let (f, _) = self.emit_function(ctor_id, inst, view, false, None)?;
                            Some(f)
                        }
                        _ => orig.constructor.clone(),
                    }
                }
                None => orig.constructor.clone(),
            };

            let mut methods = Vec::new();
            let mut static_methods = Vec::new();
            let mut getters = Vec::new();
            let mut setters = Vec::new();
            for inst_i in 1..self.solved.instances.len() {
                let inst_id = InstanceId(inst_i as u32);
                let Some(inst) = self.solved.instances.get(inst_i) else {
                    continue;
                };
                let Some(view) = self.index.view(inst.template) else {
                    continue;
                };
                match view.kind {
                    TemplateKind::ClassMethod { class, .. }
                        if class == cinst.class && inst.receiver == Some(cinst_id) =>
                    {
                        let (mut m, _) = self.emit_function(inst_id, inst, view, false, None)?;
                        m.name = inst.emitted_method_name.clone();
                        methods.push(m);
                    }
                    TemplateKind::ClassStaticMethod { class, .. } if class == cinst.class => {
                        let (mut m, _) = self.emit_function(inst_id, inst, view, false, None)?;
                        m.name = inst.emitted_method_name.clone();
                        static_methods.push(m);
                    }
                    TemplateKind::ClassGetter {
                        class, getter_idx, ..
                    } if class == cinst.class && inst.receiver == Some(cinst_id) => {
                        let (g, _) = self.emit_function(inst_id, inst, view, false, None)?;
                        let prop = orig
                            .getters
                            .get(getter_idx)
                            .map_or(view.name, |(n, _)| n.as_str())
                            .to_owned();
                        getters.push((prop, g));
                    }
                    TemplateKind::ClassSetter {
                        class, setter_idx, ..
                    } if class == cinst.class && inst.receiver == Some(cinst_id) => {
                        let (s, _) = self.emit_function(inst_id, inst, view, false, None)?;
                        let prop = orig
                            .setters
                            .get(setter_idx)
                            .map_or(view.name, |(n, _)| n.as_str())
                            .to_owned();
                        setters.push((prop, s));
                    }
                    _ => {}
                }
            }

            let mut emitted_class = orig.clone();
            emitted_class.name = cinst.emitted_name.clone();
            emitted_class.type_params.clear();
            emitted_class.fields = fields;
            emitted_class.static_fields = static_fields;
            emitted_class.constructor = constructor;
            emitted_class.methods = methods;
            emitted_class.static_methods = static_methods;
            emitted_class.getters = getters;
            emitted_class.setters = setters;
            new_classes.push(emitted_class);
        }

        // Emit classes that were never constructed with `new` but have reached static methods/fields.
        for (cid_i, &hir_idx) in self.index.class_hir_idx.iter().enumerate() {
            if emitted_class_ids.contains(cid_i) {
                continue;
            }
            let cid = ClassId(cid_i as u32);
            let Some(orig) = self.hir.classes.get(hir_idx) else {
                continue;
            };
            let mut static_methods = Vec::new();
            for inst_i in 1..self.solved.instances.len() {
                let inst_id = InstanceId(inst_i as u32);
                let Some(inst) = self.solved.instances.get(inst_i) else {
                    continue;
                };
                let Some(view) = self.index.view(inst.template) else {
                    continue;
                };
                if let TemplateKind::ClassStaticMethod { class, .. } = view.kind
                    && class == cid
                {
                    let (mut m, _) = self.emit_function(inst_id, inst, view, false, None)?;
                    m.name = inst.emitted_method_name.clone();
                    static_methods.push(m);
                }
            }
            if static_methods.is_empty()
                && orig.static_fields.is_empty()
                && orig.extends_name.is_none()
            {
                continue;
            }
            let mut static_fields = Vec::with_capacity(orig.static_fields.len());
            for (f_idx, field) in orig.static_fields.iter().enumerate() {
                let mut f = field.clone();
                if let Some((_, _, ty)) = self
                    .solved
                    .static_fields
                    .iter()
                    .find(|(c, i, _)| *c == cid && *i == f_idx)
                    && !self.types.is_open(*ty)
                {
                    f.ty = self.materialize(*ty);
                }
                if let Some(init) = &field.init {
                    f.init = Some(self.rewrite_expr(init_inst, init, None)?);
                }
                static_fields.push(f);
            }
            let mut emitted_class = orig.clone();
            emitted_class.type_params.clear();
            emitted_class.constructor = None;
            emitted_class.methods.clear();
            emitted_class.getters.clear();
            emitted_class.setters.clear();
            emitted_class.static_fields = static_fields;
            emitted_class.static_methods = static_methods;
            new_classes.push(emitted_class);
        }

        Ok((new_init, new_functions, new_classes))
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
            let param_ty = self
                .solved
                .param_locs
                .get(inst.param_locs.range())
                .and_then(|slice| slice.get(p_idx))
                .and_then(|loc| self.solved.locations.get(loc.0 as usize).copied())
                .unwrap_or(Ty::OPEN);
            if let TyKind::Func(ident) = self.types.kind(param_ty) {
                // Static function parameter: bind to its specialized instance and drop from runtime params.
                if let Some(target_inst) = self.find_func_identity_instance(ident) {
                    static_func_params.push((param.id, target_inst));
                }
                continue;
            }
            let mut new_param: Param = param.clone();
            if !self.types.is_open(param_ty) {
                new_param.ty = self.materialize(param_ty);
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

        let ret_ty = self
            .solved
            .locations
            .get(inst.return_loc.0 as usize)
            .copied()
            .unwrap_or(Ty::VOID);
        let return_type = if !self.types.is_open(ret_ty) {
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
        for (i, cap) in &self.solved.instance_captures {
            if *i == inst_id && !captures.contains(cap) {
                captures.push(*cap);
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
            self.instantiations.push((
                inst.emitted_name.clone(),
                view.name.to_owned(),
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
            .instances
            .iter()
            .enumerate()
            .find(|(idx, i)| {
                self.solved.live_instances.contains(*idx)
                    && i.template == ident.template
                    && i.capture_owner == ident.capture_owner
            })
            .map(|(_, i)| i.emitted_func_id)
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
                    let target_fid = self
                        .find_func_identity_instance(ident)
                        .or_else(|| self.index.view(ident.template).map(|v| v.func_id))
                        .unwrap_or(0);
                    return Ok(Stmt::Let {
                        id: *id,
                        name: name.clone(),
                        ty: ty.clone(),
                        mutable: *mutable,
                        init: Some(Expr::FuncRef(target_fid)),
                    });
                }
                let is_deferred = self
                    .index
                    .local_idx(*id)
                    .is_some_and(|lidx| self.index.local_deferred.contains(lidx.0 as usize));
                let new_ty = match resolved {
                    Some(t)
                        if !self.types.is_open(t)
                            && (is_inferred
                                || matches!(ty, HirType::Any | HirType::Unknown)
                                || matches!(ty, HirType::Array(e) if matches!(**e, HirType::Any | HirType::Unknown))
                                || matches!(self.types.kind(t), TyKind::ClassInst(_))) =>
                    {
                        if is_inferred && is_deferred && self.needs_json_union_box(t) {
                            HirType::Unknown
                        } else {
                            self.materialize(t)
                        }
                    }
                    _ => ty.clone(),
                };
                let new_init = match init {
                    Some(e) => Some(self.rewrite_expr(inst_id, e, func_params)?),
                    None => None,
                };
                Ok(Stmt::Let {
                    id: *id,
                    name: name.clone(),
                    ty: new_ty,
                    mutable: *mutable,
                    init: new_init,
                })
            }
            Stmt::Expr(e) => Ok(Stmt::Expr(self.rewrite_expr(inst_id, e, func_params)?)),
            Stmt::Throw(e) => Ok(Stmt::Throw(self.rewrite_expr(inst_id, e, func_params)?)),
            Stmt::Return(v) => Ok(Stmt::Return(match v {
                Some(e) => Some(self.rewrite_expr(inst_id, e, func_params)?),
                None => None,
            })),
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => Ok(Stmt::If {
                condition: self.rewrite_expr(inst_id, condition, func_params)?,
                then_branch: self.rewrite_stmts(inst_id, then_branch, func_params)?,
                else_branch: match else_branch {
                    Some(b) => Some(self.rewrite_stmts(inst_id, b, func_params)?),
                    None => None,
                },
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
                init: match init {
                    Some(s) => Some(Box::new(self.rewrite_stmt(inst_id, s, func_params)?)),
                    None => None,
                },
                condition: match condition {
                    Some(e) => Some(self.rewrite_expr(inst_id, e, func_params)?),
                    None => None,
                },
                update: match update {
                    Some(e) => Some(self.rewrite_expr(inst_id, e, func_params)?),
                    None => None,
                },
                body: self.rewrite_stmts(inst_id, body, func_params)?,
            }),
            Stmt::Try {
                body,
                catch,
                finally,
                forof_completion,
            } => Ok(Stmt::Try {
                body: self.rewrite_stmts(inst_id, body, func_params)?,
                catch: match catch {
                    Some(c) => Some(perry_hir::ir::CatchClause {
                        param: c.param.clone(),
                        body: self.rewrite_stmts(inst_id, &c.body, func_params)?,
                    }),
                    None => None,
                },
                finally: match finally {
                    Some(f) => Some(self.rewrite_stmts(inst_id, f, func_params)?),
                    None => None,
                },
                forof_completion: *forof_completion,
            }),
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                let mut new_cases = Vec::with_capacity(cases.len());
                for case in cases {
                    new_cases.push(perry_hir::ir::SwitchCase {
                        test: match &case.test {
                            Some(t) => Some(self.rewrite_expr(inst_id, t, func_params)?),
                            None => None,
                        },
                        body: self.rewrite_stmts(inst_id, &case.body, func_params)?,
                    });
                }
                Ok(Stmt::Switch {
                    discriminant: self.rewrite_expr(inst_id, discriminant, func_params)?,
                    cases: new_cases,
                })
            }
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
                    if !keep_inline_closure {
                        let fid = self
                            .solved
                            .instances
                            .get(target.0 as usize)
                            .map_or(0, |i| i.emitted_func_id);
                        return Ok(Expr::FuncRef(fid));
                    }
                    if let Expr::Closure {
                        func_id,
                        params,
                        body,
                        captures,
                        mutable_captures,
                        captures_this,
                        captures_new_target,
                        enclosing_class,
                        is_async,
                        is_generator,
                        is_strict,
                        is_arrow,
                        construct_only,
                        ..
                    } = expr
                    {
                        let ret_ty = self
                            .solved
                            .instances
                            .get(target.0 as usize)
                            .and_then(|i| {
                                self.solved.locations.get(i.return_loc.0 as usize).copied()
                            })
                            .map_or(HirType::Void, |t| self.materialize(t));
                        let mut new_params = Vec::with_capacity(params.len());
                        let param_span = self
                            .solved
                            .instances
                            .get(target.0 as usize)
                            .map(|i| i.param_locs);
                        for (i, p) in params.iter().enumerate() {
                            let mut np = p.clone();
                            if let Some(span) = param_span
                                && let Some(loc) = self
                                    .solved
                                    .param_locs
                                    .get(span.range())
                                    .and_then(|slice| slice.get(i))
                                && let Some(&ty) = self.solved.locations.get(loc.0 as usize)
                                && !self.types.is_open(ty)
                            {
                                np.ty = self.materialize(ty);
                            }
                            new_params.push(np);
                        }
                        let new_body = self.rewrite_stmts(target, body, None)?;
                        return Ok(Expr::Closure {
                            func_id: *func_id,
                            params: new_params,
                            return_type: ret_ty,
                            body: new_body,
                            captures: captures.clone(),
                            mutable_captures: mutable_captures.clone(),
                            captures_this: *captures_this,
                            captures_new_target: *captures_new_target,
                            enclosing_class: enclosing_class.clone(),
                            is_async: *is_async,
                            is_generator: *is_generator,
                            is_strict: *is_strict,
                            is_arrow: *is_arrow,
                            construct_only: *construct_only,
                        });
                    }
                }
                SiteResolution::CollectionNew { is_map, type_args } => {
                    let name = if is_map { "\0perry_Map" } else { "\0perry_Set" };
                    let materialized: Vec<HirType> =
                        type_args.into_iter().map(|t| self.materialize(t)).collect();
                    return Ok(Expr::New {
                        class_name: name.into(),
                        type_args: materialized,
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
                        .map_or(0, |i| i.emitted_func_id);
                    if let Expr::Call {
                        args,
                        byte_offset,
                        byte_end,
                        ..
                    } = expr
                    {
                        let mut new_args = Vec::with_capacity(args.len());
                        for (i, arg) in args.iter().enumerate() {
                            if i < 64 && (strip_arg_mask & (1u64 << i)) != 0 {
                                continue;
                            }
                            new_args.push(self.rewrite_expr(inst_id, arg, func_params)?);
                        }
                        return Ok(Expr::Call {
                            callee: Box::new(Expr::FuncRef(fid)),
                            args: new_args,
                            type_args: Vec::new(),
                            byte_offset: *byte_offset,
                            byte_end: *byte_end,
                        });
                    }
                    if let Expr::CallSpread {
                        args,
                        type_args,
                        byte_offset,
                        byte_end,
                        ..
                    } = expr
                    {
                        let mut new_args = Vec::with_capacity(args.len());
                        for arg in args {
                            new_args.push(match arg {
                                perry_hir::ir::CallArg::Expr(e) => perry_hir::ir::CallArg::Expr(
                                    self.rewrite_expr(inst_id, e, func_params)?,
                                ),
                                perry_hir::ir::CallArg::Spread(e) => {
                                    perry_hir::ir::CallArg::Spread(self.rewrite_expr(
                                        inst_id,
                                        e,
                                        func_params,
                                    )?)
                                }
                            });
                        }
                        return Ok(Expr::CallSpread {
                            callee: Box::new(Expr::FuncRef(fid)),
                            args: new_args,
                            type_args: type_args.clone(),
                            byte_offset: *byte_offset,
                            byte_end: *byte_end,
                        });
                    }
                }
                SiteResolution::MethodCall {
                    target,
                    strip_arg_mask,
                } => {
                    if let Expr::Call {
                        callee,
                        args,
                        byte_offset,
                        byte_end,
                        ..
                    } = expr
                        && let Expr::PropertyGet {
                            object,
                            byte_offset: prop_offset,
                            byte_end: prop_end,
                            ..
                        } = callee.as_ref()
                    {
                        let method_name = self
                            .solved
                            .instances
                            .get(target.0 as usize)
                            .map(|i| i.emitted_method_name.clone())
                            .unwrap_or_default();
                        let new_obj = self.rewrite_expr(inst_id, object, func_params)?;
                        let mut new_args = Vec::with_capacity(args.len());
                        for (i, arg) in args.iter().enumerate() {
                            if i < 64 && (strip_arg_mask & (1u64 << i)) != 0 {
                                continue;
                            }
                            new_args.push(self.rewrite_expr(inst_id, arg, func_params)?);
                        }
                        return Ok(Expr::Call {
                            callee: Box::new(Expr::PropertyGet {
                                object: Box::new(new_obj),
                                property: method_name,
                                byte_offset: *prop_offset,
                                byte_end: *prop_end,
                            }),
                            args: new_args,
                            type_args: Vec::new(),
                            byte_offset: *byte_offset,
                            byte_end: *byte_end,
                        });
                    }
                }
                SiteResolution::ClassNew {
                    class_inst,
                    strip_arg_mask,
                } => {
                    if let Expr::New {
                        args,
                        byte_offset,
                        byte_end,
                        cap_args_appended,
                        ..
                    } = expr
                    {
                        let name = self.class_inst_name(class_inst).unwrap_or_default();
                        let mut new_args = Vec::with_capacity(args.len());
                        for (i, arg) in args.iter().enumerate() {
                            if i < 64 && (strip_arg_mask & (1u64 << i)) != 0 {
                                continue;
                            }
                            new_args.push(self.rewrite_expr(inst_id, arg, func_params)?);
                        }
                        return Ok(Expr::New {
                            class_name: name,
                            type_args: Vec::new(),
                            args: new_args,
                            byte_offset: *byte_offset,
                            byte_end: *byte_end,
                            cap_args_appended: *cap_args_appended,
                        });
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
