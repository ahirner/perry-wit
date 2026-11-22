//! Single-pass boundary indexing over the SWC AST and Perry HIR.
//!
//! Numbers every template (module init, functions, class members, closures), user class,
//! local binding, and initial storage location into dense `Vec`s and `BitSet`s.
use super::bitset::BitSet;
use super::types::{ClassId, LocId, LocalIdx, TemplateId, Ty, TypeTable};
use crate::hir_compat::{self, Locations};
use perry_hir::{
    ir::{Expr, Function, Module as HirModule, Param, Stmt},
    types::{FuncId, LocalId},
    walker::{stmt_any_expr, walk_expr_children},
};
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Result, unsupported};
use pliron::context::Context;
use std::collections::HashMap;
use swc_common::{Span, Spanned};
use swc_ecma_visit::{Visit, VisitWith};

/// Source-level facts extracted in a single pass over the SWC AST before HIR lowering.
#[derive(Default)]
pub(crate) struct SourceFacts {
    pub(crate) sparse_arrays: bool,
    /// Sorted `(start, end)` spans of unannotated variable declarations.
    pub(crate) inferred_spans: Vec<(u32, u32)>,
    /// Sorted start offsets of `let x;` declarations without an initializer.
    pub(crate) deferred_starts: Vec<u32>,
    /// Sorted start offsets of `const` variable declarations.
    pub(crate) const_starts: Vec<u32>,
    /// Sorted start offsets of explicitly type-annotated parameter bindings.
    pub(crate) param_annotated_starts: Vec<u32>,
    /// Equality spans involving function/closure expressions, preserved for diagnostics.
    pub(crate) callable_comparisons: Vec<Span>,
    /// Sorted `(param_start, value_use_span)` pairs for non-call uses of parameter bindings.
    pub(crate) param_value_uses: Vec<(u32, Span)>,
    /// First escaping closure/function literal found syntactically (name, span).
    pub(crate) escaping_closure: Option<(String, Span)>,
    /// Sorted start offsets of invoked local closure bindings (`const f = () => ...; f()`).
    pub(crate) invoked_closure_starts: Vec<u32>,
}

impl SourceFacts {
    pub(crate) fn is_inferred(&self, start: u32, end: u32) -> bool {
        self.inferred_spans.binary_search(&(start, end)).is_ok()
    }

    pub(crate) fn is_deferred(&self, start: u32) -> bool {
        self.deferred_starts.binary_search(&start).is_ok()
    }

    pub(crate) fn is_const(&self, start: u32) -> bool {
        self.const_starts.binary_search(&start).is_ok()
    }

    pub(crate) fn is_param_annotated(&self, start: u32) -> bool {
        self.param_annotated_starts.binary_search(&start).is_ok()
    }

    pub(crate) fn is_invoked_closure(&self, start: u32) -> bool {
        self.invoked_closure_starts.binary_search(&start).is_ok()
    }

    pub(crate) fn param_value_use(&self, start: u32) -> Option<Span> {
        let idx = self
            .param_value_uses
            .binary_search_by_key(&start, |(s, _)| *s)
            .ok()?;
        self.param_value_uses.get(idx).map(|(_, span)| *span)
    }
}

/// Analyze the SWC module in one pass: validate unsupported constructs and record source facts.
pub(crate) fn scan_ast(module: &ast::Module, locations: &Locations<'_>) -> Result<SourceFacts> {
    struct AstScanner {
        invalid_surrogate: Option<Span>,
        code_point_arity: Option<Span>,
        enum_decl: Option<Span>,
        facts: SourceFacts,
        // Boundary-only symbol tables keyed by SWC resolved `Id`, dropped when `scan_ast` returns.
        parameters: HashMap<ast::Id, u32>,
        closures: HashMap<ast::Id, Span>,
        invoked_ids: Vec<ast::Id>,
        enclosing: Option<swc_atoms::Atom>,
    }

    impl AstScanner {
        fn record_param(&mut self, pat: &ast::Pat) {
            record_pattern_annotations(pat, false, &mut self.facts.param_annotated_starts);
            if let ast::Pat::Ident(binding) = pat {
                self.parameters
                    .insert(binding.id.to_id(), binding.id.span.lo.0);
            }
        }

        fn is_closure_expr(&self, expr: &ast::Expr) -> bool {
            match expr {
                ast::Expr::Ident(id) => self.closures.contains_key(&id.to_id()),
                ast::Expr::Paren(paren) => self.is_closure_expr(&paren.expr),
                other => matches!(other, ast::Expr::Arrow(_) | ast::Expr::Fn(_)),
            }
        }

        fn mark_invoked(&mut self, expr: &ast::Expr) {
            if let ast::Expr::Ident(id) = expr {
                self.invoked_ids.push(id.to_id());
            }
        }

        fn visit_callable_position(&mut self, expr: &ast::Expr) {
            if matches!(expr, ast::Expr::Arrow(_) | ast::Expr::Fn(_)) {
                expr.visit_children_with(self);
                return;
            }
            if let ast::Expr::Paren(paren) = expr {
                self.visit_callable_position(&paren.expr);
                return;
            }
            if !matches!(expr, ast::Expr::Ident(_)) {
                expr.visit_with(self);
            }
        }
    }

    impl Visit for AstScanner {
        fn visit_ts_enum_decl(&mut self, decl: &ast::TsEnumDecl) {
            if !decl.declare {
                self.enum_decl.get_or_insert(decl.span);
            }
        }

        fn visit_str(&mut self, string: &ast::Str) {
            if string.value.as_str().is_none() {
                self.invalid_surrogate.get_or_insert(string.span);
            }
        }

        fn visit_tpl_element(&mut self, text: &ast::TplElement) {
            if text
                .cooked
                .as_ref()
                .is_some_and(|value| value.as_str().is_none())
            {
                self.invalid_surrogate.get_or_insert(text.span);
            }
        }

        fn visit_array_lit(&mut self, array: &ast::ArrayLit) {
            self.facts.sparse_arrays |= array.elems.iter().any(Option::is_none);
            array.visit_children_with(self);
        }

        fn visit_param(&mut self, param: &ast::Param) {
            self.record_param(&param.pat);
            param.visit_children_with(self);
        }

        fn visit_arrow_expr(&mut self, arrow: &ast::ArrowExpr) {
            for pat in &arrow.params {
                self.record_param(pat);
            }
            arrow.visit_children_with(self);
        }

        fn visit_fn_decl(&mut self, decl: &ast::FnDecl) {
            if self.enclosing.is_some() {
                self.closures.insert(decl.ident.to_id(), decl.ident.span);
            }
            let prev = self.enclosing.replace(decl.ident.sym.clone());
            decl.function.visit_with(self);
            self.enclosing = prev;
        }

        fn visit_for_head(&mut self, head: &ast::ForHead) {
            if let ast::ForHead::VarDecl(var) = head {
                for decl in &var.decls {
                    if var.kind == ast::VarDeclKind::Const {
                        record_pattern_starts(&decl.name, &mut self.facts.const_starts);
                    }
                    decl.visit_children_with(self);
                }
            } else {
                head.visit_children_with(self);
            }
        }

        fn visit_var_decl(&mut self, declaration: &ast::VarDecl) {
            let is_const = declaration.kind == ast::VarDeclKind::Const;
            for decl in &declaration.decls {
                if decl.init.is_none()
                    && let ast::Pat::Ident(binding) = &decl.name
                {
                    self.facts.deferred_starts.push(binding.id.span.lo.0);
                }
                if is_const {
                    record_pattern_starts(&decl.name, &mut self.facts.const_starts);
                }
                hir_compat::pattern_bindings(&decl.name, false, &mut |id, annotated| {
                    if !annotated {
                        self.facts.inferred_spans.push((id.span.lo.0, id.span.hi.0));
                    }
                });
                decl.name.visit_with(self);
                if let Some(init) = &decl.init {
                    if is_const
                        && let ast::Pat::Ident(binding) = &decl.name
                        && self.is_closure_expr(init)
                    {
                        let origin = match init.as_ref() {
                            ast::Expr::Ident(other) => self
                                .closures
                                .get(&other.to_id())
                                .copied()
                                .unwrap_or(binding.id.span),
                            _ => binding.id.span,
                        };
                        self.closures.insert(binding.id.to_id(), origin);
                    }
                    if is_const
                        && let (ast::Pat::Ident(binding), ast::Expr::Ident(source)) =
                            (&decl.name, init.as_ref())
                        && let Some(&origin) = self.parameters.get(&source.to_id())
                    {
                        self.parameters.insert(binding.id.to_id(), origin);
                    }
                    self.visit_callable_position(init);
                }
            }
        }

        fn visit_expr(&mut self, expr: &ast::Expr) {
            if let ast::Expr::Ident(id) = expr
                && let Some(&origin) = self.parameters.get(&id.to_id())
            {
                self.facts.param_value_uses.push((origin, id.span));
            }
            if self.is_closure_expr(expr) {
                self.facts.escaping_closure.get_or_insert_with(|| {
                    let name = match expr {
                        ast::Expr::Ident(id) => format!("function '{}'", id.sym),
                        _ => self.enclosing.as_ref().map_or_else(
                            || "anonymous function".into(),
                            |name| format!("closure in '{}'", hir_compat::display_name(name)),
                        ),
                    };
                    (name, expr.span())
                });
            } else {
                expr.visit_children_with(self);
            }
        }

        fn visit_bin_expr(&mut self, expr: &ast::BinExpr) {
            if matches!(
                expr.op,
                ast::BinaryOp::EqEqEq
                    | ast::BinaryOp::NotEqEq
                    | ast::BinaryOp::EqEq
                    | ast::BinaryOp::NotEq
            ) {
                self.mark_invoked(&expr.left);
                self.mark_invoked(&expr.right);
                if self.is_closure_expr(&expr.left) || self.is_closure_expr(&expr.right) {
                    self.facts.callable_comparisons.push(expr.span);
                }
                self.visit_callable_position(&expr.left);
                self.visit_callable_position(&expr.right);
            } else {
                expr.visit_children_with(self);
            }
        }

        fn visit_call_expr(&mut self, call: &ast::CallExpr) {
            if matches!(&call.callee, ast::Callee::Expr(callee) if matches!(&**callee, ast::Expr::Member(member) if matches!(&*member.obj, ast::Expr::Ident(id) if id.sym == "String") && matches!(&member.prop, ast::MemberProp::Ident(id) if id.sym == "fromCodePoint")))
                && (call.args.len() != 1 || call.args.iter().any(|arg| arg.spread.is_some()))
            {
                self.code_point_arity.get_or_insert(call.span);
            }
            if let ast::Callee::Expr(callee) = &call.callee
                && matches!(callee.as_ref(), ast::Expr::Ident(_))
            {
                self.mark_invoked(callee);
                for arg in &call.args {
                    self.mark_invoked(&arg.expr);
                }
            }
            match &call.callee {
                ast::Callee::Expr(expr) => self.visit_callable_position(expr),
                callee => callee.visit_with(self),
            }
            for arg in &call.args {
                self.visit_callable_position(&arg.expr);
            }
        }
    }

    let mut scanner = AstScanner {
        invalid_surrogate: None,
        code_point_arity: None,
        enum_decl: None,
        facts: SourceFacts::default(),
        parameters: HashMap::new(),
        closures: HashMap::new(),
        invoked_ids: Vec::new(),
        enclosing: None,
    };
    module.visit_with(&mut scanner);

    if let Some(span) = scanner.invalid_surrogate {
        return Err(locations.annotate(
            unsupported!(
                "lone UTF-16 surrogate in a string literal; Perry strings contain Unicode scalars only"
            ),
            span,
        ));
    }
    if let Some(span) = scanner.code_point_arity {
        return Err(locations.annotate(
            unsupported!("String.fromCodePoint currently requires exactly one scalar argument"),
            span,
        ));
    }
    if let Some(span) = scanner.enum_decl {
        return Err(locations.annotate(
            unsupported!(
                "TypeScript enum declarations are not supported yet; use a union of string or number literals instead"
            ),
            span,
        ));
    }

    for id in &scanner.invoked_ids {
        if let Some(span) = scanner.closures.get(id) {
            scanner.facts.invoked_closure_starts.push(span.lo.0);
        }
    }
    scanner.facts.inferred_spans.sort_unstable();
    scanner.facts.inferred_spans.dedup();
    scanner.facts.deferred_starts.sort_unstable();
    scanner.facts.deferred_starts.dedup();
    scanner.facts.const_starts.sort_unstable();
    scanner.facts.const_starts.dedup();
    scanner.facts.param_annotated_starts.sort_unstable();
    scanner.facts.param_annotated_starts.dedup();
    scanner.facts.invoked_closure_starts.sort_unstable();
    scanner.facts.invoked_closure_starts.dedup();
    scanner
        .facts
        .param_value_uses
        .sort_unstable_by_key(|(start, _)| *start);
    scanner
        .facts
        .param_value_uses
        .dedup_by_key(|(start, _)| *start);

    Ok(scanner.facts)
}

fn record_pattern_starts(pat: &ast::Pat, out: &mut Vec<u32>) {
    hir_compat::pattern_bindings(pat, false, &mut |id, _| out.push(id.span.lo.0));
}

fn record_pattern_annotations(pat: &ast::Pat, inherited: bool, out: &mut Vec<u32>) {
    match pat {
        ast::Pat::Ident(binding) => {
            if inherited || binding.type_ann.is_some() {
                out.push(binding.id.span.lo.0);
            }
        }
        ast::Pat::Array(arr) => {
            let ann = inherited || arr.type_ann.is_some();
            if ann {
                out.push(arr.span.lo.0);
            }
            for elem in arr.elems.iter().flatten() {
                record_pattern_annotations(elem, ann, out);
            }
        }
        ast::Pat::Object(obj) => {
            let ann = inherited || obj.type_ann.is_some();
            if ann {
                out.push(obj.span.lo.0);
            }
            for prop in &obj.props {
                match prop {
                    ast::ObjectPatProp::KeyValue(kv) => {
                        record_pattern_annotations(&kv.value, ann, out)
                    }
                    ast::ObjectPatProp::Assign(assign) => {
                        if ann {
                            out.push(assign.key.id.span.lo.0);
                        }
                    }
                    ast::ObjectPatProp::Rest(rest) => {
                        record_pattern_annotations(&rest.arg, ann, out)
                    }
                }
            }
        }
        ast::Pat::Rest(rest) => {
            let ann = inherited || rest.type_ann.is_some();
            if ann {
                out.push(rest.span.lo.0);
            }
            record_pattern_annotations(&rest.arg, ann, out);
        }
        ast::Pat::Assign(assign) => record_pattern_annotations(&assign.left, inherited, out),
        ast::Pat::Invalid(_) | ast::Pat::Expr(_) => {}
    }
}

/// What declaration a [`TemplateId`] refers to inside the borrowed [`HirModule`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TemplateKind {
    ModuleInit,
    Function {
        hir_idx: usize,
        func_id: FuncId,
    },
    ClassConstructor {
        class: ClassId,
        func_id: FuncId,
    },
    ClassMethod {
        class: ClassId,
        method_idx: usize,
        func_id: FuncId,
    },
    ClassStaticMethod {
        class: ClassId,
        method_idx: usize,
        func_id: FuncId,
    },
    ClassGetter {
        class: ClassId,
        getter_idx: usize,
        func_id: FuncId,
    },
    ClassSetter {
        class: ClassId,
        setter_idx: usize,
        func_id: FuncId,
    },
    Closure {
        parent: TemplateId,
        func_id: FuncId,
    },
}

/// Borrowed view of a template declaration inside [`HirModule`], avoiding `Rc` and `Function` clones.
pub(crate) struct TemplateView<'h> {
    pub(crate) id: TemplateId,
    pub(crate) kind: TemplateKind,
    pub(crate) name: &'h str,
    pub(crate) func_id: FuncId,
    pub(crate) params: &'h [Param],
    pub(crate) return_type: &'h perry_hir::types::Type,
    pub(crate) body: &'h [Stmt],
    pub(crate) captures: &'h [LocalId],
    pub(crate) type_params: &'h [perry_hir::types::TypeParam],
    pub(crate) is_async: bool,
    pub(crate) is_generator: bool,
    pub(crate) is_strict: bool,
    pub(crate) construct_only: bool,
    pub(crate) is_exported: bool,
    pub(crate) captures_this: bool,
    pub(crate) captures_new_target: bool,
}

/// Kind of open aggregate allocated by `[]` or `{}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AggregateKind {
    Array,
    Dictionary,
}

/// What a storage location [`LocId`] represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocationKind {
    ModuleBinding(LocalIdx),
    StaticField { class: ClassId, field_idx: usize },
    ClassField { class: ClassId, field_idx: usize },
    Local(LocalIdx),
    EmptyArray(LocalIdx),
    EmptyObject(LocalIdx),
    TemplateReturn(TemplateId),
}

/// Dense index of all templates, classes, locals, and initial locations built in one pass.
pub(crate) struct ModuleIndex<'h> {
    pub(crate) templates: Vec<TemplateKind>,
    pub(crate) views: Vec<TemplateView<'h>>,
    pub(crate) template_exported: BitSet,
    pub(crate) template_is_closure: BitSet,
    pub(crate) template_explicit_return: BitSet,
    /// `hir_func_id -> TemplateId` boundary map (used once to resolve `Expr::FuncRef` and closures).
    func_to_template: HashMap<FuncId, TemplateId>,
    /// User classes (excluding `__AnonShape_` object literal classes): `ClassId -> index in hir.classes`.
    pub(crate) class_hir_idx: Vec<usize>,
    pub(crate) class_names: Vec<&'h str>,
    pub(crate) class_ctor: Vec<Option<TemplateId>>,
    pub(crate) class_methods: Vec<Vec<(&'h str, TemplateId)>>,
    pub(crate) class_static_methods: Vec<Vec<(&'h str, TemplateId)>>,
    pub(crate) class_getters: Vec<Vec<(&'h str, TemplateId)>>,
    pub(crate) class_setters: Vec<Vec<(&'h str, TemplateId)>>,
    pub(crate) class_static_field_locs: Vec<Vec<(&'h str, LocId)>>,
    pub(crate) class_has_implicit_params: BitSet,
    /// `__AnonShape_*` literal class fields (name -> slice of fields).
    literal_classes: Vec<(&'h str, &'h [perry_hir::ir::ClassField])>,
    /// `LocalIdx -> Hir LocalId`.
    pub(crate) local_ids: Vec<LocalId>,
    /// `LocalIdx -> declaring TemplateId`.
    pub(crate) local_owner: Vec<TemplateId>,
    /// `LocalIdx -> boundary declared Ty` (interned once).
    pub(crate) local_declared: Vec<Ty>,
    /// `LocalIdx -> initial LocId` (if the local has a module/static storage location).
    pub(crate) local_loc: Vec<Option<LocId>>,
    /// `LocalIdx -> initial empty aggregate location` (for module-level `[]` / `{}`).
    pub(crate) local_empty_agg: Vec<Option<(LocId, AggregateKind)>>,
    /// `LocalIdx -> first escaping parameter value-use span` (if any).
    pub(crate) local_value_use: Vec<Option<Span>>,
    /// `Hir LocalId -> LocalIdx` boundary map.
    local_map: HashMap<LocalId, LocalIdx>,
    /// Source-fact bitsets indexed by `LocalIdx`.
    pub(crate) local_inferred: BitSet,
    pub(crate) local_deferred: BitSet,
    pub(crate) local_const: BitSet,
    pub(crate) local_param: BitSet,
    pub(crate) local_param_annotated: BitSet,
    pub(crate) local_module: BitSet,
    pub(crate) local_captured: BitSet,
    pub(crate) local_written_from_closure: BitSet,
    pub(crate) local_invoked: BitSet,
    pub(crate) local_escaping: BitSet,
    /// Initial storage locations numbered at the boundary.
    pub(crate) locations: Vec<LocationKind>,
    pub(crate) location_types: Vec<Ty>,
    /// Maximum `FuncId` and `LocalId` seen in the module (for fresh emission IDs).
    pub(crate) max_func_id: FuncId,
    pub(crate) max_local_id: LocalId,
}

static VOID_HIR: perry_hir::types::Type = perry_hir::types::Type::Void;

impl<'h> ModuleIndex<'h> {
    pub(crate) fn build(
        ctx: &Context,
        hir: &'h HirModule,
        facts: &SourceFacts,
        locations: &Locations<'_>,
        types: &mut TypeTable,
    ) -> Self {
        let mut idx = Self {
            templates: Vec::with_capacity(1 + hir.functions.len()),
            views: Vec::with_capacity(1 + hir.functions.len()),
            template_exported: BitSet::default(),
            template_is_closure: BitSet::default(),
            template_explicit_return: BitSet::default(),
            func_to_template: HashMap::with_capacity(hir.functions.len()),
            class_hir_idx: Vec::new(),
            class_names: Vec::new(),
            class_ctor: Vec::new(),
            class_methods: Vec::new(),
            class_static_methods: Vec::new(),
            class_getters: Vec::new(),
            class_setters: Vec::new(),
            class_static_field_locs: Vec::new(),
            class_has_implicit_params: BitSet::default(),
            literal_classes: Vec::new(),
            local_ids: Vec::new(),
            local_owner: Vec::new(),
            local_declared: Vec::new(),
            local_loc: Vec::new(),
            local_empty_agg: Vec::new(),
            local_value_use: Vec::new(),
            local_map: HashMap::new(),
            local_inferred: BitSet::default(),
            local_deferred: BitSet::default(),
            local_const: BitSet::default(),
            local_param: BitSet::default(),
            local_param_annotated: BitSet::default(),
            local_module: BitSet::default(),
            local_captured: BitSet::default(),
            local_written_from_closure: BitSet::default(),
            local_invoked: BitSet::default(),
            local_escaping: BitSet::default(),
            locations: Vec::new(),
            location_types: Vec::new(),
            max_func_id: 0,
            max_local_id: hir.local_source_spans.keys().copied().max().unwrap_or(0),
        };

        // Pre-index `__AnonShape_*` object literal classes so empty-object checks work during init scan.
        for class in &hir.classes {
            if hir_compat::is_object_literal(&class.name) {
                idx.literal_classes
                    .push((class.name.as_str(), class.fields.as_slice()));
            }
        }

        // TemplateId(0) is always ModuleInit.
        let init_tid = idx.add_template(
            TemplateView {
                id: TemplateId(0),
                kind: TemplateKind::ModuleInit,
                name: "<init>",
                func_id: 0,
                params: &[],
                return_type: &VOID_HIR,
                body: &hir.init,
                captures: &[],
                type_params: &[],
                is_async: hir.has_top_level_await,
                is_generator: false,
                is_strict: true,
                construct_only: false,
                is_exported: false,
                captures_this: false,
                captures_new_target: false,
            },
            false,
            false,
            false,
        );
        let mut scan = ScanCtx {
            ctx,
            hir,
            facts,
            types,
        };
        idx.scan_stmts(&mut scan, &hir.init, init_tid, true);

        for (hir_idx, function) in hir.functions.iter().enumerate() {
            idx.max_func_id = idx.max_func_id.max(function.id);
            let explicit_ret = locations.has_explicit_return(&function.name);
            let tid = idx.add_function_template(
                TemplateKind::Function {
                    hir_idx,
                    func_id: function.id,
                },
                function,
                function.is_exported,
                explicit_ret,
            );
            idx.func_to_template.insert(function.id, tid);
            idx.scan_function(&mut scan, function, tid);
        }

        for (hir_idx, class) in hir.classes.iter().enumerate() {
            if hir_compat::is_object_literal(&class.name) {
                continue;
            }
            let cid = ClassId(idx.class_hir_idx.len() as u32);
            idx.class_hir_idx.push(hir_idx);
            idx.class_names.push(class.name.as_str());
            idx.class_ctor.push(None);
            idx.class_methods.push(Vec::new());
            idx.class_static_methods.push(Vec::new());
            idx.class_getters.push(Vec::new());
            idx.class_setters.push(Vec::new());
            idx.class_static_field_locs.push(Vec::new());

            for (field_idx, field) in class.static_fields.iter().enumerate() {
                let ty = scan.types.intern_hir(scan.ctx, &field.ty, true);
                let loc = idx.add_location(
                    LocationKind::StaticField {
                        class: cid,
                        field_idx,
                    },
                    ty,
                );
                if let Some(list) = idx.class_static_field_locs.get_mut(cid.0 as usize) {
                    list.push((field.name.as_str(), loc));
                }
                if let Some(init) = &field.init {
                    idx.scan_expr(&mut scan, init, init_tid);
                }
            }
            for (field_idx, field) in class.fields.iter().enumerate() {
                let ty = scan.types.intern_hir(scan.ctx, &field.ty, true);
                idx.add_location(
                    LocationKind::ClassField {
                        class: cid,
                        field_idx,
                    },
                    ty,
                );
                if let Some(init) = &field.init {
                    idx.scan_expr(&mut scan, init, init_tid);
                }
            }

            if let Some(ctor) = &class.constructor {
                idx.max_func_id = idx.max_func_id.max(ctor.id);
                let tid = idx.add_function_template(
                    TemplateKind::ClassConstructor {
                        class: cid,
                        func_id: ctor.id,
                    },
                    ctor,
                    false,
                    false,
                );
                idx.func_to_template.insert(ctor.id, tid);
                if let Some(slot) = idx.class_ctor.get_mut(cid.0 as usize) {
                    *slot = Some(tid);
                }
                if ctor
                    .params
                    .iter()
                    .any(|p| idx.param_is_unannotated(scan.hir, scan.facts, p))
                {
                    idx.class_has_implicit_params.insert(cid.0 as usize);
                }
                idx.scan_function(&mut scan, ctor, tid);
            }

            for (method_idx, method) in class.methods.iter().enumerate() {
                idx.max_func_id = idx.max_func_id.max(method.id);
                let key = format!("{}::{}", class.name, method.name);
                let tid = idx.add_function_template(
                    TemplateKind::ClassMethod {
                        class: cid,
                        method_idx,
                        func_id: method.id,
                    },
                    method,
                    false,
                    locations.has_explicit_return(&key),
                );
                idx.func_to_template.insert(method.id, tid);
                if let Some(list) = idx.class_methods.get_mut(cid.0 as usize) {
                    list.push((method.name.as_str(), tid));
                }
                if method
                    .params
                    .iter()
                    .any(|p| idx.param_is_unannotated(scan.hir, scan.facts, p))
                {
                    idx.class_has_implicit_params.insert(cid.0 as usize);
                }
                idx.scan_function(&mut scan, method, tid);
            }

            for (method_idx, method) in class.static_methods.iter().enumerate() {
                idx.max_func_id = idx.max_func_id.max(method.id);
                let key = format!("{}::{}", class.name, method.name);
                let tid = idx.add_function_template(
                    TemplateKind::ClassStaticMethod {
                        class: cid,
                        method_idx,
                        func_id: method.id,
                    },
                    method,
                    false,
                    locations.has_explicit_return(&key),
                );
                idx.func_to_template.insert(method.id, tid);
                if let Some(list) = idx.class_static_methods.get_mut(cid.0 as usize) {
                    list.push((method.name.as_str(), tid));
                }
                idx.scan_function(&mut scan, method, tid);
            }

            for (getter_idx, (name, getter)) in class.getters.iter().enumerate() {
                idx.max_func_id = idx.max_func_id.max(getter.id);
                let key = format!("{}::get_{name}", class.name);
                let tid = idx.add_function_template(
                    TemplateKind::ClassGetter {
                        class: cid,
                        getter_idx,
                        func_id: getter.id,
                    },
                    getter,
                    false,
                    locations.has_explicit_return(&key),
                );
                idx.func_to_template.insert(getter.id, tid);
                if let Some(list) = idx.class_getters.get_mut(cid.0 as usize) {
                    list.push((name.as_str(), tid));
                }
                idx.scan_function(&mut scan, getter, tid);
            }

            for (setter_idx, (name, setter)) in class.setters.iter().enumerate() {
                idx.max_func_id = idx.max_func_id.max(setter.id);
                let key = format!("{}::set_{name}", class.name);
                let tid = idx.add_function_template(
                    TemplateKind::ClassSetter {
                        class: cid,
                        setter_idx,
                        func_id: setter.id,
                    },
                    setter,
                    false,
                    locations.has_explicit_return(&key),
                );
                idx.func_to_template.insert(setter.id, tid);
                if let Some(list) = idx.class_setters.get_mut(cid.0 as usize) {
                    list.push((name.as_str(), tid));
                }
                idx.scan_function(&mut scan, setter, tid);
            }
        }

        idx
    }

    pub(crate) fn view(&self, id: TemplateId) -> Option<&TemplateView<'h>> {
        self.views.get(id.0 as usize)
    }

    pub(crate) fn class_by_name(&self, name: &str) -> Option<ClassId> {
        self.class_names
            .iter()
            .position(|&n| n == name)
            .map(|i| ClassId(i as u32))
    }

    pub(crate) fn class_method(&self, class: ClassId, name: &str) -> Option<TemplateId> {
        let norm = hir_compat::method(name).name();
        self.class_methods
            .get(class.0 as usize)?
            .iter()
            .find(|(n, _)| *n == norm)
            .map(|(_, tid)| *tid)
    }

    pub(crate) fn class_static_method(&self, class: ClassId, name: &str) -> Option<TemplateId> {
        let norm = hir_compat::method(name).name();
        self.class_static_methods
            .get(class.0 as usize)?
            .iter()
            .find(|(n, _)| *n == norm)
            .map(|(_, tid)| *tid)
    }

    pub(crate) fn class_getter(&self, class: ClassId, name: &str) -> Option<TemplateId> {
        self.class_getters
            .get(class.0 as usize)?
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, tid)| *tid)
    }

    pub(crate) fn class_setter(&self, class: ClassId, name: &str) -> Option<TemplateId> {
        self.class_setters
            .get(class.0 as usize)?
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, tid)| *tid)
    }

    pub(crate) fn class_static_field_loc(&self, class: ClassId, name: &str) -> Option<LocId> {
        self.class_static_field_locs
            .get(class.0 as usize)?
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, loc)| *loc)
    }

    pub(crate) fn literal_fields(&self, name: &str) -> Option<&'h [perry_hir::ir::ClassField]> {
        self.literal_classes
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, fields)| *fields)
    }

    pub(crate) fn is_empty_object_expr(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Object(fields) => fields.is_empty(),
            Expr::New {
                class_name, args, ..
            } => {
                args.is_empty()
                    && hir_compat::is_object_literal(class_name)
                    && self.literal_fields(class_name).is_some_and(<[_]>::is_empty)
            }
            _ => false,
        }
    }

    pub(crate) fn template_of_func(&self, id: FuncId) -> Option<TemplateId> {
        self.func_to_template.get(&id).copied()
    }

    pub(crate) fn local_idx(&self, id: LocalId) -> Option<LocalIdx> {
        self.local_map.get(&id).copied()
    }

    pub(crate) fn template_count(&self) -> usize {
        self.templates.len()
    }

    pub(crate) fn local_count(&self) -> usize {
        self.local_ids.len()
    }

    pub(crate) fn location_count(&self) -> usize {
        self.locations.len()
    }

    fn add_function_template(
        &mut self,
        kind: TemplateKind,
        function: &'h Function,
        exported: bool,
        explicit_return: bool,
    ) -> TemplateId {
        let id = TemplateId(self.templates.len() as u32);
        self.add_template(
            TemplateView {
                id,
                kind,
                name: function.name.as_str(),
                func_id: function.id,
                params: &function.params,
                return_type: &function.return_type,
                body: &function.body,
                captures: &function.captures,
                type_params: &function.type_params,
                is_async: function.is_async,
                is_generator: function.is_generator,
                is_strict: function.is_strict,
                construct_only: function.construct_only,
                is_exported: function.is_exported,
                captures_this: false,
                captures_new_target: false,
            },
            exported,
            false,
            explicit_return,
        )
    }

    fn add_template(
        &mut self,
        mut view: TemplateView<'h>,
        exported: bool,
        is_closure: bool,
        explicit_return: bool,
    ) -> TemplateId {
        let id = TemplateId(self.templates.len() as u32);
        view.id = id;
        self.templates.push(view.kind);
        self.views.push(view);
        if exported {
            self.template_exported.insert(id.0 as usize);
        }
        if is_closure {
            self.template_is_closure.insert(id.0 as usize);
        }
        if explicit_return {
            self.template_explicit_return.insert(id.0 as usize);
        }
        id
    }

    fn add_location(&mut self, kind: LocationKind, initial: Ty) -> LocId {
        let id = LocId(self.locations.len() as u32);
        self.locations.push(kind);
        self.location_types.push(initial);
        id
    }

    fn ensure_local(
        &mut self,
        id: LocalId,
        owner: TemplateId,
        declared: Ty,
        hir: &HirModule,
        facts: &SourceFacts,
    ) -> LocalIdx {
        self.max_local_id = self.max_local_id.max(id);
        if let Some(&existing) = self.local_map.get(&id) {
            return existing;
        }
        let idx = LocalIdx(self.local_ids.len() as u32);
        self.local_ids.push(id);
        self.local_owner.push(owner);
        self.local_declared.push(declared);
        self.local_loc.push(None);
        self.local_empty_agg.push(None);
        self.local_value_use.push(None);
        self.local_map.insert(id, idx);

        if let Some(span) = hir.local_source_spans.get(&id) {
            if facts.is_inferred(span.start, span.end) {
                self.local_inferred.insert(idx.0 as usize);
            }
            if facts.is_deferred(span.start) {
                self.local_deferred.insert(idx.0 as usize);
            }
            if facts.is_const(span.start) {
                self.local_const.insert(idx.0 as usize);
            }
            if facts.is_param_annotated(span.start) {
                self.local_param_annotated.insert(idx.0 as usize);
            }
            if facts.is_invoked_closure(span.start) {
                self.local_invoked.insert(idx.0 as usize);
            }
            if let Some(use_span) = facts.param_value_use(span.start) {
                self.local_escaping.insert(idx.0 as usize);
                if let Some(slot) = self.local_value_use.get_mut(idx.0 as usize) {
                    *slot = Some(use_span);
                }
            }
        }
        idx
    }

    fn param_is_unannotated(&self, hir: &HirModule, facts: &SourceFacts, param: &Param) -> bool {
        let annotated = hir
            .local_source_spans
            .get(&param.id)
            .is_some_and(|span| facts.is_param_annotated(span.start));
        !annotated
            && matches!(
                param.ty,
                perry_hir::types::Type::Any | perry_hir::types::Type::Unknown
            )
    }

    fn scan_function(
        &mut self,
        scan: &mut ScanCtx<'h, '_>,
        function: &'h Function,
        owner: TemplateId,
    ) {
        let explicit_ret = self.template_explicit_return.contains(owner.0 as usize);
        let ret_ty = scan
            .types
            .intern_hir(scan.ctx, &function.return_type, explicit_ret);
        self.add_location(LocationKind::TemplateReturn(owner), ret_ty);

        for param in &function.params {
            let explicit = scan
                .hir
                .local_source_spans
                .get(&param.id)
                .is_some_and(|span| scan.facts.is_param_annotated(span.start));
            let ty = scan.types.intern_hir(scan.ctx, &param.ty, explicit);
            let local = self.ensure_local(param.id, owner, ty, scan.hir, scan.facts);
            self.local_param.insert(local.0 as usize);
            if let Some(default) = &param.default {
                self.scan_expr(scan, default, owner);
            }
        }
        self.scan_stmts(scan, &function.body, owner, false);
    }

    fn scan_stmts(
        &mut self,
        scan: &mut ScanCtx<'h, '_>,
        stmts: &'h [Stmt],
        owner: TemplateId,
        is_module_top: bool,
    ) {
        for stmt in stmts {
            match stmt {
                Stmt::Let {
                    id,
                    ty,
                    mutable,
                    init,
                    ..
                } => {
                    let explicit = scan
                        .hir
                        .local_source_spans
                        .get(id)
                        .is_some_and(|span| !scan.facts.is_inferred(span.start, span.end));
                    let declared = scan.types.intern_hir(scan.ctx, ty, explicit);
                    let local = self.ensure_local(*id, owner, declared, scan.hir, scan.facts);
                    if !*mutable {
                        self.local_const.insert(local.0 as usize);
                    }
                    if is_module_top {
                        self.local_module.insert(local.0 as usize);
                        let initial = if !explicit || scan.types.is_open(declared) {
                            Ty::BOTTOM
                        } else {
                            declared
                        };
                        let loc = self.add_location(LocationKind::ModuleBinding(local), initial);
                        if let Some(slot) = self.local_loc.get_mut(local.0 as usize) {
                            *slot = Some(loc);
                        }
                    } else if self.local_deferred.contains(local.0 as usize) {
                        let loc = self.add_location(LocationKind::Local(local), Ty::BOTTOM);
                        if let Some(slot) = self.local_loc.get_mut(local.0 as usize) {
                            *slot = Some(loc);
                        }
                    }
                    if let Some(init) = init {
                        if matches!(init, Expr::Array(items, _) if items.is_empty()) {
                            let agg_loc =
                                self.add_location(LocationKind::EmptyArray(local), Ty::BOTTOM);
                            if let Some(slot) = self.local_empty_agg.get_mut(local.0 as usize) {
                                *slot = Some((agg_loc, AggregateKind::Array));
                            }
                        } else if self.is_empty_object_expr(init) {
                            let agg_loc =
                                self.add_location(LocationKind::EmptyObject(local), Ty::BOTTOM);
                            if let Some(slot) = self.local_empty_agg.get_mut(local.0 as usize) {
                                *slot = Some((agg_loc, AggregateKind::Dictionary));
                            }
                        }
                        self.scan_expr(scan, init, owner);
                    }
                }
                Stmt::If {
                    condition,
                    then_branch,
                    else_branch,
                } => {
                    self.scan_expr(scan, condition, owner);
                    self.scan_stmts(scan, then_branch, owner, false);
                    if let Some(else_branch) = else_branch {
                        self.scan_stmts(scan, else_branch, owner, false);
                    }
                }
                Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                    self.scan_expr(scan, condition, owner);
                    self.scan_stmts(scan, body, owner, false);
                }
                Stmt::For {
                    init,
                    condition,
                    update,
                    body,
                } => {
                    if let Some(init) = init {
                        self.scan_stmts(scan, std::slice::from_ref(init), owner, false);
                    }
                    if let Some(condition) = condition {
                        self.scan_expr(scan, condition, owner);
                    }
                    if let Some(update) = update {
                        self.scan_expr(scan, update, owner);
                    }
                    self.scan_stmts(scan, body, owner, false);
                }
                Stmt::Try {
                    body,
                    catch,
                    finally,
                    ..
                } => {
                    self.scan_stmts(scan, body, owner, false);
                    if let Some(catch) = catch {
                        if let Some((id, _)) = &catch.param {
                            let err_ty = scan.types.named(scan.ctx, "Error");
                            self.ensure_local(*id, owner, err_ty, scan.hir, scan.facts);
                        }
                        self.scan_stmts(scan, &catch.body, owner, false);
                    }
                    if let Some(finally) = finally {
                        self.scan_stmts(scan, finally, owner, false);
                    }
                }
                Stmt::Switch {
                    discriminant,
                    cases,
                } => {
                    self.scan_expr(scan, discriminant, owner);
                    for case in cases {
                        if let Some(test) = &case.test {
                            self.scan_expr(scan, test, owner);
                        }
                        self.scan_stmts(scan, &case.body, owner, false);
                    }
                }
                Stmt::Labeled { body, .. } => {
                    self.scan_stmts(scan, std::slice::from_ref(body), owner, false);
                }
                other => {
                    stmt_any_expr(other, &mut |expr| {
                        self.scan_expr(scan, expr, owner);
                        false
                    });
                }
            }
        }
    }

    fn scan_expr(&mut self, scan: &mut ScanCtx<'h, '_>, expr: &'h Expr, owner: TemplateId) {
        match expr {
            Expr::LocalGet(id) => {
                self.record_local_use(*id, owner, false);
            }
            Expr::LocalSet(id, _) | Expr::Update { id, .. } => {
                self.record_local_use(*id, owner, true);
            }
            Expr::ScopedTemp { id, .. } => {
                self.ensure_local(*id, owner, Ty::OPEN, scan.hir, scan.facts);
            }
            Expr::Closure {
                func_id,
                params,
                return_type,
                body,
                captures,
                mutable_captures,
                is_async,
                is_generator,
                is_strict,
                construct_only,
                captures_this,
                captures_new_target,
                ..
            } => {
                self.max_func_id = self.max_func_id.max(*func_id);
                let explicit_ret = !matches!(
                    return_type,
                    perry_hir::types::Type::Any | perry_hir::types::Type::Unknown
                );
                let tid = self.add_template(
                    TemplateView {
                        id: TemplateId(0),
                        kind: TemplateKind::Closure {
                            parent: owner,
                            func_id: *func_id,
                        },
                        name: "",
                        func_id: *func_id,
                        params: params.as_slice(),
                        return_type,
                        body: body.as_slice(),
                        captures: captures.as_slice(),
                        type_params: &[],
                        is_async: *is_async,
                        is_generator: *is_generator,
                        is_strict: *is_strict,
                        construct_only: *construct_only,
                        is_exported: false,
                        captures_this: *captures_this,
                        captures_new_target: *captures_new_target,
                    },
                    false,
                    true,
                    explicit_ret,
                );
                self.func_to_template.insert(*func_id, tid);
                let ret_ty = scan.types.intern_hir(scan.ctx, return_type, explicit_ret);
                self.add_location(LocationKind::TemplateReturn(tid), ret_ty);

                for &cap in captures {
                    if let Some(local) = self.local_idx(cap) {
                        self.local_captured.insert(local.0 as usize);
                    }
                }
                for &mcap in mutable_captures {
                    if let Some(local) = self.local_idx(mcap) {
                        self.local_captured.insert(local.0 as usize);
                        self.local_written_from_closure.insert(local.0 as usize);
                    }
                }
                for param in params {
                    let explicit = scan
                        .hir
                        .local_source_spans
                        .get(&param.id)
                        .is_some_and(|span| scan.facts.is_param_annotated(span.start));
                    let ty = scan.types.intern_hir(scan.ctx, &param.ty, explicit);
                    let local = self.ensure_local(param.id, tid, ty, scan.hir, scan.facts);
                    self.local_param.insert(local.0 as usize);
                    if let Some(default) = &param.default {
                        self.scan_expr(scan, default, tid);
                    }
                }
                self.scan_stmts(scan, body, tid, false);
                return;
            }
            _ => {}
        }
        walk_expr_children(expr, &mut |child| {
            self.scan_expr(scan, child, owner);
        });
    }

    fn record_local_use(&mut self, id: LocalId, reader: TemplateId, is_write: bool) {
        self.max_local_id = self.max_local_id.max(id);
        let Some(local) = self.local_idx(id) else {
            return;
        };
        let declared_owner = self.local_owner.get(local.0 as usize).copied();
        if declared_owner.is_some_and(|owner| owner != reader) {
            self.local_captured.insert(local.0 as usize);
            if is_write {
                self.local_written_from_closure.insert(local.0 as usize);
            }
        }
    }
}

struct ScanCtx<'h, 'a> {
    ctx: &'a Context,
    hir: &'h HirModule,
    facts: &'a SourceFacts,
    types: &'a mut TypeTable,
}
