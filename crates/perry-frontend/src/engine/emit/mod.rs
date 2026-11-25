//! Final monomorphic HIR emission phase.
mod async_helpers;
mod expr_plain;
mod expressions;
mod funcs;
mod resources;
mod stmts;
use super::bitset::BitSet;
use super::boundary::{LocalFlags, ModuleIndex, TemplateKind, TemplateView};
use super::solve::{
    ClassInst, FuncCoercion, Instance, Receiver, SiteEntry, SiteResolution, Solved,
};
use super::types::{
    CaptureOwner, CellKind, ClassId, ClassInstId, FuncIdentity, HelperId, Idx, IdxVec, InstanceId,
    TemplateId, Ty, TyKind, TypeTable,
};
use crate::hir_compat::resource_classes::{synth_call, synth_param};
use crate::hir_compat::{Locations, Metadata};
use perry_hir::{
    ir::{
        CallArg, CatchClause, Class, CompareOp, Expr, Function, Module as HirModule, Stmt,
        SwitchCase, UnaryOp,
    },
    types::{FuncId, LocalId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};
use pliron::context::Context;
use std::collections::HashMap;

pub(crate) struct Emitted {
    plans: super::InstancePlans,
    pub(crate) init: Vec<Stmt>,
    pub(crate) functions: Vec<Function>,
    pub(crate) classes: Vec<Class>,
    pub(crate) remapped_locals: Vec<(LocalId, LocalId)>,
    pub(crate) instantiations: Vec<(String, String, u32, Option<String>)>,
    pub(crate) inline_callbacks: Vec<FuncId>,
    pub(crate) class_origins: Vec<(String, String)>,
}

impl Emitted {
    pub(crate) fn apply(
        self,
        hir: &mut HirModule,
        metadata: &mut Metadata<'_>,
    ) -> super::InstancePlans {
        hir.init = self.init;
        hir.functions = self.functions;
        hir.classes = self.classes;
        for (old_id, new_id) in self.remapped_locals {
            if let Some(&span) = hir.local_source_spans.get(&old_id) {
                hir.local_source_spans.insert(new_id, span);
            }
        }
        for (inst_name, template_name, offset, caller) in self.instantiations {
            metadata
                .locations
                .record_instantiation(inst_name, template_name, offset, caller);
        }
        metadata.inline_callbacks.extend(self.inline_callbacks);
        metadata.class_origins.extend(self.class_origins);
        self.plans
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
        remapped_locals: Vec::new(),
        instantiations: Vec::new(),
        inline_callbacks: Vec::new(),
        class_origins: Vec::new(),
        environments: HashMap::new(),
        timer_record: None,
        helper_slots: solved.helpers.filled(None),
        expression_ids: HashMap::new(),
        plans: Default::default(),
    };
    let (init, functions, classes) = e.build_all()?;
    Ok(Emitted {
        plans: e.plans,
        init,
        functions,
        classes,
        remapped_locals: e.remapped_locals,
        instantiations: e.instantiations,
        inline_callbacks: e.inline_callbacks,
        class_origins: e.class_origins,
    })
}

type EnvField = (LocalId, String, Ty, bool);

struct Emitter<'h, 'a> {
    ctx: &'a Context,
    hir: &'h HirModule,
    index: &'a ModuleIndex<'h>,
    locations: &'a Locations<'a>,
    resource_members: &'a [crate::wit::ResourceMember<'a>],
    resource_functions: Vec<Option<FuncId>>,
    types: &'a TypeTable,
    solved: &'a Solved,
    next_local: LocalId,
    next_func_id: FuncId,
    remapped_locals: Vec<(LocalId, LocalId)>,
    instantiations: Vec<(String, String, u32, Option<String>)>,
    inline_callbacks: Vec<FuncId>,
    class_origins: Vec<(String, String)>,
    environments: HashMap<FuncIdentity, FuncId>,
    timer_record: Option<FuncId>,
    helper_slots: IdxVec<HelperId, Option<FuncId>>,
    expression_ids: HashMap<super::ExpressionOp, FuncId>,
    plans: super::InstancePlans,
}

struct RewriteCtx<'a> {
    inst_id: InstanceId,
    func_params: &'a [(LocalId, FuncId)],
    self_local: Option<LocalId>,

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

            void_return: self.void_return,
            has_expected,
            remap: None,
        }
    }
}

impl<'h> Emitter<'h, '_> {
    fn expression_op(&mut self, op: super::ExpressionOp, args: Vec<Expr>) -> Result<Expr> {
        let id = self.expression_id(op)?;
        Ok(synth_call(id, args, 0, 0))
    }

    fn expression_id(&mut self, op: super::ExpressionOp) -> Result<FuncId> {
        Ok(if let Some(&id) = self.expression_ids.get(&op) {
            id
        } else {
            let id = self.fresh_func_id()?;
            self.expression_ids.insert(op, id);
            self.plans
                .operations
                .insert(id, super::NativeOperation::Expression(op));
            id
        })
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
        let template = self.solved.instances[inst_id].template;
        let local_site = self.index.site_of(template, expr)?;
        self.solved.inst_sites[inst_id].get(local_site)
    }

    fn local_ty(&self, inst_id: InstanceId, id: LocalId) -> Option<Ty> {
        if let Some(lidx) = self.index.local_idx(id) {
            let local = self.index.locals[lidx];
            if local.flags.contains(LocalFlags::MODULE) {
                return local.module_loc().map(|loc| self.solved.locations[loc]);
            }
            if let Some(slot) = local.frame_slot()
                && local.owner == self.solved.instances[inst_id].template
            {
                return self.solved.inst_frame_locals[inst_id].get(slot).copied();
            }
        }
        self.solved.inst_local_tys[inst_id]
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
        let mut inst_remaps: IdxVec<InstanceId, Option<HashMap<LocalId, LocalId>>> =
            self.solved.instances.filled(None);
        let mut new_functions = Vec::new();
        for (inst_id, inst) in self.solved.instances.iter_enumerated().skip(1) {
            if !self.solved.live_instances.contains(inst_id.index()) {
                continue;
            }
            let view = self.index.view(inst.template);
            if let TemplateKind::PromiseResolve(_) | TemplateKind::PromiseReject(_) = view.kind {
                self.promise_handle_function(inst, view.kind)?;
                continue;
            }
            if !matches!(view.kind, TemplateKind::Function | TemplateKind::Closure) {
                continue;
            }
            let remap_locals = !seen_templates.insert(inst.template.index());
            let inherited = inst
                .capture_owner
                .instance()
                .and_then(|o| inst_remaps[o].as_ref())
                .cloned();
            let (func, remap) = self.emit_function(inst_id, inst, view, remap_locals, inherited)?;
            if !remap.is_empty() {
                inst_remaps[inst_id] = Some(remap);
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
        for (cinst_id, cinst) in self.solved.class_insts.iter_enumerated() {
            emitted_classes.insert(cinst.class.index());
            let Some(orig) = self.hir.classes.get(self.index.class(cinst.class).hir_idx) else {
                continue;
            };
            if cinst.emitted_name != orig.name {
                self.class_origins
                    .push((cinst.emitted_name.clone(), orig.name.clone()));
            }
            new_classes.push(self.emit_class(
                init_inst,
                cinst.class,
                Some((cinst_id, cinst)),
                orig,
            )?);
        }
        for (cid, cinfo) in self.index.classes.iter_enumerated() {
            if emitted_classes.contains(cid.index()) {
                continue;
            }
            let Some(orig) = self.hir.classes.get(cinfo.hir_idx) else {
                continue;
            };
            let has_statics = !self.solved.class_static_methods[cid].is_empty();
            if has_statics || !orig.static_fields.is_empty() || orig.extends_name.is_some() {
                new_classes.push(self.emit_class(init_inst, cid, None, orig)?);
            }
        }
        Ok((new_init, new_functions, new_classes))
    }

    fn emit_class(
        &mut self,
        init_inst: InstanceId,
        cid: ClassId,
        instantiated: Option<(ClassInstId, &ClassInst)>,
        orig: &'h Class,
    ) -> Result<Class> {
        let mut cls = orig.clone();
        cls.type_params.clear();
        let init_ctx = RewriteCtx::plain(init_inst);
        let name = instantiated
            .as_ref()
            .map_or(orig.name.as_str(), |(_, instance)| {
                instance.emitted_name.as_str()
            });
        let mut layout = super::ClassFields::default();
        let static_tys = &self.solved.class_static_field_tys[cid];
        cls.static_fields = orig
            .static_fields
            .iter()
            .enumerate()
            .map(|(f_idx, field)| {
                let mut f = field.clone();
                if let Some(&ty) = static_tys.get(f_idx)
                    && self.is_resolved(ty)
                {
                    self.prepare_layouts(ty)?;
                    layout.statics.insert(field.name.clone(), ty);
                }
                f.init = self.rewrite_opt_expr(&init_ctx, field.init.as_ref())?;
                Ok(f)
            })
            .collect::<Result<Vec<_>>>()?;
        cls.static_methods = self.emit_named_methods(&self.solved.class_static_methods[cid])?;
        let Some((cinst_id, cinst)) = instantiated else {
            cls.constructor = None;
            cls.methods.clear();
            cls.getters.clear();
            cls.setters.clear();
            self.plans.class_fields.insert(name.to_owned(), layout);
            return Ok(cls);
        };
        cls.name = cinst.emitted_name.clone();
        let field_locs = &self.solved.field_locs[cinst.field_locs];
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
                self.prepare_layouts(resolved)?;
                layout.instance.insert(field.name.clone(), resolved);
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
            Some(ctor_id) => {
                let inst = &self.solved.instances[ctor_id];
                let view = self.index.view(inst.template);
                Some(self.emit_function(ctor_id, inst, view, false, None)?.0)
            }
            None => orig.constructor.clone(),
        };
        let members = &self.solved.class_inst_members[cinst_id];
        cls.methods = self.emit_named_methods(&members.methods)?;
        cls.getters = self.emit_accessors(&members.getters, &orig.getters)?;
        cls.setters = self.emit_accessors(&members.setters, &orig.setters)?;
        self.plans.class_fields.insert(name.to_owned(), layout);
        Ok(cls)
    }

    fn emit_named_methods(&mut self, ids: &[InstanceId]) -> Result<Vec<Function>> {
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            let inst = &self.solved.instances[id];
            let view = self.index.view(inst.template);
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
            let view = self.index.view(inst.template);
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
        let mut param_types = Vec::with_capacity(view.params.len() + 2);
        let mut static_func_params: Vec<(LocalId, FuncId)> = Vec::new();
        let is_env_fn = self.is_env_template(inst.template);
        let self_ident = FuncIdentity {
            template: inst.template,
            capture_owner: inst.capture_owner,
        };
        let environment = if is_env_fn {
            self.environment(self_ident)?;
            let fields = self.env_layout(self_ident)?;
            let env_pid = self.fresh_synthetic_local()?;
            params.push(synth_param(
                env_pid,
                "__env".to_owned(),
                HirType::Named(super::types::env_class_name(self_ident)),
            ));
            param_types.push(None);
            Some((env_pid, fields))
        } else {
            None
        };
        let self_local = match inst.receiver {
            Some(Receiver::Object(obj_ty)) => {
                let sid = self.fresh_synthetic_local()?;
                self.prepare_layouts(obj_ty)?;
                params.push(synth_param(sid, "__self".to_owned(), HirType::Any));
                param_types.push(Some(obj_ty));
                Some(sid)
            }
            _ => None,
        };

        for (p_idx, param) in view.params.iter().enumerate() {
            let param_loc = *self.solved.param_locs[inst.param_locs]
                .get(p_idx)
                .ok_or_else(|| {
                    Diagnostic::internal("emitted function missing parameter location slot")
                })?;
            let param_ty = self.solved.locations[param_loc];
            if let TyKind::Func(ident) = self.types.kind(param_ty)
                && !self.is_env_template(ident.template)
            {
                static_func_params.push((param.id, self.find_func_identity_instance(ident)));
                continue;
            }
            let mut new_param = param.clone();
            let resolved = self.is_resolved(param_ty).then_some(param_ty);
            param_types.push(resolved);
            if resolved.is_some() {
                self.prepare_layouts(param_ty)?;
            } else if !self.index.is_param_annotated(param.id)
                && !inst.is_witness
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

                void_return: false,
                has_expected: false,
                remap: None,
            };
            new_param.default = self.rewrite_opt_expr(&param_ctx, param.default.as_ref())?;
            if self.is_boxed_local(param.id) {
                let ty = self.boxed_type(param.id, resolved)?;
                self.plans
                    .boxed_parameters
                    .insert((inst.emitted_func_id, new_param.id), ty);
            }
            params.push(new_param);
        }

        let ret_ty = self.solved.locations[inst.return_loc];
        let void_return = self.is_zero_sized_func(ret_ty);
        let result = if void_return {
            Some(Ty::VOID)
        } else if self.is_resolved(ret_ty) {
            self.prepare_layouts(ret_ty)?;
            Some(ret_ty)
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
            None
        };
        self.plans.signatures.insert(
            inst.emitted_func_id,
            super::InstanceSignature {
                params: param_types,
                result,
            },
        );
        let mut rctx = RewriteCtx {
            inst_id,
            func_params: &static_func_params,
            self_local,

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

        if let Some((parameter, fields)) = environment {
            let captures = fields
                .into_iter()
                .map(|(id, field, ty, boxed)| super::Capture {
                    local: remap.get(&id).copied().unwrap_or(id),
                    field,
                    storage: if boxed {
                        super::Storage::Boxed {
                            ty,
                            init: self.cell_init(id),
                        }
                    } else {
                        super::Storage::Value(ty)
                    },
                })
                .collect();
            self.plans.captures.insert(parameter, captures);
        }

        let captures = if is_env_fn {
            Vec::new()
        } else {
            let mut caps = view.captures.to_vec();
            caps.extend(self.solved.inst_captures[inst_id].iter().copied());
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

        if !inst.is_witness && inst.caller.is_some() {
            let caller_name = inst
                .caller
                .map(|c| &self.solved.instances[c])
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
                return_type: view.return_type.clone(),
                body,
                captures,
                is_async: view.is_async,
                is_generator: view.is_generator,
                is_strict: view.is_strict,
                construct_only: view.construct_only,
                is_exported: inst.is_witness,
                decorators: Vec::new(),
                was_plain_async: false,
                was_unrolled: false,
            },
            remap,
        ))
    }

    fn find_func_identity_instance(&self, ident: FuncIdentity) -> FuncId {
        self.solved
            .instances
            .iter_enumerated()
            .find_map(|(id, inst)| {
                (self.solved.live_instances.contains(id.index())
                    && inst.template == ident.template
                    && inst.capture_owner == ident.capture_owner)
                    .then_some(inst.emitted_func_id)
            })
            .unwrap_or_else(|| self.index.view(ident.template).func_id)
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
