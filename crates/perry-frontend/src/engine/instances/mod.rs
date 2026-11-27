//! Bind solved instances to borrowed source declarations and native MIR operation plans.
use crate::source::{Bodies, InstanceBodies, InstanceClass, InstanceFunction, ModuleDeclarations};
mod async_helpers;
mod expressions;
mod funcs;
mod stmts;
use super::BodyOwner;
use super::bitset::BitSet;
use super::boundary::{LocalFlags, ModuleIndex, Origins, TemplateKind};
use super::solve::Invocation;
use super::solve::{ClassInst, Receiver, SiteEntry, Solved};
use super::types::{
    CaptureOwner, CellKind, ClassId, Idx, IdxVec, InstanceId, Ty, TyKind, TypeTable,
};
use crate::source::CallerKind;
use perry_hir::types::{FuncId, LocalId, Type as HirType};
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};
use pliron::context::Context;

pub(super) fn build<'h>(
    bodies: &'h Bodies,
    declarations: &ModuleDeclarations,
    locations: &mut crate::hir_compat::Locations<'_>,
    index: &mut ModuleIndex<'h>,
    types: &TypeTable,
    solved: &Solved,
    layouts: (&Context, &mut perry_mir::RecordLayouts),
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
        layouts,

        origins: index.views.filled(Vec::new()),
        native: Default::default(),
        plans: super::InstancePlans {
            functions: solved.instances.map(|inst| inst.emitted_func_id),
            helpers: solved.helpers.filled(None),
            ..Default::default()
        },
        locals: solved.instances.filled(Default::default()),
    };
    let bodies = planner.build_all()?;
    planner.finalize_sites()?;
    let InstancePlanner {
        mut plans, origins, ..
    } = planner;
    plans.decisions.expressions = std::mem::take(&mut index.expressions);
    plans.local_ids = std::mem::take(&mut index.local_to_idx);
    plans.local_flags = index.locals.map(|local| local.flags);
    plans.globals = index.emitted_uses(&bodies, origins, &plans, locations);
    Ok((bodies, plans))
}

struct InstancePlanner<'h, 'a, 's> {
    bodies: &'h Bodies,
    declarations: &'a ModuleDeclarations,
    locations: &'a mut crate::hir_compat::Locations<'s>,
    index: &'a ModuleIndex<'h>,
    types: &'a TypeTable,
    solved: &'a Solved,
    origins: Origins,
    next_func_id: FuncId,
    layouts: (&'a Context, &'a mut perry_mir::RecordLayouts),

    native: Vec<(FuncId, super::NativeFunction)>,
    plans: super::InstancePlans,
    locals: IdxVec<InstanceId, std::collections::BTreeMap<super::types::LocalIdx, super::Storage>>,
}

impl<'h> InstancePlanner<'h, '_, '_> {
    fn is_resolved(&self, ty: Ty) -> bool {
        !self.types.is_open_with(
            ty,
            |loc| (loc, self.solved.locations[loc]),
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

    fn local_ty(&self, inst_id: InstanceId, lidx: super::types::LocalIdx) -> Option<Ty> {
        let info = self.index.locals[lidx];
        if let Some(slot) = info.frame_slot()
            && info.owner == self.solved.instances[inst_id].template
        {
            return self.solved.inst_frame_locals[inst_id].get(slot).copied();
        }
        self.local_ty_in_owner(CaptureOwner::Instance(inst_id), lidx)
    }

    fn build_all(&mut self) -> Result<InstanceBodies<'h>> {
        let init_inst = self.solved.init_inst;
        self.prepare_bindings(init_inst)?;

        let mut new_functions = Vec::new();
        for (inst_id, inst) in self.solved.instances.iter_enumerated() {
            if !self.solved.live_instances.contains(inst_id.index()) {
                continue;
            }
            let view = self.index.view(inst.template);
            let (kind, caller) = match view.kind {
                TemplateKind::ModuleInit => (CallerKind::Initializer, None),
                TemplateKind::Closure(_) | TemplateKind::Function => (
                    if inst.is_witness {
                        CallerKind::Export
                    } else {
                        CallerKind::Function
                    },
                    Some(inst.emitted_func_id),
                ),
                _ => (CallerKind::Class, None),
            };
            self.origins[inst.template].push((self.solved.body_owner(inst_id), kind, caller));
            match view.kind {
                TemplateKind::PromiseResolve(_) | TemplateKind::PromiseReject(_) => {
                    self.promise_handle_function(inst, view.kind)?
                }
                TemplateKind::Function
                | TemplateKind::Closure(_)
                | TemplateKind::ClassStaticMethod(_)
                | TemplateKind::ClassMethod
                | TemplateKind::ClassGetter
                | TemplateKind::ClassSetter => {
                    new_functions.push(self.plan_function(inst_id, None)?);
                }
                _ => {}
            }
        }
        if let Some(f) = self.bodies.functions.iter().find(|f| f.is_generator) {
            let name = self.locations.display_function_name(&f.name);
            return Err(self.locations.annotate_caller(
                Diagnostic::unsupported(format!("generator function {name} is not supported yet"))
                    .with_fallback_span(self.locations.function_span(&f.name)),
                &f.name,
            ));
        }
        let mut static_fields = std::collections::BTreeMap::new();
        for class in self.index.classes.iter() {
            for (field, loc) in class.static_fields() {
                let value = self.solved.locations[loc];
                let ty = self.is_resolved(value).then_some(value);
                let id = 0x8000_0000 + static_fields.len() as LocalId;
                static_fields
                    .entry((class.declaration.name.as_str(), field.name.as_str()))
                    .or_insert(crate::source::StaticField { id, field, ty });
            }
        }
        let mut new_classes = IdxVec::default();
        let mut emitted_classes = BitSet::default();
        for cinst in self.solved.class_insts.iter() {
            emitted_classes.insert(cinst.class.index());
            new_classes.push(self.plan_class(cinst.class, Some(cinst))?);
        }
        for (id, class) in self.index.classes.iter_enumerated() {
            let decl = class.declaration;
            if !emitted_classes.contains(id.index())
                && (!decl.static_fields.is_empty() || decl.extends_name.is_some())
            {
                new_classes.push(self.plan_class(id, None)?);
            }
        }
        for id in self.solved.helpers.ids() {
            if self.solved.live_helpers.contains(id.index()) {
                self.prepare_helper(id)?;
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
                            result: match *result {
                                super::solve::AdapterResult::Declared(ty) => ty,
                                super::solve::AdapterResult::Inferred(loc) => {
                                    self.solved.locations[loc]
                                }
                            },
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
            static_fields,
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
        instantiated: Option<&ClassInst>,
    ) -> Result<InstanceClass<'h>> {
        let class = self.index.class(cid);
        let orig = class.declaration;
        if instantiated.and_then(|i| i.field_init).is_none()
            && let Some(tid) = class.field_init
            && self.origins[tid].is_empty()
        {
            self.origins[tid].push((BodyOwner::Initializer, CallerKind::Class, None));
        }
        for member in &orig.computed_members {
            let id = member.function.id;
            if let Some(tid) = self.index.template_of_func(id) {
                self.origins[tid].push((BodyOwner::Function(id), CallerKind::Class, None));
            }
        }
        let name = instantiated
            .as_ref()
            .map_or(orig.name.as_str(), |instance| {
                instance.emitted_name.as_str()
            });
        let mut cls = InstanceClass {
            ty: instantiated.map_or(class.ty, |instance| instance.ty),
            declaration: orig,
            name: name.to_owned(),
            fields: Vec::new(),
            initializer: None,
            constructor: None,
            direct_constructor: false,
        };
        let Some(cinst) = instantiated else {
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
            if ty.is_none()
                && matches!(
                    self.types.kind(resolved),
                    TyKind::Cell {
                        kind: CellKind::Array,
                        ..
                    }
                )
            {
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
        if let (Some(initializer), Some(_)) = (cinst.field_init, cinst.ctor_inst) {
            self.prepare_bindings(initializer)?;
        }
        cls.constructor = cinst
            .ctor_inst
            .or(cinst.field_init)
            .map(|id| self.plan_function(id, Some(cls.ty)))
            .transpose()?;
        cls.direct_constructor = class.direct_constructor && cls.fields.len() == orig.fields.len();
        Ok(cls)
    }

    fn plan_function(
        &mut self,
        inst_id: InstanceId,
        constructed: Option<Ty>,
    ) -> Result<InstanceFunction<'h>> {
        let inst = &self.solved.instances[inst_id];
        let view = self.index.view(inst.template);
        let lexical = inst.invocation == Invocation::Lexical;
        let is_env_fn = !lexical && self.is_env_template(inst.template);
        let self_ident = inst.identity();
        let environment = is_env_fn.then_some(self_ident);
        if let Some(identity) = environment {
            self.environment(identity)?;
        }
        let receiver = match inst.receiver {
            Some(Receiver::Class(id)) if !lexical && constructed.is_none() => {
                Some(self.solved.class_insts[id].ty)
            }
            Some(Receiver::Object(ty)) if !lexical => Some(ty),
            _ => None,
        };

        for (param, &param_loc) in view
            .params
            .iter()
            .zip(&self.solved.capture_locs.slots[inst.activation])
        {
            let local = self.index.param_local(param)?;
            let param_ty = self.solved.locations[param_loc];
            if !lexical
                && let Some(ident) = self
                    .types
                    .erased_function(param_ty, &self.solved.escaping_env_templates)
            {
                self.locals[inst_id].insert(
                    local,
                    super::Storage::ErasedFunction(Some(self.index.view(ident.template).func_id)),
                );
                continue;
            }
            let resolved = self.is_resolved(param_ty).then_some(param_ty);
            if resolved.is_none()
                && !self.index.is_param_annotated(param.id)
                && !inst.is_witness
                && matches!(param.ty, HirType::Any | HirType::Unknown)
            {
                let err = diag!(codes::MISSING_TYPE_ANNOTATION, "cannot infer the type of parameter '{}' of '{}'", param.name, view.display_name(self.locations))
                    .with_help("Add a concrete type annotation to this binding, parameter, or function return.");
                let err = self.annotate_local(err, param.id);
                return Err(self.attach_inst_chain(err, inst_id));
            }
            let ty = resolved.unwrap_or(self.index.locals[local].declared);
            let storage = if self.solved.boxed_locals.contains(local.index()) {
                super::Storage::Boxed {
                    ty,
                    init: super::CellInit::AtDeclaration,
                }
            } else {
                super::Storage::Value(ty)
            };
            self.locals[inst_id].insert(local, storage);
        }

        let ret_ty = self.solved.locations[inst.return_loc];
        let void_return = !lexical && self.is_zero_sized_func(ret_ty);
        let result = if let Some(class) = constructed {
            super::InstanceResult::Value(class)
        } else if void_return {
            super::InstanceResult::ErasedFunction
        } else if self.is_resolved(ret_ty) {
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
        self.prepare_bindings(inst_id)?;
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
        let inline = (constructed.is_none()
            && (inst.invocation == Invocation::Lexical
                || (!self.is_env_template(inst.template)
                    && (inst.invocation == Invocation::Inline || captures_frame))))
            .then_some(inst.invocation);
        Ok(InstanceFunction {
            template: inst.template,
            inline,
            environment,
            receiver,
            result,
            id: inst.emitted_func_id,
            name: if inst.emitted_name == view.name {
                view.name.clone()
            } else {
                inst.emitted_name.clone().into()
            },
            is_exported: inst.is_witness,
        })
    }
}
