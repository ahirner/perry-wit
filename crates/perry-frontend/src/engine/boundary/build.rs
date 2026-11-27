//! Assemble the lexical index and close over capture, frame and promise sites.
use super::*;

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
            functions: Vec::with_capacity(bodies.functions.len() + 1),
            local_to_idx: vec![None; max_local_hint as usize + 1],
            ..Self::default()
        };
        let init_tid = idx.add_template(TemplateView {
            body: &bodies.init,
            is_async: declarations.has_top_level_await,
            ..TemplateView::synthetic(TemplateKind::ModuleInit, "<init>")
        });
        debug_assert_eq!(init_tid, TemplateId::INIT);
        let mut scan = ScanCtx {
            constructors,
            decls,
            locations,
            regexes: Default::default(),
            ctx,
            declarations,
            closure_names,
            types,
            promise_sites: BTreeSet::new(),
            bindings: Vec::new(),
            defined_locals: BitSet::default(),
            pending_captures: Vec::new(),
        };
        idx.scan_stmts(&mut scan, &bodies.init, init_tid, false);

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
                idx.literal_classes.insert(&class.name, &class.fields);
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
                        .any(|p| scan.declarations.implicit_parameter(p)),
                    direct_constructor: hir_compat::direct_constructor(class),
                    static_field_locs: Vec::new(),
                },
            );
            idx.register_type_params(scan.ctx, GenericOwner::Class(cid), &class.type_params);
            for field in &class.fields {
                idx.annotations.register(&mut scan, &field.ty, true);
            }
            for field in &class.static_fields {
                let ty = idx.annotations.register(&mut scan, &field.ty, true);
                let loc = idx.add_location(if ty == Ty::OPEN { Ty::BOTTOM } else { ty });
                idx.classes[&cid].static_field_locs.push(loc);
            }
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

        idx.regexes = scan.regexes.finish(&idx.expressions);
        scan.bindings.sort_by_key(|(owner, _)| *owner);
        for group in scan.bindings.chunk_by(|a, b| a.0 == b.0) {
            if let Some((owner, _)) = group.first() {
                idx.layouts[*owner].bindings =
                    idx.bindings.extend(group.iter().map(|(_, local)| *local));
            }
        }
        for (owner, site) in scan.promise_sites {
            let resolve = idx.add_template(TemplateView::synthetic(
                TemplateKind::PromiseResolve(site),
                "resolve",
            ));
            let reject = idx.add_template(TemplateView::synthetic(
                TemplateKind::PromiseReject(site),
                "reject",
            ));
            idx.promises
                .entry(owner)
                .or_default()
                .push((site, resolve, reject));
        }

        for (cap_id, written, forward) in scan.pending_captures {
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
                    .is_some_and(|local| idx.locals[local].owner != tid)
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
        idx
    }
}

impl ModuleIndex<'_> {
    pub(crate) fn emitted_uses(
        &self,
        bodies: &crate::source::InstanceBodies<'_>,
        mut origins: Origins,
        plans: &super::super::InstancePlans,
        locations: &mut hir_compat::Locations,
    ) -> std::collections::HashSet<LocalId> {
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
            let functions: HashMap<_, _> = bodies.functions.iter().map(|f| (f.id, f)).collect();
            for (&(tid, _), &expr) in &self.events {
                let Some(call) = crate::source::SourceCall::of(expr) else {
                    continue;
                };
                for &(owner, kind, caller) in &origins[tid] {
                    if let Some(callee) = plans
                        .call_target(owner, expr)
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
        self.external_uses
            .iter()
            .filter_map(|&(template, id)| {
                (origins[template]
                    .iter()
                    .any(|(_, kind, _)| *kind != CallerKind::Initializer)
                    && plans.binding_flags(id).contains(LocalFlags::MODULE))
                .then_some(id)
            })
            .collect()
    }
}
