//! Bind solved instances to borrowed source declarations and native MIR operation plans.
use crate::source::{
    Bodies, InstanceBodies, InstanceClass, InstanceFunction, ModuleDeclarations, is_pure_func_expr,
};
mod async_helpers;
mod expressions;
mod funcs;
mod stmts;
use super::bitset::BitSet;
use super::boundary::{LocalFlags, ModuleIndex, TemplateKind, TemplateView};
use super::solve::Invocation;
use super::solve::{ClassInst, FuncCoercion, Instance, Receiver, SiteEntry, Solved};
use super::types::{
    CaptureOwner, CellKind, ClassId, ClassInstId, FuncIdentity, HelperId, Idx, IdxVec, InstanceId,
    TemplateId, Ty, TyKind, TypeTable,
};
use perry_hir::{
    ir::{Class, CompareOp, Expr, Function, Stmt},
    types::{FuncId, LocalId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};
use std::collections::HashSet;

pub(super) fn build<'h>(
    bodies: &'h Bodies,
    declarations: &ModuleDeclarations,
    locations: &crate::hir_compat::Locations<'_>,
    index: &ModuleIndex<'h>,
    types: &TypeTable,
    solved: &Solved,
    instantiations: &mut Vec<super::InstantiationOrigin>,
) -> Result<(InstanceBodies<'h>, super::InstancePlans)> {
    let max_inst_fid = solved
        .instances
        .iter()
        .map(|i| i.emitted_func_id)
        .max()
        .unwrap_or(0);
    let mut planner = InstancePlanner {
        bodies,
        declarations,
        locations,
        index,
        types,
        solved,
        next_func_id: index.max_func_id.max(max_inst_fid).saturating_add(1),
        instantiations,
        environments: HashSet::new(),
        helper_slots: solved.helpers.filled(None),
        plans: Default::default(),
    };
    let bodies = planner.build_all()?;
    Ok((bodies, planner.plans))
}

struct InstancePlanner<'h, 'a> {
    bodies: &'h Bodies,
    declarations: &'a ModuleDeclarations,
    locations: &'a crate::hir_compat::Locations<'a>,
    index: &'a ModuleIndex<'h>,
    types: &'a TypeTable,
    solved: &'a Solved,
    next_func_id: FuncId,
    instantiations: &'a mut Vec<super::InstantiationOrigin>,
    environments: HashSet<FuncIdentity>,
    helper_slots: IdxVec<HelperId, Option<FuncId>>,
    plans: super::InstancePlans,
}

struct InstanceContext<'a> {
    inst_id: InstanceId,
    func_params: &'a [(LocalId, FuncId)],
    void_return: bool,
    has_expected: bool,
}
impl InstanceContext<'_> {
    fn plain(inst_id: InstanceId) -> Self {
        Self {
            inst_id,
            func_params: &[],
            void_return: false,
            has_expected: false,
        }
    }
    fn with_expected(&self, has_expected: bool) -> InstanceContext<'_> {
        InstanceContext {
            inst_id: self.inst_id,
            func_params: self.func_params,
            void_return: self.void_return,
            has_expected,
        }
    }
}

impl<'h> InstancePlanner<'h, '_> {
    fn owner(&self, instance: InstanceId) -> Option<FuncId> {
        (instance != self.solved.init_inst).then(|| self.solved.instances[instance].emitted_func_id)
    }

    fn is_resolved(&self, ty: Ty) -> bool {
        !self.types.is_open_with(
            ty,
            self.solved.locations.as_slice(),
            &self.solved.dict_reads,
        )
    }

    fn annotate_local(&self, err: Diagnostic, id: LocalId) -> Diagnostic {
        match self.declarations.locals.get(&id) {
            Some(local) => self.locations.annotate(err, local.span),
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

    fn build_all(&mut self) -> Result<InstanceBodies<'h>> {
        let init_inst = self.solved.init_inst;
        self.plan_stmts(&InstanceContext::plain(init_inst), &self.bodies.init)?;

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
            let func = self.plan_function(inst_id, inst, view)?;
            if inst.invocation == Invocation::Lexical
                || (!self.is_env_template(inst.template)
                    && (inst.invocation == Invocation::Inline || inst.captures_non_global))
            {
                self.plans.inline_callbacks.insert(func.id, inst.invocation);
            }
            new_functions.push(func);
        }

        let keep_unexported = new_functions.is_empty();
        for func in &self.bodies.functions {
            if new_functions.iter().any(|f| f.id == func.id) {
                continue;
            }
            let concrete = keep_unexported
                && func.type_params.is_empty()
                && func.params.iter().all(|p| !matches!(p.ty, HirType::Any))
                && !matches!(func.return_type, HirType::Any);
            if func.is_generator || concrete {
                new_functions.push(InstanceFunction::from(func));
            }
        }

        let mut new_classes = Vec::new();
        for (index, declaration) in self.bodies.classes.iter().enumerate() {
            if self.index.hir_class_is_literal.contains(index) {
                new_classes.push(InstanceClass {
                    declaration,
                    name: declaration.name.clone(),
                    fields: declaration.fields.iter().collect(),
                    constructor: declaration.constructor.as_ref().map(InstanceFunction::from),
                    methods: declaration
                        .methods
                        .iter()
                        .map(InstanceFunction::from)
                        .collect(),
                    static_methods: declaration
                        .static_methods
                        .iter()
                        .map(InstanceFunction::from)
                        .collect(),
                    getters: declaration
                        .getters
                        .iter()
                        .map(|(n, f)| (n.as_str(), InstanceFunction::from(f)))
                        .collect(),
                    setters: declaration
                        .setters
                        .iter()
                        .map(|(n, f)| (n.as_str(), InstanceFunction::from(f)))
                        .collect(),
                });
            }
        }
        let mut emitted_classes = BitSet::default();
        for (cinst_id, cinst) in self.solved.class_insts.iter_enumerated() {
            emitted_classes.insert(cinst.class.index());
            let Some(orig) = self
                .bodies
                .classes
                .get(self.index.class(cinst.class).hir_idx)
            else {
                continue;
            };
            if cinst.emitted_name != orig.name {
                self.plans
                    .class_origins
                    .insert(cinst.emitted_name.clone(), orig.name.clone());
            }
            new_classes.push(self.plan_class(
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
            let Some(orig) = self.bodies.classes.get(cinfo.hir_idx) else {
                continue;
            };
            let has_statics = !self.solved.class_static_methods[cid].is_empty();
            if has_statics || !orig.static_fields.is_empty() || orig.extends_name.is_some() {
                new_classes.push(self.plan_class(init_inst, cid, None, orig)?);
            }
        }
        Ok(InstanceBodies {
            init: &self.bodies.init,
            functions: new_functions,
            classes: new_classes,
        })
    }

    fn plan_class(
        &mut self,
        init_inst: InstanceId,
        cid: ClassId,
        instantiated: Option<(ClassInstId, &ClassInst)>,
        orig: &'h Class,
    ) -> Result<InstanceClass<'h>> {
        let init_ctx = InstanceContext::plain(init_inst);
        let name = instantiated
            .as_ref()
            .map_or(orig.name.as_str(), |(_, instance)| {
                instance.emitted_name.as_str()
            });
        let mut layout = super::ClassFields::default();
        let static_tys = &self.solved.class_static_field_tys[cid];
        for (index, field) in orig.static_fields.iter().enumerate() {
            if let Some(&ty) = static_tys.get(index)
                && self.is_resolved(ty)
            {
                self.prepare_layouts(ty)?;
                layout.statics.insert(field.name.clone(), ty);
            }
            if let Some(init) = &field.init {
                self.plan_expr(&init_ctx, init)?;
            }
        }
        let mut cls = InstanceClass {
            declaration: orig,
            name: name.to_owned(),
            fields: Vec::new(),
            constructor: None,
            methods: Vec::new(),
            static_methods: self.plan_methods(&self.solved.class_static_methods[cid])?,
            getters: Vec::new(),
            setters: Vec::new(),
        };
        let Some((cinst_id, cinst)) = instantiated else {
            cls.fields.extend(&orig.fields);
            self.plans.class_fields.insert(name.to_owned(), layout);
            return Ok(cls);
        };
        layout.initializer = cinst
            .field_init
            .map(|id| self.solved.instances[id].emitted_func_id);
        let field_locs = &self.solved.field_locs[cinst.field_locs];
        let mut fields = Vec::with_capacity(orig.fields.len());
        for (f_idx, field) in orig.fields.iter().enumerate() {
            let resolved = field_locs
                .get(f_idx)
                .map_or(Ty::OPEN, |&loc| self.solved.locations[loc]);
            if self.is_zero_sized_func(resolved) {
                continue;
            }
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
                self.plan_expr(&InstanceContext::plain(owner), init)?;
            }
            fields.push(field);
        }
        cls.fields = fields;
        cls.constructor = match cinst.ctor_inst {
            Some(ctor_id) => {
                let inst = &self.solved.instances[ctor_id];
                let view = self.index.view(inst.template);
                Some(self.plan_function(ctor_id, inst, view)?)
            }
            None => orig.constructor.as_ref().map(InstanceFunction::from),
        };
        let members = &self.solved.class_inst_members[cinst_id];
        cls.methods = self.plan_methods(&members.methods)?;
        cls.getters = self.plan_accessors(&members.getters, &orig.getters)?;
        cls.setters = self.plan_accessors(&members.setters, &orig.setters)?;
        self.plans.class_fields.insert(name.to_owned(), layout);
        Ok(cls)
    }

    fn plan_methods(&mut self, ids: &[InstanceId]) -> Result<Vec<InstanceFunction<'h>>> {
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            let inst = &self.solved.instances[id];
            let view = self.index.view(inst.template);
            let mut m = self.plan_function(id, inst, view)?;
            m.name = inst.emitted_method_name.clone();
            out.push(m);
        }
        Ok(out)
    }

    fn plan_accessors(
        &mut self,
        ids: &[(usize, InstanceId)],
        orig: &'h [(String, Function)],
    ) -> Result<Vec<(&'h str, InstanceFunction<'h>)>> {
        let mut out = Vec::with_capacity(ids.len());
        for &(acc_idx, id) in ids {
            let inst = &self.solved.instances[id];
            let view = self.index.view(inst.template);
            let f = self.plan_function(id, inst, view)?;
            out.push((orig.get(acc_idx).map_or(view.name, |(n, _)| n.as_str()), f));
        }
        Ok(out)
    }

    fn plan_function(
        &mut self,
        inst_id: InstanceId,
        inst: &Instance,
        view: &TemplateView<'h>,
    ) -> Result<InstanceFunction<'h>> {
        let mut params = Vec::with_capacity(view.params.len());
        let mut param_types = Vec::with_capacity(view.params.len());
        let mut static_func_params: Vec<(LocalId, FuncId)> = Vec::new();
        let lexical = inst.invocation == Invocation::Lexical;
        let is_env_fn = !lexical && self.is_env_template(inst.template);
        let self_ident = FuncIdentity {
            template: inst.template,
            capture_owner: inst.capture_owner,
        };
        let environment = is_env_fn.then_some(self_ident);
        if let Some(identity) = environment {
            self.environment(identity)?;
        }
        let receiver = match inst.receiver {
            Some(Receiver::Object(ty)) if !lexical => {
                self.prepare_layouts(ty)?;
                Some(ty)
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
                && !lexical
                && !self.is_env_template(ident.template)
            {
                static_func_params.push((param.id, self.find_func_identity_instance(ident)));
                continue;
            }
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
            let param_ctx = InstanceContext {
                inst_id,
                func_params: &static_func_params,
                void_return: false,
                has_expected: false,
            };
            if let Some(default) = &param.default {
                self.plan_expr(&param_ctx, default)?;
            }
            if self.is_boxed_local(param.id) {
                let ty = self.boxed_type(param.id, resolved)?;
                self.plans
                    .boxed_parameters
                    .insert((inst.emitted_func_id, param.id), ty);
            }
            params.push(param);
        }

        let ret_ty = self.solved.locations[inst.return_loc];
        let void_return = !lexical && self.is_zero_sized_func(ret_ty);
        let result = if void_return {
            super::InstanceResult::ErasedFunction
        } else if self.is_resolved(ret_ty) {
            self.prepare_layouts(ret_ty)?;
            super::InstanceResult::Value(ret_ty)
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
            super::InstanceResult::Source
        };
        self.plans.signatures.insert(
            inst.emitted_func_id,
            super::InstanceSignature {
                environment,
                receiver,
                params: param_types,
                result,
            },
        );
        let scope = InstanceContext {
            inst_id,
            func_params: &static_func_params,
            void_return,
            has_expected: false,
        };
        self.plan_stmts(&scope, view.body)?;
        let captures = if is_env_fn {
            Vec::new()
        } else {
            let mut caps = view.captures.to_vec();
            caps.extend(self.solved.inst_captures[inst_id].iter().copied());
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

        Ok(InstanceFunction {
            id: inst.emitted_func_id,
            name: inst.emitted_name.clone(),
            params,
            return_type: view.return_type,
            body: view.body,
            captures,
            is_async: view.is_async,
            is_generator: view.is_generator,
            is_exported: inst.is_witness,
        })
    }

    fn find_func_identity_instance(&self, ident: FuncIdentity) -> FuncId {
        self.solved
            .instances
            .iter_enumerated()
            .find_map(|(id, inst)| {
                (self.solved.live_instances.contains(id.index())
                    && inst.invocation != Invocation::Lexical
                    && inst.template == ident.template
                    && inst.capture_owner == ident.capture_owner)
                    .then_some(inst.emitted_func_id)
            })
            .unwrap_or_else(|| self.index.view(ident.template).func_id)
    }
}
