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
    CaptureOwner, CellKind, ClassId, ClassInstId, FuncIdentity, Idx, IdxVec, InstanceId, Ty,
    TyKind, TypeTable,
};
use perry_hir::{
    ir::{Class, Expr, Function},
    types::{FuncId, LocalId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};
use pliron::context::Context;

pub(super) fn build<'h>(
    bodies: &'h Bodies,
    declarations: &ModuleDeclarations,
    locations: &mut crate::hir_compat::Locations<'_>,
    index: &ModuleIndex<'h>,
    types: &TypeTable,
    solved: &Solved,
    layouts: (&Context, &mut perry_mir::RecordLayouts),
) -> Result<(
    InstanceBodies<'h>,
    super::InstancePlans,
    super::plans::Pending,
)> {
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
        next_static_id: 0x8000_0000,
        next_func_id: index.max_func_id.max(max_inst_fid).saturating_add(1),
        layouts,

        native: Default::default(),
        plans: super::InstancePlans {
            functions: solved.instances.map(|inst| inst.emitted_func_id),
            helpers: solved.helpers.filled(None),
            ..Default::default()
        },
        decisions: Default::default(),
    };
    let bodies = planner.build_all()?;
    Ok((bodies, planner.plans, planner.decisions))
}

struct InstancePlanner<'h, 'a, 's> {
    bodies: &'h Bodies,
    declarations: &'a ModuleDeclarations,
    locations: &'a mut crate::hir_compat::Locations<'s>,
    index: &'a ModuleIndex<'h>,
    types: &'a TypeTable,
    solved: &'a Solved,
    next_func_id: FuncId,
    next_static_id: LocalId,
    layouts: (&'a Context, &'a mut perry_mir::RecordLayouts),

    native: Vec<(FuncId, super::NativeFunction)>,
    plans: super::InstancePlans,
    decisions: super::plans::Pending,
}

impl<'h> InstancePlanner<'h, '_, '_> {
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

    fn local_ty(&self, inst_id: InstanceId, id: LocalId) -> Option<Ty> {
        let lidx = self.index.local_idx(id)?;
        let info = self.index.locals[lidx];
        if let Some(slot) = info.frame_slot()
            && info.owner == self.solved.instances[inst_id].template
        {
            return self.solved.inst_frame_locals[inst_id].get(slot).copied();
        }
        self.local_ty_in_owner(CaptureOwner::Instance(inst_id), id)
    }

    fn build_all(&mut self) -> Result<InstanceBodies<'h>> {
        let init_inst = self.solved.init_inst;
        self.prepare_instance(init_inst)?;

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

        let mut new_classes = IdxVec::default();
        let mut emitted_classes = BitSet::default();
        for (cinst_id, cinst) in self.solved.class_insts.iter_enumerated() {
            emitted_classes.insert(cinst.class.index());
            let orig = self.index.class(cinst.class).declaration;
            new_classes.push(self.plan_class(cinst.class, Some((cinst_id, cinst)), orig)?);
        }
        for (cid, cinfo) in self.index.classes.iter_enumerated() {
            if emitted_classes.contains(cid.index()) {
                continue;
            }
            let orig = cinfo.declaration;
            let has_statics = !self.solved.class_static_methods[cid].is_empty();
            if has_statics || !orig.static_fields.is_empty() || orig.extends_name.is_some() {
                new_classes.push(self.plan_class(cid, None, orig)?);
            }
        }
        let mut exports = Vec::with_capacity(self.solved.witness_instances.len());
        for (i, root) in self.solved.witness_instances.iter().enumerate() {
            use crate::engine::{NativeBody, NativeFunction, NativeParam, solve::ExportRoot};
            exports.push(match root {
                None => None,
                Some(ExportRoot::Function(id)) => Some(self.plans.function(*id)),
                Some(ExportRoot::Adapter {
                    target,
                    params,
                    result,
                }) => {
                    let id = self.fresh_func_id()?;
                    let name = crate::hir_compat::adapter_name(i);
                    if let crate::hir_compat::Target::Function(id) = target {
                        let template = self
                            .index
                            .view(self.solved.instances[*id].template)
                            .raw_name();
                        self.locations
                            .record_instantiation(name.clone(), template, 0, None);
                    }
                    self.native.push((
                        id,
                        NativeFunction {
                            name,
                            params: self
                                .types
                                .tuple_slice(*params)
                                .iter()
                                .copied()
                                .map(NativeParam::Value)
                                .collect(),
                            result: self.solved.locations[*result],
                            is_async: false,
                            body: NativeBody::Resource(
                                target.try_map(|id| Ok(self.plans.function(id)))?,
                            ),
                        },
                    ));
                    Some(id)
                }
            });
        }
        self.native.sort_unstable_by_key(|&(id, _)| id);
        Ok(InstanceBodies {
            exports,
            native: std::mem::take(&mut self.native),
            init: &self.bodies.init,
            functions: new_functions,
            classes: new_classes,
        })
    }

    fn plan_class(
        &mut self,
        cid: ClassId,
        instantiated: Option<(ClassInstId, &ClassInst)>,
        orig: &'h Class,
    ) -> Result<InstanceClass<'h>> {
        let name = instantiated
            .as_ref()
            .map_or(orig.name.as_str(), |(_, instance)| {
                instance.emitted_name.as_str()
            });
        let mut static_fields = Vec::with_capacity(orig.static_fields.len());
        let static_tys = &self.solved.class_static_field_tys[cid];
        for (index, field) in orig.static_fields.iter().enumerate() {
            let ty = static_tys
                .get(index)
                .copied()
                .filter(|&ty| self.is_resolved(ty));
            if let Some(ty) = ty {
                self.prepare_layouts(ty)?;
            }
            static_fields.push(crate::source::StaticField {
                id: self.next_static_id,
                field,
                ty,
            });
            self.next_static_id += 1;
        }
        let mut cls = InstanceClass {
            ty: instantiated.map_or(self.index.class(cid).ty, |(_, instance)| instance.ty),
            declaration: orig,
            name: name.to_owned(),
            fields: Vec::new(),
            static_fields,
            initializer: None,
            constructor: None,
            methods: Vec::new(),
            static_methods: self.plan_methods(&self.solved.class_static_methods[cid])?,
            getters: Vec::new(),
            setters: Vec::new(),
        };
        let Some((cinst_id, cinst)) = instantiated else {
            cls.fields
                .extend(orig.fields.iter().map(|field| (field, None)));
            return Ok(cls);
        };
        cls.initializer = cinst
            .field_init
            .map(|id| self.solved.instances[id].emitted_func_id);
        let field_locs = &self.solved.field_locs[cinst.field_locs];
        cls.fields.reserve(orig.fields.len());
        for (f_idx, field) in orig.fields.iter().enumerate() {
            let resolved = field_locs
                .get(f_idx)
                .map_or(Ty::OPEN, |&loc| self.solved.locations[loc]);
            if self.is_zero_sized_func(resolved) {
                continue;
            }
            let ty = self.is_resolved(resolved).then_some(resolved);
            if let Some(resolved) = ty {
                self.prepare_layouts(resolved)?;
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
            cls.fields.push((field, ty));
        }
        if let Some(owner) = cinst.field_init {
            self.prepare_instance(owner)?;
        }
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
        Ok(cls)
    }

    fn plan_methods(&mut self, ids: &[InstanceId]) -> Result<Vec<InstanceFunction<'h>>> {
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            let inst = &self.solved.instances[id];
            let view = self.index.view(inst.template);
            let mut m = self.plan_function(id, inst, view)?;
            m.name = if inst.emitted_method_name == view.name {
                view.name.into()
            } else {
                inst.emitted_method_name.clone().into()
            };
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
        let lexical = inst.invocation == Invocation::Lexical;
        let is_env_fn = !lexical && self.is_env_template(inst.template);
        let self_ident = inst.identity();
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
                self.local_storage(
                    inst_id,
                    param.id,
                    super::Storage::ErasedFunction(Some(self.find_func_identity_instance(ident))),
                )?;
                continue;
            }
            let resolved = self.is_resolved(param_ty).then_some(param_ty);
            if resolved.is_some() {
                self.prepare_layouts(param_ty)?;
            } else if !self.index.is_param_annotated(param.id)
                && !inst.is_witness
                && matches!(param.ty, HirType::Any | HirType::Unknown)
            {
                let err = diag!(codes::MISSING_TYPE_ANNOTATION, "cannot infer the type of parameter '{}' of '{}'", param.name, view.display_name(self.locations))
                    .with_help("Add a concrete type annotation to this binding, parameter, or function return.");
                let err = self.annotate_local(err, param.id);
                return Err(self.attach_inst_chain(err, inst_id));
            }
            if self.is_boxed_local(param.id) {
                let ty = self.boxed_type(param.id, resolved)?;
                self.local_storage(
                    inst_id,
                    param.id,
                    super::Storage::Boxed {
                        ty,
                        init: super::CellInit::AtDeclaration,
                    },
                )?;
            }
            params.push((param, resolved));
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
                view.display_name(self.locations)
            )
            .with_help("Add a concrete return type annotation to this function.");
            let err = match self.locations.declaration_span(&view.raw_name()) {
                Some(span) => err.with_span(span),
                None => err,
            };
            return Err(self.attach_inst_chain(err, inst_id));
        } else if let Some(partial) = inst.partial_record_result {
            super::InstanceResult::Value(partial)
        } else {
            super::InstanceResult::Source
        };
        self.prepare_instance(inst_id)?;
        if !inst.is_witness
            && let Some(caller) = inst.caller
        {
            let caller = &self.solved.instances[caller];
            self.locations.record_instantiation(
                inst.emitted_name.clone(),
                view.raw_name(),
                inst.call_byte_offset,
                (caller.template != super::types::TemplateId::INIT)
                    .then(|| caller.emitted_name.clone()),
            );
        }

        let captures_frame = inst.capture_owner.is_some() || self.types.tuple_slice(inst.args).iter().any(|&arg| {
            matches!(self.types.kind(arg), TyKind::Func(id) if !self.index.template_captures(id.template).is_empty())
        });
        let inline = (matches!(view.kind, TemplateKind::Function | TemplateKind::Closure)
            && (inst.invocation == Invocation::Lexical
                || (!self.is_env_template(inst.template)
                    && (inst.invocation == Invocation::Inline || captures_frame))))
            .then_some(inst.invocation);
        Ok(InstanceFunction {
            signature: Some(super::InstanceSignature {
                inline,
                environment,
                receiver,
                result,
            }),
            id: inst.emitted_func_id,
            name: if inst.emitted_name == view.name {
                view.name.into()
            } else {
                inst.emitted_name.clone().into()
            },
            params,
            return_type: view.return_type,
            body: view.body,
            captures: if is_env_fn { &[] } else { view.captures },
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
                    && inst.identity() == ident)
                    .then_some(inst.emitted_func_id)
            })
            .unwrap_or_else(|| self.index.view(ident.template).func_id)
    }
}
