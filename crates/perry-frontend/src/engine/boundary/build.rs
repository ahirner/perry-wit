//! Assemble the lexical index and close over capture, frame and promise sites.
use super::*;

impl<'h> ModuleIndex<'h> {
    pub(crate) fn build(
        ctx: &Context,
        bodies: &'h crate::source::Bodies,
        declarations: &crate::source::ModuleDeclarations,
        locations: &crate::hir_compat::Locations<'_>,
        closure_names: &'h HashMap<FuncId, String>,
        constructors: &crate::source::Constructors,
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
            name: "<init>",
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
            regex_operands: Vec::new(),
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
                methods: Vec::new(),
                static_methods: Vec::new(),
                getters: Vec::new(),
                setters: Vec::new(),
                static_field_locs: Vec::new(),
            });
            let mut static_field_locs = Vec::new();
            for field in &class.static_fields {
                let ty = idx.annotations.register(&mut scan, &field.ty, true);
                let loc = idx.add_location(ty);
                static_field_locs.push((scan.types.intern_name(&field.name), loc));
                if let Some(init) = &field.init {
                    Self::record_site_expr(&mut scan, init_tid, init);
                    idx.scan_expr(&mut scan, init, init_tid, false);
                }
            }
            let mut initializers = class
                .fields
                .iter()
                .filter_map(|field| field.init.as_ref())
                .peekable();
            if initializers.peek().is_some() {
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
            let ctor = class
                .constructor
                .as_ref()
                .map(|c| idx.register_member(&mut scan, TemplateKind::ClassConstructor, c, false));
            let methods: Vec<_> = class
                .methods
                .iter()
                .map(|m| {
                    let tid = idx.register_member(
                        &mut scan,
                        TemplateKind::ClassMethod,
                        m,
                        locations.has_explicit_return(&format!("{}::{}", class.name, m.name)),
                    );
                    (
                        scan.types.intern_name(hir_compat::method(&m.name).name()),
                        tid,
                    )
                })
                .collect();
            let static_methods: Vec<_> = class
                .static_methods
                .iter()
                .map(|m| {
                    let tid = idx.register_member(
                        &mut scan,
                        TemplateKind::ClassStaticMethod(cid),
                        m,
                        locations.has_explicit_return(&format!("{}::{}", class.name, m.name)),
                    );
                    (
                        scan.types.intern_name(hir_compat::method(&m.name).name()),
                        tid,
                    )
                })
                .collect();
            let getters: Vec<_> = class
                .getters
                .iter()
                .enumerate()
                .map(|(i, (name, g))| {
                    let tid = idx.register_member(
                        &mut scan,
                        TemplateKind::ClassGetter(i),
                        g,
                        locations.has_explicit_return(&format!("{}::get_{name}", class.name)),
                    );
                    (scan.types.intern_name(name), tid)
                })
                .collect();
            let setters: Vec<_> = class
                .setters
                .iter()
                .enumerate()
                .map(|(i, (name, s))| {
                    let tid = idx.register_member(
                        &mut scan,
                        TemplateKind::ClassSetter(i),
                        s,
                        locations.has_explicit_return(&format!("{}::set_{name}", class.name)),
                    );
                    (scan.types.intern_name(name), tid)
                })
                .collect();
            let info = &mut idx.classes[cid];
            info.ctor = ctor;
            info.methods = methods;
            info.static_methods = static_methods;
            info.getters = getters;
            info.setters = setters;
            info.static_field_locs = static_field_locs;
        }

        idx.strings
            .resolve_operands(scan.regex_operands, &idx.local_to_idx);
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
