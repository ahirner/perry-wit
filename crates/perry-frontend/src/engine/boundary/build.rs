//! Assemble the lexical index and close over capture, frame and promise sites.
use super::*;

fn assigns_constructor_arguments(class: &perry_hir::ir::Class) -> bool {
    let Some(ctor) = &class.constructor else {
        return false;
    };
    ctor.params.len() == class.fields.len()
        && ctor.body.len() == class.fields.len()
        && ctor
            .body
            .iter()
            .zip(&ctor.params)
            .zip(&class.fields)
            .all(|((stmt, param), field)| {
                let Stmt::Expr(Expr::PropertySet {
                    object,
                    property,
                    value,
                    ..
                }) = stmt
                else {
                    return false;
                };
                field.init.is_none()
                    && param.default.is_none()
                    && !param.is_rest
                    && matches!(object.as_ref(), Expr::This)
                    && property == &field.name
                    && matches!(value.as_ref(), Expr::LocalGet(id) if *id == param.id)
            })
}

impl<'h> ModuleIndex<'h> {
    pub(crate) fn build(
        ctx: &Context,
        bodies: &'h crate::source::Bodies,
        declarations: &'h crate::source::ModuleDeclarations,
        locations: &crate::hir_compat::Locations<'_>,
        closure_names: &'h HashMap<FuncId, String>,
        (constructors, decls): (
            &crate::source::Constructors,
            &crate::shapes::Declarations<'h>,
        ),
        types: &mut TypeTable,
    ) -> Self {
        let max_local_hint = declarations.locals.keys().copied().max().unwrap_or(0);
        let mut idx = Self {
            functions: LexicalFunctions(Vec::with_capacity(bodies.functions.len() + 1)),
            local_to_idx: vec![None; max_local_hint as usize + 1],
            max_local_id: max_local_hint,
            ..Self::default()
        };
        let init_tid = idx.add_template(TemplateView {
            kind: TemplateKind::ModuleInit,
            name: "<init>".into(),
            func_id: 0,
            params: &[],
            return_type: &VOID_HIR,
            body: &bodies.init,
            captures: &[],
            type_params: &[],
            is_async: declarations.has_top_level_await,
            is_generator: false,

            captures_this: false,
            captures_new_target: false,
            uses_this: false,
            explicit_return: false,
        });
        debug_assert_eq!(init_tid, TemplateId::INIT);
        let mut raw_sites: Vec<Vec<&'h Expr>> = vec![Vec::new()];
        let mut defined_locals = BitSet::default();
        let mut pending_captures: Vec<(LocalId, bool, bool)> = Vec::new();
        let mut scan = ScanCtx {
            constructors,
            decls,
            locations,
            regexes: Default::default(),
            ctx,
            declarations,
            closure_names,
            types,
            raw_sites: &mut raw_sites,
            bindings: Vec::new(),
            defined_locals: &mut defined_locals,
            pending_captures: &mut pending_captures,
        };
        idx.scan_stmts(&mut scan, &bodies.init, init_tid, true, false);

        for f in &bodies.functions {
            idx.register_member(
                &mut scan,
                TemplateKind::Function,
                f,
                locations.has_explicit_return(&f.name),
            );
        }

        for class in &bodies.classes {
            if hir_compat::is_object_literal(&class.name) {
                idx.literal_classes.insert(&class.name, &class.fields);
                continue;
            }
            let cid = idx.classes.push(ClassInfo {
                declaration: class,
                ty: scan.types.named(scan.ctx, &class.name),
                name_id: scan.types.intern_name(&class.name),
                field_init: None,
                ctor: None,
                direct_constructor: assigns_constructor_arguments(class),
                static_field_locs: Vec::new(),
            });
            idx.register_type_params(scan.ctx, GenericOwner::Class(cid), &class.type_params);
            for field in &class.static_fields {
                let ty = idx.annotations.register(&mut scan, &field.ty, true);
                let loc = idx.add_location(if ty == Ty::OPEN { Ty::BOTTOM } else { ty });
                idx.classes[cid]
                    .static_field_locs
                    .push((scan.types.intern_name(&field.name), loc));
            }
            let mut initializers = class
                .fields
                .iter()
                .filter_map(|field| field.init.as_ref())
                .peekable();
            if class.constructor.is_none() || initializers.peek().is_some() {
                let owner =
                    idx.add_synthetic_template(TemplateKind::FieldInitializers, &class.name);
                idx.classes[cid].field_init = Some(owner);
                scan.raw_sites.resize_with(idx.views.len(), Vec::new);
                for init in initializers {
                    Self::record_site_expr(&mut scan, owner, init);
                    idx.scan_expr(&mut scan, init, owner, false);
                }
            }
            if class
                .constructor
                .iter()
                .chain(&class.methods)
                .flat_map(|f| &f.params)
                .any(|p| idx.param_is_unannotated(scan.declarations, p))
            {
                idx.class_has_implicit_params.insert(cid.index());
            }
            idx.classes[cid].ctor = class
                .constructor
                .as_ref()
                .map(|c| idx.register_member(&mut scan, TemplateKind::ClassConstructor, c, false));
            let mut register = |kind, function, prefix: &str, name: &str| {
                let name = format!("{}::{prefix}{name}", class.name);
                let tid = idx.register_member(
                    &mut scan,
                    kind,
                    function,
                    locations.has_explicit_return(&name),
                );
                idx.views[tid].name = name.into();
            };
            for method in &class.methods {
                register(TemplateKind::ClassMethod, method, "", &method.name);
            }
            for method in &class.static_methods {
                register(
                    TemplateKind::ClassStaticMethod(cid),
                    method,
                    "",
                    &method.name,
                );
            }
            for (name, getter) in &class.getters {
                register(TemplateKind::ClassGetter, getter, "get_", name);
            }
            for (name, setter) in &class.setters {
                register(TemplateKind::ClassSetter, setter, "set_", name);
            }
            for member in &class.computed_members {
                idx.register_member(
                    &mut scan,
                    TemplateKind::ClassMethod,
                    &member.function,
                    false,
                );
            }
        }

        idx.regexes = scan.regexes.finish(&idx.expressions);
        scan.bindings.sort_by_key(|(owner, _)| *owner);
        for group in scan.bindings.chunk_by(|a, b| a.0 == b.0) {
            if let Some((owner, _)) = group.first() {
                idx.layouts[*owner].bindings =
                    idx.bindings.extend(group.iter().map(|(_, local)| *local));
            }
        }
        raw_sites.resize_with(idx.views.len(), Vec::new);
        for (template, list) in idx.views.ids().zip(raw_sites) {
            let mut sites: Vec<_> = list
                .into_iter()
                .map(|expr| IndexedSite {
                    expr,
                    index: idx.expressions.register(expr, constructors),
                    handles: None,
                })
                .collect();
            sites.sort_unstable_by_key(|site| site.index);
            sites.dedup_by_key(|site| site.index);
            for (site, entry) in sites.iter_mut().enumerate() {
                if matches!(entry.expr, Expr::New { class_name, .. } if class_name == "Promise") {
                    entry.handles = Some((
                        idx.add_synthetic_template(TemplateKind::PromiseResolve(site), "resolve"),
                        idx.add_synthetic_template(TemplateKind::PromiseReject(site), "reject"),
                    ));
                }
            }
            idx.layouts[template].sites = idx.site_pool.extend_from_slice(&sites);
        }

        for (cap_id, written, forward) in pending_captures {
            if let Some(lidx) = idx.local_idx(cap_id) {
                let flags = &mut idx.locals[lidx].flags;
                flags.insert(LocalFlags::CAPTURED);
                if written {
                    flags.insert(LocalFlags::WRITTEN_FROM_CLOSURE);
                }
                if forward && !flags.intersects(LocalFlags::MODULE_OR_PARAM) {
                    flags.insert(LocalFlags::FORWARD_CAPTURED);
                }
            }
        }
        let mut captured_locals = idx.views.filled(Vec::new());
        for lidx in idx.locals.ids() {
            let local = &mut idx.locals[lidx];
            if !local.flags.intersects(LocalFlags::MODULE_OR_PARAM) {
                if !local.flags.contains(LocalFlags::CAPTURED) {
                    let count = &mut idx.layouts[local.owner].frame_slots;
                    local.storage = LocalStorage::Frame(*count);
                    *count += 1;
                } else {
                    captured_locals[local.owner].push(lidx);
                }
            }
        }
        for (tid, list) in captured_locals.iter_enumerated() {
            if !list.is_empty() {
                idx.layouts[tid].captured_locals = idx.captured_local_pool.extend_from_slice(list);
            }
        }
        let mut template_captures = idx.views.map(|v| {
            v.captures
                .iter()
                .copied()
                .filter(|&c| idx.is_non_global_local(c))
                .collect::<Vec<_>>()
        });
        for (tid, view) in idx.views.iter_enumerated() {
            for &cap_id in view.captures {
                let Some(lidx) = idx.local_idx(cap_id) else {
                    continue;
                };
                let local = idx.locals[lidx];
                if local.flags.contains(LocalFlags::MODULE) {
                    continue;
                }
                let cap_owner = local.owner;
                let mut cur = idx.layouts[tid].closure_owner;
                while let Some(parent) = cur {
                    if parent == cap_owner || parent == TemplateId::INIT {
                        break;
                    }
                    template_captures[parent].push(cap_id);
                    cur = idx.layouts[parent].closure_owner;
                }
            }
        }
        for (tid, view) in idx.views.iter_enumerated() {
            let caps = &mut template_captures[tid];
            caps.sort_unstable();
            caps.dedup();
            let is_promise_handle = matches!(
                view.kind,
                TemplateKind::PromiseResolve(_) | TemplateKind::PromiseReject(_)
            );
            if caps.is_empty() && !is_promise_handle {
                idx.capture_free_templates.insert(tid.index());
            } else if !caps.is_empty() {
                idx.layouts[tid].captures = idx.capture_pool.extend_from_slice(caps);
            }
        }
        idx
    }
}

impl ModuleIndex<'_> {
    pub(crate) fn emitted_uses(
        &self,
        bodies: &crate::source::InstanceBodies<'_>,
        solved: &super::super::solve::Solved,
        plans: &super::super::InstancePlans,
        locations: &mut hir_compat::Locations,
    ) -> std::collections::HashSet<LocalId> {
        use crate::engine::BodyOwner;
        use crate::source::CallerKind;
        let mut origins = self.views.filled(Vec::new());
        for (id, inst) in solved
            .instances
            .iter_enumerated()
            .filter(|(id, _)| solved.live_instances.contains(id.index()))
        {
            let (kind, name) = match self.view(inst.template).kind {
                TemplateKind::ModuleInit => (CallerKind::Initializer, None),
                TemplateKind::Closure | TemplateKind::Function => (
                    if inst.is_witness {
                        CallerKind::Export
                    } else {
                        CallerKind::Function
                    },
                    Some(inst.emitted_name.as_str()),
                ),
                _ => (CallerKind::Class, None),
            };
            origins[inst.template].push((solved.body_owner(id), kind, name));
        }
        let mut source = |id, kind, name| {
            if let Some(tid) = self.template_of_func(id) {
                origins[tid].push((BodyOwner::Function(id), kind, name));
            }
        };
        let mut field_initializers = std::collections::HashSet::new();
        for c in bodies.classes.iter() {
            if c.initializer.is_none() {
                field_initializers.insert(c.declaration.name.as_str());
            }
            for m in &c.declaration.computed_members {
                source(m.function.id, CallerKind::Class, None);
            }
        }
        let names: HashMap<_, _> = bodies
            .functions
            .iter()
            .filter(|_| locations.has_imports())
            .map(|f| (f.id, f.name.as_ref()))
            .collect();
        for (tid, view) in self.views.iter_enumerated() {
            if origins[tid].is_empty() {
                if view.kind == TemplateKind::FieldInitializers
                    && field_initializers.contains(view.name.as_ref())
                {
                    origins[tid].push((BodyOwner::Initializer, CallerKind::Class, None));
                } else if let Some(parent) = self.closure_owner(tid)
                    && let Some(&(_, kind, name)) = origins[parent].first()
                {
                    origins[tid].push((BodyOwner::Function(view.func_id), kind, name));
                }
            }
            if !names.is_empty() {
                for &(owner, kind, caller) in &origins[tid] {
                    for site in self.sites(tid) {
                        if let Expr::Call { byte_offset, .. } = site.expr
                            && let Some(callee) = plans
                                .call_target(owner, site.expr)
                                .and_then(|id| names.get(&id))
                        {
                            locations.record_cross_file_call(callee, *byte_offset, caller, kind);
                        }
                    }
                }
            }
        }
        self.global_uses
            .iter()
            .filter_map(|&(template, id)| {
                (origins[template]
                    .iter()
                    .any(|(_, kind, _)| *kind != CallerKind::Initializer)
                    && self
                        .local_idx(id)
                        .is_some_and(|i| self.locals[i].flags.contains(LocalFlags::MODULE)))
                .then_some(id)
            })
            .collect()
    }
}
