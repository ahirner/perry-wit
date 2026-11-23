//! Final monomorphic HIR emission phase.
mod funcs;
use super::bitset::BitSet;
use super::boundary::{ModuleIndex, TemplateKind, TemplateView};
use super::solve::{ClassInst, FuncCoercion, Instance, Receiver, SiteResolution, Solved};
use super::types::{
    CellKind, ClassInstId, FuncIdentity, InstanceId, TemplateId, Ty, TyKind, TypeTable, box_hir,
    func_tag,
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
            .json_eligible_bindings
            .extend(self.json_eligible_bindings);
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
        locations,
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
        synth_shapes: HashMap::new(),
        synth_classes: Vec::new(),
        synth_functions: Vec::new(),
        dispatchers: HashMap::new(),
        wideners: HashMap::new(),
        casters: HashMap::new(),
        comparers: HashMap::new(),
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
        deletes_array_elements: solved.deletes_array_elements,
    })
}

type DispatcherKey = (Ty, Vec<(FuncIdentity, InstanceId)>, Ty, u64, Option<Ty>);
type EnvField = (LocalId, String, Ty, bool);

struct Emitter<'h, 'a> {
    ctx: &'a Context,
    hir: &'h HirModule,
    index: &'a ModuleIndex<'h>,
    locations: &'a Locations<'a>,
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
    synth_shapes: HashMap<Vec<String>, String>,
    synth_classes: Vec<Class>,
    synth_functions: Vec<Function>,
    dispatchers: HashMap<DispatcherKey, FuncId>,
    wideners: HashMap<(Ty, Ty), FuncId>,
    casters: HashMap<Ty, FuncId>,
    comparers: HashMap<(Ty, Ty), FuncId>,
}

struct RewriteCtx<'a> {
    inst_id: InstanceId,
    func_params: &'a [(LocalId, FuncId)],
    self_local: Option<LocalId>,
    env_local: Option<LocalId>,
    env_captures: &'a [(LocalId, String, bool)],
    self_capture_locals: &'a [LocalId],
    self_env_tag: Option<&'a str>,
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
            self_capture_locals: &[],
            self_env_tag: None,
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
            self_capture_locals: self.self_capture_locals,
            self_env_tag: self.self_env_tag,
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

    fn site_of(&self, inst_id: InstanceId, expr: &Expr) -> Option<&SiteResolution> {
        let template = self.solved.instances.get(inst_id)?.template;
        let local_site = self.index.site_of(template, expr)?;
        self.solved
            .inst_sites
            .get(inst_id.index())?
            .get(local_site)?
            .as_ref()
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
            if !matches!(view.kind, TemplateKind::Function | TemplateKind::Closure) {
                continue;
            }
            let keep_original = view.is_exported
                || self
                    .index
                    .exported_obj_methods
                    .contains(inst.template.index());
            let remap_locals = !keep_original && !seen_templates.insert(inst.template.index());
            let inherited = inst
                .capture_owner
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
        let field_locs = self
            .solved
            .field_locs
            .get(cinst.field_locs.range())
            .unwrap_or(&[]);
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
            f.init = self.rewrite_opt_expr(&init_ctx, field.init.as_ref())?;
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
        let mut remap = inherited.unwrap_or_default();
        let has_inherited = !remap.is_empty();
        let mut params = Vec::with_capacity(view.params.len() + 2);
        let mut static_func_params: Vec<(LocalId, FuncId)> = Vec::new();
        let is_env_fn = self.is_env_template(inst.template);
        let self_ident = FuncIdentity {
            template: inst.template,
            capture_owner: inst.capture_owner,
        };
        let self_env_tag_owned = is_env_fn.then(|| func_tag(self_ident));
        let (env_local, env_captures, self_capture_locals) = if is_env_fn {
            let (fields, self_caps) = self.env_layout(self_ident);
            let env_pid = self.fresh_synthetic_local()?;
            let mut env_props = HashMap::with_capacity(fields.len().max(1));
            let mut env_order = Vec::with_capacity(fields.len().max(1));
            let mut cap_entries = Vec::with_capacity(fields.len());
            if fields.is_empty() {
                env_order.push("_u".to_owned());
                env_props.insert(
                    "_u".to_owned(),
                    perry_hir::types::PropertyInfo {
                        ty: HirType::Number,
                        optional: false,
                        readonly: false,
                    },
                );
            } else {
                for (cap_id, fname, fty, boxed) in fields {
                    let inner = self.materialize(fty);
                    let ty = if boxed { box_hir(inner) } else { inner };
                    env_order.push(fname.clone());
                    env_props.insert(
                        fname.clone(),
                        perry_hir::types::PropertyInfo {
                            ty,
                            optional: false,
                            readonly: false,
                        },
                    );
                    cap_entries.push((cap_id, fname, boxed));
                }
            }
            let env_ty = HirType::Object(perry_hir::types::ObjectType {
                name: None,
                properties: env_props,
                property_order: Some(env_order),
                index_signature: None,
            });
            params.push(Self::synth_param(env_pid, "__env".to_owned(), env_ty));
            (Some(env_pid), cap_entries, self_caps)
        } else {
            (None, Vec::new(), Vec::new())
        };
        let self_local = match inst.receiver {
            Some(Receiver::Object(obj_ty)) => {
                let sid = self.fresh_synthetic_local()?;
                params.push(Self::synth_param(
                    sid,
                    "__self".to_owned(),
                    self.materialize(obj_ty),
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
                new_param.ty = self.materialize(param_ty);
            } else if !self.index.is_param_annotated(param.id)
                && !view.is_exported
                && !self
                    .index
                    .exported_obj_methods
                    .contains(inst.template.index())
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
                self_capture_locals: &self_capture_locals,
                self_env_tag: self_env_tag_owned.as_deref(),
                boxed_param_map: &boxed_param_map,
                void_return: false,
                has_expected: false,
                remap: None,
            };
            new_param.default = self.rewrite_opt_expr(&param_ctx, param.default.as_ref())?;
            if self.is_boxed_local(param.id) {
                let box_id = self.fresh_synthetic_local()?;
                boxed_param_map.push((param.id, box_id));
                let init_expr = self.emit_box_record(Expr::LocalGet(new_param.id));
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
            self.materialize(ret_ty)
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
            self_capture_locals: &self_capture_locals,
            self_env_tag: self_env_tag_owned.as_deref(),
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

        if !view.is_exported && inst.caller.is_some() {
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
                (self.solved.live_instances.contains(id.index())
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
                        .is_none_or(|li| self.index.local_inferred.contains(li.index()));
                let is_deferred = self
                    .index
                    .local_idx(*id)
                    .is_some_and(|li| self.index.local_deferred.contains(li.index()));
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
                        if is_inferred
                            && matches!(
                                self.types.kind(t),
                                TyKind::Cell {
                                    kind: CellKind::Array,
                                    ..
                                }
                            ) =>
                    {
                        let mut err = diag!(codes::MISSING_TYPE_ANNOTATION, "an empty array literal needs a declared element type")
                            .with_help("Add a concrete element type annotation to this binding (e.g. `const xs: number[] = []`).");
                        if let TyKind::Cell { loc, .. } = self.types.kind(t)
                            && self.solved.self_nested_locs.contains(loc.index())
                        {
                            err = err.with_note("its inferred element type nests itself");
                        }
                        let err = self.annotate_local(err, *id);
                        return Err(self.attach_inst_chain(err, rctx.inst_id));
                    }
                    Some(Ty::OPEN)
                        if is_inferred
                            && self.index.local_idx(*id).is_some_and(|lidx| {
                                self.solved.self_nested_locals.contains(lidx.index())
                            }) =>
                    {
                        let err = diag!(
                            codes::MISSING_TYPE_ANNOTATION,
                            "cannot infer type of '{name}': inferred type nests itself"
                        )
                        .with_help("Add a concrete type annotation to this binding.");
                        let err = self.annotate_local(err, *id);
                        return Err(self.attach_inst_chain(err, rctx.inst_id));
                    }
                    _ => ty.clone(),
                };
                let init_has_expected = !matches!(new_ty, HirType::Any | HirType::Unknown);
                let init_ctx = rctx.with_expected(init_has_expected);
                let rewritten_init = self.rewrite_opt_expr(&init_ctx, init.as_ref())?;
                if self.is_boxed_local(*id) {
                    let init_val = rewritten_init.unwrap_or_else(|| placeholder_for_hir(&new_ty));
                    return Ok(Stmt::Let {
                        id: out_id,
                        name: name.clone(),
                        ty: box_hir(new_ty),
                        mutable: false,
                        init: Some(self.emit_box_record(init_val)),
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
        let coercion = self.coercion_of(rctx.inst_id, expr);
        if let Some(c) = coercion {
            match c {
                FuncCoercion::Zero if is_pure_func_expr(expr) => return Ok(Expr::Number(0.0)),
                FuncCoercion::Tag(ident) if is_pure_func_expr(expr) => {
                    return Ok(self.emit_tag_record(ident));
                }
                _ => {}
            }
        }
        let rewritten = self.rewrite_expr_inner(rctx, expr)?;
        match coercion {
            Some(FuncCoercion::Zero) => Ok(Expr::Sequence(vec![rewritten, Expr::Number(0.0)])),
            Some(FuncCoercion::Tag(ident)) => {
                let tag = self.emit_tag_record(ident);
                Ok(Expr::Sequence(vec![rewritten, tag]))
            }
            Some(FuncCoercion::BoxEnv(ident)) => {
                let val_expr = Self::prop_get(rewritten, "val");
                Ok(self.emit_env_wrapper(ident, val_expr))
            }
            Some(FuncCoercion::Widen { from, to }) => {
                let fid = self.intern_widener(from, to)?;
                Ok(Self::synth_call(fid, vec![rewritten], 0, 0))
            }
            Some(FuncCoercion::EnvCallback { target, input_tys }) => {
                let target_fid = self
                    .instance(target, "missing env callback target instance")?
                    .emitted_func_id;
                let inputs = self.types.tuple_slice(input_tys).to_vec();
                let arm_ret = self.arm_return_hir(target);
                self.emit_callback_wrapper(
                    target_fid,
                    Self::prop_get(rewritten, "val"),
                    &inputs,
                    arm_ret,
                )
            }
            Some(FuncCoercion::DispatchCallback {
                callee_ty,
                arms,
                input_tys,
            }) => {
                let arm_slice = self
                    .solved
                    .dispatch_arms
                    .get(arms.range())
                    .unwrap_or(&[])
                    .to_vec();
                let inputs = self.types.tuple_slice(input_tys).to_vec();
                let max_params = arm_slice
                    .iter()
                    .filter_map(|&(ident, _)| self.index.view(ident.template))
                    .map(|v| {
                        if v.params.iter().any(|p| p.is_rest) {
                            inputs.len()
                        } else {
                            v.params.len()
                        }
                    })
                    .max()
                    .unwrap_or(0)
                    .min(inputs.len());
                let (disp_fid, _) =
                    self.intern_dispatcher(callee_ty, &arm_slice, input_tys, 0, None)?;
                let disp_ret = self
                    .synth_functions
                    .iter()
                    .find(|f| f.id == disp_fid)
                    .map_or(HirType::Void, |f| f.return_type.clone());
                self.emit_callback_wrapper(
                    disp_fid,
                    rewritten,
                    inputs.get(..max_params).unwrap_or(&inputs),
                    disp_ret,
                )
            }
            None => Ok(rewritten),
        }
    }

    fn rewrite_expr_inner(&mut self, rctx: &RewriteCtx<'_>, expr: &'h Expr) -> Result<Expr> {
        if let Some(site) = self.site_of(rctx.inst_id, expr).cloned() {
            match site {
                SiteResolution::Cell(_) => {}
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
                            captures,
                            mutable_captures,
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
                            let mut cb_ctx = RewriteCtx {
                                inst_id: target,
                                func_params: &[],
                                self_local: None,
                                env_local: rctx.env_local,
                                env_captures: rctx.env_captures,
                                self_capture_locals: rctx.self_capture_locals,
                                self_env_tag: rctx.self_env_tag,
                                boxed_param_map: rctx.boxed_param_map,
                                void_return: false,
                                has_expected: false,
                                remap: None,
                            };
                            *out_body = self.rewrite_stmts(&mut cb_ctx, body)?;
                            if rctx.env_local.is_some()
                                || !rctx.boxed_param_map.is_empty()
                                || captures.iter().any(|c| self.is_boxed_local(*c))
                            {
                                let mut new_caps = Vec::new();
                                let mut defined: std::collections::HashSet<usize> =
                                    params.iter().map(|p| p.id as usize).collect();
                                for s in out_body.iter() {
                                    perry_hir::analysis::collect_local_refs_stmt(
                                        s,
                                        &mut new_caps,
                                        &mut defined,
                                    );
                                }
                                *captures = new_caps;
                                mutable_captures.retain(|c| {
                                    !self.is_boxed_local(*c)
                                        && !rctx.env_captures.iter().any(|(lid, _, _)| lid == c)
                                        && !rctx.boxed_param_map.iter().any(|(lid, _)| lid == c)
                                });
                            }
                        }
                        return Ok(out);
                    }
                }
                SiteResolution::DirectCall {
                    target,
                    strip_arg_mask,
                    pass_receiver,
                    pass_env,
                } => {
                    let fid = self
                        .instance(target, "missing direct call target instance")?
                        .emitted_func_id;
                    let mut out = expr.clone();
                    match (expr, &mut out) {
                        (
                            Expr::Call {
                                callee: orig_callee,
                                args,
                                ..
                            },
                            Expr::Call {
                                callee,
                                args: out_args,
                                type_args,
                                ..
                            },
                        ) => {
                            let arg_ctx = rctx.with_expected(true);
                            let mut rewritten_args =
                                self.rewrite_filtered_args(&arg_ctx, args, strip_arg_mask)?;
                            let pre_effect = if pass_env {
                                if pass_receiver
                                    && let Expr::PropertyGet { object, .. } = orig_callee.as_ref()
                                {
                                    rewritten_args.insert(0, self.rewrite_expr(&arg_ctx, object)?);
                                }
                                let env_arg = if let Expr::LocalGet(id) = orig_callee.as_ref()
                                    && rctx.self_capture_locals.contains(id)
                                    && let Some(env_pid) = rctx.env_local
                                {
                                    Expr::LocalGet(env_pid)
                                } else {
                                    Self::prop_get(self.rewrite_expr(&arg_ctx, orig_callee)?, "val")
                                };
                                rewritten_args.insert(0, env_arg);
                                None
                            } else if let Expr::PropertyGet { object, .. } = orig_callee.as_ref() {
                                if pass_receiver {
                                    rewritten_args.insert(0, self.rewrite_expr(&arg_ctx, object)?);
                                    None
                                } else if !is_pure_func_expr(object) {
                                    Some(self.rewrite_expr(rctx, object)?)
                                } else {
                                    None
                                }
                            } else if !is_pure_func_expr(orig_callee) {
                                Some(self.rewrite_expr(rctx, orig_callee)?)
                            } else {
                                None
                            };
                            **callee = Expr::FuncRef(fid);
                            *out_args = rewritten_args;
                            type_args.clear();
                            return Ok(match pre_effect {
                                Some(eff) => Expr::Sequence(vec![eff, out]),
                                None => out,
                            });
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
                SiteResolution::FuncCompare {
                    left_ty,
                    right_ty,
                    op,
                } => {
                    if let Expr::Compare { left, right, .. } = expr {
                        let is_eq = matches!(op, CompareOp::Eq | CompareOp::LooseEq);
                        let l_zero = self.is_zero_sized_func(left_ty);
                        let r_zero = self.is_zero_sized_func(right_ty);
                        if l_zero || r_zero {
                            let (zero_ty, union_expr, union_ty) = if l_zero {
                                (left_ty, right.as_ref(), right_ty)
                            } else {
                                (right_ty, left.as_ref(), left_ty)
                            };
                            if let TyKind::Func(ident) = self.types.kind(zero_ty) {
                                if !self.types.func_members(union_ty).contains(&ident) {
                                    return Ok(Expr::Bool(!is_eq));
                                }
                                let u_rewritten = self.rewrite_expr(rctx, union_expr)?;
                                return Ok(Expr::Compare {
                                    op: if is_eq { CompareOp::Eq } else { CompareOp::Ne },
                                    left: Box::new(Self::prop_get(u_rewritten, "tag")),
                                    right: Box::new(Expr::String(func_tag(ident))),
                                });
                            }
                        }
                        let l_env = self
                            .types
                            .is_env_func(left_ty, &self.solved.escaping_env_templates);
                        let r_env = self
                            .types
                            .is_env_func(right_ty, &self.solved.escaping_env_templates);
                        let l_rewritten = self.rewrite_expr(rctx, left)?;
                        let r_rewritten = self.rewrite_expr(rctx, right)?;
                        if l_env && r_env && left_ty == right_ty {
                            return Ok(Expr::Compare {
                                op: if is_eq { CompareOp::Eq } else { CompareOp::Ne },
                                left: Box::new(Self::prop_get(l_rewritten, "val")),
                                right: Box::new(Self::prop_get(r_rewritten, "val")),
                            });
                        }
                        let cmp_fid = self.intern_comparer(left_ty, right_ty)?;
                        let call = Self::synth_call(cmp_fid, vec![l_rewritten, r_rewritten], 0, 0);
                        return Ok(if is_eq {
                            call
                        } else {
                            Expr::Unary {
                                op: UnaryOp::Not,
                                operand: Box::new(call),
                            }
                        });
                    }
                }
                SiteResolution::ObjectLiteral { keep_mask } => {
                    let pairs: Vec<(&str, &'h Expr)> = match expr {
                        Expr::Object(fields) => {
                            fields.iter().map(|(k, v)| (k.as_str(), v)).collect()
                        }
                        Expr::New {
                            class_name, args, ..
                        } => self
                            .hir
                            .classes
                            .iter()
                            .find(|c| c.name == *class_name)
                            .map(|c| {
                                c.fields
                                    .iter()
                                    .zip(args)
                                    .map(|(f, a)| (f.name.as_str(), a))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        _ => Vec::new(),
                    };
                    let mut kept_names = Vec::new();
                    let mut kept_args = Vec::new();
                    let mut effects = Vec::new();
                    for (idx, (k, v)) in pairs.into_iter().enumerate() {
                        if idx < 64 && (keep_mask & (1u64 << idx)) != 0 {
                            kept_names.push(k);
                            kept_args.push(self.rewrite_expr(rctx, v)?);
                        } else if !is_pure_func_expr(v) {
                            effects.push(self.rewrite_expr(rctx, v)?);
                        }
                    }
                    let synth_name = self.intern_anon_shape(&kept_names);
                    let constructed = Expr::New {
                        class_name: synth_name,
                        args: kept_args,
                        type_args: Vec::new(),
                        byte_offset: 0,
                        byte_end: 0,
                        cap_args_appended: 0,
                    };
                    return Ok(if effects.is_empty() {
                        constructed
                    } else {
                        effects.push(constructed);
                        Expr::Sequence(effects)
                    });
                }
                SiteResolution::DispatchCall {
                    callee_ty,
                    arms,
                    strip_arg_mask,
                    arg_tys,
                    recv_ty,
                } => {
                    let arm_slice = self
                        .solved
                        .dispatch_arms
                        .get(arms.range())
                        .unwrap_or(&[])
                        .to_vec();
                    let (fid, needs_recv) = self.intern_dispatcher(
                        callee_ty,
                        &arm_slice,
                        arg_tys,
                        strip_arg_mask,
                        recv_ty,
                    )?;
                    if let Expr::Call {
                        callee: orig_callee,
                        args,
                        byte_offset,
                        byte_end,
                        ..
                    } = expr
                    {
                        let arg_ctx = rctx.with_expected(true);
                        let mut call_args = Vec::with_capacity(args.len() + 2);
                        call_args.push(self.rewrite_expr(&arg_ctx, orig_callee)?);
                        if needs_recv && let Expr::PropertyGet { object, .. } = orig_callee.as_ref()
                        {
                            call_args.push(self.rewrite_expr(&arg_ctx, object)?);
                        }
                        call_args.extend(self.rewrite_filtered_args(
                            &arg_ctx,
                            args,
                            strip_arg_mask,
                        )?);
                        return Ok(Self::synth_call(fid, call_args, *byte_offset, *byte_end));
                    }
                }
                SiteResolution::TaggedArray(arr_ty) => {
                    if let Expr::Array(elems, _) = expr {
                        let elem_ctx = rctx.with_expected(true);
                        let mut out_elems = Vec::with_capacity(elems.len());
                        for e in elems {
                            out_elems.push(self.rewrite_expr(&elem_ctx, e)?);
                        }
                        let arr = Expr::Array(out_elems, None);
                        if rctx.has_expected {
                            return Ok(arr);
                        }
                        let caster = self.intern_caster(arr_ty)?;
                        return Ok(Self::synth_call(caster, vec![arr], 0, 0));
                    }
                }
                SiteResolution::ZeroSizedArray => {
                    if let Expr::Array(elems, _) = expr {
                        let mut out_elems = Vec::with_capacity(elems.len());
                        for e in elems {
                            if is_pure_func_expr(e) {
                                out_elems.push(Expr::Number(0.0));
                            } else {
                                out_elems.push(Expr::Sequence(vec![
                                    self.rewrite_expr(rctx, e)?,
                                    Expr::Number(0.0),
                                ]));
                            }
                        }
                        return Ok(Expr::Array(out_elems, None));
                    }
                }
                SiteResolution::ZeroSizedFieldSet => {
                    if let Expr::PropertySet { object, value, .. }
                    | Expr::PutValueSet {
                        target: object,
                        value,
                        ..
                    } = expr
                    {
                        let mut effects = Vec::new();
                        if !is_pure_func_expr(object) {
                            effects.push(self.rewrite_expr(rctx, object)?);
                        }
                        if !is_pure_func_expr(value) {
                            effects.push(self.rewrite_expr(rctx, value)?);
                        }
                        return Ok(if effects.is_empty() {
                            Expr::Undefined
                        } else {
                            effects.push(Expr::Undefined);
                            Expr::Sequence(effects)
                        });
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
        self.rewrite_expr_plain(rctx, expr)
    }

    fn boxed_target(&self, rctx: &RewriteCtx<'_>, id: LocalId) -> Option<Expr> {
        if let Some(env_pid) = rctx.env_local
            && let Some((_, field, boxed)) = rctx.env_captures.iter().find(|(lid, _, _)| *lid == id)
        {
            return boxed.then(|| Self::prop_get(Expr::LocalGet(env_pid), field));
        }
        if let Some(&(_, box_id)) = rctx.boxed_param_map.iter().find(|(lid, _)| *lid == id) {
            return Some(Expr::LocalGet(box_id));
        }
        if self.is_boxed_local(id) {
            return Some(Expr::LocalGet(id));
        }
        None
    }

    fn rewrite_expr_plain(&mut self, rctx: &RewriteCtx<'_>, expr: &'h Expr) -> Result<Expr> {
        if let Expr::This = expr
            && let Some(sid) = rctx.self_local
        {
            return Ok(Expr::LocalGet(sid));
        }
        if let Expr::Closure { func_id, .. } = expr
            && let Some(tid) = self.index.template_of_func(*func_id)
            && self.is_env_template(tid)
        {
            let ident = FuncIdentity {
                template: tid,
                capture_owner: Some(rctx.inst_id),
            };
            let (fields, _) = self.env_layout(ident);
            let env_expr = if fields.is_empty() {
                self.emit_anon_new(&["_u"], vec![Expr::Number(0.0)])
            } else {
                let names: Vec<&str> = fields.iter().map(|(_, f, _, _)| f.as_str()).collect();
                let args = fields
                    .iter()
                    .map(|&(cap_id, _, _, _)| self.emit_capture_ref(rctx, cap_id))
                    .collect();
                self.emit_anon_new(&names, args)
            };
            return Ok(self.emit_env_wrapper(ident, env_expr));
        }
        if let Expr::LocalGet(id) = expr {
            if let Some((_, fid)) = rctx.func_params.iter().find(|(lid, _)| lid == id) {
                return Ok(Expr::FuncRef(*fid));
            }
            if rctx.self_capture_locals.contains(id)
                && let (Some(env_pid), Some(tag)) = (rctx.env_local, rctx.self_env_tag)
            {
                return Ok(self.emit_env_wrapper_tag(tag, Expr::LocalGet(env_pid)));
            }
            if let Some(env_pid) = rctx.env_local
                && let Some((_, field, boxed)) =
                    rctx.env_captures.iter().find(|(lid, _, _)| lid == id)
            {
                let base = Self::prop_get(Expr::LocalGet(env_pid), field);
                return Ok(if *boxed {
                    Self::prop_get(base, "v")
                } else {
                    base
                });
            }
            if let Some(&(_, box_id)) = rctx.boxed_param_map.iter().find(|(lid, _)| lid == id) {
                return Ok(Self::prop_get(Expr::LocalGet(box_id), "v"));
            }
            if self.is_boxed_local(*id) {
                return Ok(Self::prop_get(Expr::LocalGet(*id), "v"));
            }
        }
        if let Expr::LocalSet(id, value) = expr
            && let Some(box_obj) = self.boxed_target(rctx, *id)
        {
            let val = self.rewrite_expr(rctx, value)?;
            return Ok(Expr::PropertySet {
                object: Box::new(box_obj),
                property: "v".into(),
                value: Box::new(val),
                byte_offset: 0,
                byte_end: 0,
            });
        }
        if let Expr::Update { id, op, prefix } = expr
            && let Some(box_obj) = self.boxed_target(rctx, *id)
        {
            let bin_op = match op {
                UpdateOp::Increment => BinaryOp::Add,
                UpdateOp::Decrement => BinaryOp::Sub,
            };
            return Ok(Expr::PropertyUpdate {
                object: Box::new(box_obj),
                property: "v".into(),
                op: bin_op,
                prefix: *prefix,
                strict: true,
                byte_offset: 0,
                byte_end: 0,
            });
        }
        if rctx.self_local.is_some()
            && matches!(
                expr,
                Expr::Closure {
                    captures_this: false,
                    ..
                }
            )
        {
            let nested = RewriteCtx {
                inst_id: rctx.inst_id,
                remap: None,
                func_params: rctx.func_params,
                self_local: None,
                env_local: rctx.env_local,
                env_captures: rctx.env_captures,
                self_capture_locals: rctx.self_capture_locals,
                self_env_tag: rctx.self_env_tag,
                boxed_param_map: rctx.boxed_param_map,
                void_return: false,
                has_expected: false,
            };
            let mut cloned = expr.clone();
            let mut orig_children: Vec<&'h Expr> = Vec::new();
            perry_hir::walker::walk_expr_children(expr, &mut |child| orig_children.push(child));
            let (mut idx, mut err) = (0usize, None);
            perry_hir::walker::walk_expr_children_mut(&mut cloned, &mut |child| {
                if err.is_none()
                    && let Some(&orig) = orig_children.get(idx)
                {
                    idx += 1;
                    match self.rewrite_expr(&nested, orig) {
                        Ok(r) => *child = r,
                        Err(e) => err = Some(e),
                    }
                }
            });
            return match err {
                Some(e) => Err(e),
                None => Ok(cloned),
            };
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

fn placeholder_for_hir(ty: &HirType) -> Expr {
    match ty {
        HirType::Number | HirType::Int32 => Expr::Number(0.0),
        HirType::Boolean => Expr::Bool(false),
        HirType::String => Expr::String(String::new()),
        _ => Expr::Undefined,
    }
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
