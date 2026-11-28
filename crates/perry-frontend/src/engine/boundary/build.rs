//! Assemble the lexical index and close over capture, frame and promise sites.
use super::*;

impl<'h> ModuleIndex<'h> {
    pub(crate) fn build(
        ctx: &Context,
        bodies: &'h crate::source::Bodies,
        declarations: &'h crate::source::ModuleDeclarations,
        locations: &crate::hir_compat::Locations<'_>,
        decls: &crate::shapes::Declarations<'h>,
        types: &mut TypeTable,
    ) -> Self {
        let local_ids = declarations.locals.len().max(1);
        let mut idx = Self {
            functions: Vec::with_capacity(bodies.functions.len() + 1),
            ..Self::default()
        };
        idx.locals.ids.resize(local_ids, None);
        let init_tid = idx.add_template(TemplateView {
            body: &bodies.init,
            is_async: declarations.has_top_level_await,
            ..TemplateView::synthetic(TemplateKind::ModuleInit, "<init>")
        });
        debug_assert_eq!(init_tid, TemplateId::INIT);
        let mut scan = ScanCtx {
            decls,
            strings: Default::default(),
            static_uses: Vec::new(),
            literals: bodies
                .classes
                .iter()
                .filter(|c| hir_compat::is_object_literal(&c.name))
                .map(|c| (c.name.as_str(), c.fields.as_slice()))
                .collect(),
            regex_constructors: Vec::new(),
            ctx,
            declarations,
            types,
            promise_sites: Vec::new(),
            bindings: Vec::new(),
            defined_locals: BitSet::default(),
            pending_captures: Vec::new(),
        };
        idx.layouts[init_tid].exits = idx.scan_stmts(&mut scan, &bodies.init, init_tid, false);

        for f in &bodies.functions {
            idx.register_member(
                &mut scan,
                TemplateKind::Function,
                f,
                locations.has_explicit_return(&f.name),
            );
        }

        for class in &bodies.classes {
            let Some(cid) = ClassId::of(class) else {
                continue;
            };
            idx.classes.insert(
                cid,
                ClassInfo {
                    declaration: class,
                    ty: scan.types.named(scan.ctx, &class.name),
                    field_init: None,
                    ctor: None,
                    implicit_params: class
                        .constructor
                        .iter()
                        .chain(&class.methods)
                        .flat_map(|f| &f.params)
                        .any(|p| {
                            hir_compat::declaration_flags(scan.declarations, p.id)
                                .implicit_parameter(p)
                        }),
                    direct_constructor: hir_compat::direct_constructor(class),
                    field_types: class
                        .fields
                        .iter()
                        .map(|field| scan.types.intern_hir(scan.ctx, &field.ty, true))
                        .collect(),
                },
            );
            idx.register_type_params(scan.ctx, GenericOwner::Class(cid), &class.type_params);
            let mut initializers = class
                .fields
                .iter()
                .filter_map(|field| field.init.as_ref())
                .peekable();
            if class.constructor.is_none() || initializers.peek().is_some() {
                let owner = idx.add_template(TemplateView::synthetic(
                    TemplateKind::FieldInitializers,
                    &class.name,
                ));
                idx.classes[&cid].field_init = Some(owner);
                for init in initializers {
                    idx.scan_expr(&mut scan, init, owner, false);
                }
            }
            idx.classes[&cid].ctor = class
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

        let mut static_fields = HashMap::new();
        for class in idx.classes.values() {
            for field in &class.declaration.static_fields {
                let declared = scan.types.intern_hir(scan.ctx, &field.ty, true);
                let loc = idx.location_types.push(if declared == Ty::OPEN {
                    Ty::BOTTOM
                } else {
                    declared
                });
                let id = idx.locals.ids.len() as LocalId;
                let local = idx.locals.insert(LocalInfo {
                    iteration: None,
                    element: None,
                    span: None,
                    id,
                    owner: TemplateId::INIT,
                    declared,
                    annotation: declared,
                    storage: LocalStorage::Module(loc),
                    flags: LocalFlags::STATIC_FIELD,
                });
                static_fields
                    .entry((class.declaration.name.as_str(), field.name.as_str()))
                    .or_insert(local);
            }
        }
        for (site, class, field) in scan.static_uses {
            if let Some(&local) = static_fields.get(&(class, field)) {
                idx.expressions.static_fields.set(site, local);
            }
        }
        for (site, args) in scan.regex_constructors {
            idx.expressions.declare(
                site,
                SiteResolution::Regex(scan.strings.regexp(ctx, &idx.locals, args)),
            );
        }
        scan.bindings.sort_by_key(|(owner, _)| *owner);
        for group in scan.bindings.chunk_by(|a, b| a.0 == b.0) {
            if let Some((owner, _)) = group.first() {
                idx.layouts[*owner].bindings =
                    idx.bindings.extend(group.iter().map(|(_, local)| *local));
            }
        }
        idx.events.sort_unstable_by_key(|&(key, _)| key);
        idx.events.dedup_by_key(|row| row.0);
        idx.external_uses.sort_unstable();
        idx.external_uses.dedup();
        scan.promise_sites.sort_unstable();
        scan.promise_sites.dedup();
        let mut promises = Pool::default();
        for group in scan.promise_sites.chunk_by(|a, b| a.0 == b.0) {
            let span = promises.extend(group.iter().map(|&(_, site)| {
                let mut settle = |kind, name| idx.add_template(TemplateView::synthetic(kind, name));
                (
                    site,
                    settle(TemplateKind::PromiseResolve(site), "resolve"),
                    settle(TemplateKind::PromiseReject(site), "reject"),
                )
            }));
            if let Some(&(owner, _)) = group.first() {
                idx.layouts[owner].promises = span;
            }
        }

        idx.promises = promises;
        for (cap_id, written, forward) in scan.pending_captures {
            if let Some(lidx) = idx.local_idx(cap_id) {
                let flags = &mut idx.locals.rows[lidx].flags;
                flags.insert(LocalFlags::CAPTURED);
                if written {
                    flags.insert(LocalFlags::WRITTEN_FROM_CLOSURE);
                }
                if forward && !flags.intersects(LocalFlags::MODULE_OR_PARAM) {
                    flags.insert(LocalFlags::FORWARD_CAPTURED);
                }
            }
        }
        let mut references = idx.views.filled(BTreeMap::new());
        for &(template, id) in &idx.external_uses {
            references[template].insert(id, false);
        }
        for tid in idx.views.ids().rev() {
            let view = &idx.views[tid];
            let mut free = std::mem::take(&mut references[tid]);
            free.extend(
                view.captures
                    .iter()
                    .map(|&id| (id, idx.is_non_global_local(id))),
            );
            free.retain(|&id, _| {
                idx.local_idx(id)
                    .is_some_and(|local| idx.locals.rows[local].owner != tid)
            });
            let mut captures = free
                .iter()
                .filter_map(|(&id, &captured)| captured.then_some(id))
                .peekable();
            if captures.peek().is_some() {
                idx.layouts[tid].captures = idx.capture_pool.extend(captures);
            } else if !view.captures_this
                && !matches!(
                    view.kind,
                    TemplateKind::PromiseResolve(_) | TemplateKind::PromiseReject(_)
                )
            {
                idx.capture_free_templates.insert(tid.index());
            }
            if let Some(parent) = idx.closure_owner(tid) {
                idx.layouts[tid].references = idx.capture_pool.extend(free.keys().copied());
                for (id, captured) in free {
                    *references[parent].entry(id).or_default() |= captured;
                }
            }
        }
        for local in idx.locals.rows.as_mut_slice() {
            local.span = declarations.local(local.id).map(|d| d.span);
        }
        idx
    }
}

impl ModuleIndex<'_> {
    pub(crate) fn emitted_uses(
        &self,
        bodies: &crate::source::InstanceBodies<'_>,
        mut origins: Origins,
        plans: &mut super::super::InstancePlans<'_>,
        locations: &mut hir_compat::Locations,
    ) -> perry_wit_diagnostics::Result<()> {
        use crate::engine::BodyOwner;
        for (tid, view) in self.views.iter_enumerated() {
            if origins[tid].is_empty()
                && let Some(parent) = self.closure_owner(tid)
                && let Some(&(_, kind, name)) = origins[parent].first()
            {
                origins[tid].push((BodyOwner::Function(view.func_id), kind, name));
            }
        }
        if locations.has_imports() {
            let functions = &bodies.functions;
            for &((tid, _), expr) in &self.events {
                let Some(call) = crate::source::SourceCall::of(expr) else {
                    continue;
                };
                for &(owner, kind, caller) in &origins[tid] {
                    if let Some(callee) = plans
                        .call_target(owner, call)?
                        .and_then(|id| functions.get(&id))
                    {
                        locations.record_cross_file_call(
                            (&callee.name, &self.view(callee.template).name),
                            call.offset,
                            caller
                                .and_then(|id| functions.get(&id))
                                .map(|f| f.name.as_ref()),
                            kind,
                        );
                    }
                }
            }
        }
        for &(template, id) in &self.external_uses {
            if origins[template]
                .iter()
                .any(|(_, kind, _)| *kind != CallerKind::Initializer)
            {
                let local = plans.local_id(id)?;
                if plans.locals.rows[local].flags.contains(LocalFlags::MODULE) {
                    plans.locals.rows[local]
                        .flags
                        .insert(LocalFlags::GLOBAL_SLOT);
                }
            }
        }
        Ok(())
    }
}
