//! Parse TypeScript with diagnostics before HIR lowering.
use super::*;
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::unsupported;
use std::collections::HashSet;
use swc_common::Span;
use swc_ecma_visit::{Visit, VisitWith};

/// Validate string literals / `String.fromCodePoint` and collect untyped variable bindings in one pass.
pub(crate) fn validate(
    module: &ast::Module,
    locations: &locations::Locations<'_>,
) -> Result<HashSet<(u32, u32)>> {
    #[derive(Default)]
    struct Analyzer {
        invalid: Option<Span>,
        code_point_arity: Option<Span>,
        enum_decl: Option<Span>,
        bindings: HashSet<(u32, u32)>,
    }
    impl Visit for Analyzer {
        fn visit_ts_enum_decl(&mut self, decl: &ast::TsEnumDecl) {
            if !decl.declare {
                self.enum_decl.get_or_insert(decl.span);
            }
        }
        fn visit_str(&mut self, string: &ast::Str) {
            if string.value.as_str().is_none() {
                self.invalid.get_or_insert(string.span);
            }
        }
        fn visit_tpl_element(&mut self, text: &ast::TplElement) {
            if text
                .cooked
                .as_ref()
                .is_some_and(|value| value.as_str().is_none())
            {
                self.invalid.get_or_insert(text.span);
            }
        }
        fn visit_call_expr(&mut self, call: &ast::CallExpr) {
            if matches!(&call.callee, ast::Callee::Expr(callee) if matches!(&**callee, ast::Expr::Member(member) if matches!(&*member.obj, ast::Expr::Ident(id) if id.sym == "String") && matches!(&member.prop, ast::MemberProp::Ident(id) if id.sym == "fromCodePoint")))
                && (call.args.len() != 1 || call.args.iter().any(|arg| arg.spread.is_some()))
            {
                self.code_point_arity.get_or_insert(call.span);
            }
            call.visit_children_with(self);
        }
        fn visit_var_declarator(&mut self, declaration: &ast::VarDeclarator) {
            if declaration.init.is_some() {
                pattern_bindings(&declaration.name, false, &mut |id, annotated| {
                    if !annotated {
                        self.bindings.insert((id.span.lo.0, id.span.hi.0));
                    }
                });
            }
            declaration.visit_children_with(self);
        }
    }
    let mut analyzer = Analyzer::default();
    module.visit_with(&mut analyzer);
    if let Some(span) = analyzer.invalid {
        return Err(locations.annotate(
            unsupported!(
                "lone UTF-16 surrogate in a string literal; Perry strings contain Unicode scalars only"
            ),
            span,
        ));
    }
    if let Some(span) = analyzer.code_point_arity {
        return Err(locations.annotate(
            unsupported!("String.fromCodePoint currently requires exactly one scalar argument"),
            span,
        ));
    }
    if let Some(span) = analyzer.enum_decl {
        return Err(locations.annotate(
            unsupported!(
                "TypeScript enum declarations are not supported yet; use a union of string or number literals instead"
            ),
            span,
        ));
    }
    Ok(analyzer.bindings)
}

/// Visit the declared names and whether a type annotation covers each binding.
pub(crate) fn pattern_bindings<'a>(
    pat: &'a ast::Pat,
    annotated: bool,
    visit: &mut impl FnMut(&'a ast::Ident, bool),
) {
    match pat {
        ast::Pat::Ident(binding) => visit(&binding.id, annotated || binding.type_ann.is_some()),
        ast::Pat::Array(array) => {
            for element in array.elems.iter().flatten() {
                pattern_bindings(element, annotated || array.type_ann.is_some(), visit);
            }
        }
        ast::Pat::Object(object) => {
            let annotated = annotated || object.type_ann.is_some();
            for prop in &object.props {
                match prop {
                    ast::ObjectPatProp::KeyValue(kv) => {
                        pattern_bindings(&kv.value, annotated, visit)
                    }
                    ast::ObjectPatProp::Assign(assign) => visit(&assign.key.id, annotated),
                    ast::ObjectPatProp::Rest(rest) => pattern_bindings(&rest.arg, annotated, visit),
                }
            }
        }
        ast::Pat::Rest(rest) => {
            pattern_bindings(&rest.arg, annotated || rest.type_ann.is_some(), visit)
        }
        ast::Pat::Assign(assign) => pattern_bindings(&assign.left, annotated, visit),
        ast::Pat::Invalid(_) | ast::Pat::Expr(_) => {}
    }
}

pub(super) struct PrivateParameters {
    scope: swc_common::SyntaxContext,
    arity: usize,
    indices: Vec<usize>,
    called: bool,
}

type PrivateFunctions = HashMap<swc_atoms::Atom, PrivateParameters>;

/// Retain private declarations used only as the direct callee of fixed-arity calls.
pub(super) fn private_parameters(module: &ast::Module) -> PrivateFunctions {
    struct Uses {
        functions: PrivateFunctions,
    }
    impl Visit for Uses {
        fn visit_fn_decl(&mut self, decl: &ast::FnDecl) {
            decl.function.visit_with(self);
        }
        fn visit_ident(&mut self, ident: &ast::Ident) {
            if self
                .functions
                .get(&ident.sym)
                .is_some_and(|f| f.scope == ident.ctxt)
            {
                self.functions.remove(&ident.sym);
            }
        }
        fn visit_call_expr(&mut self, call: &ast::CallExpr) {
            if call.type_args.is_none()
                && let ast::Callee::Expr(callee) = &call.callee
                && let ast::Expr::Ident(ident) = &**callee
                && let Some(function) = self.functions.get_mut(&ident.sym)
                && function.scope == ident.ctxt
                && function.arity == call.args.len()
                && call.args.iter().all(|arg| arg.spread.is_none())
            {
                function.called = true;
                call.args.visit_with(self);
            } else {
                call.visit_children_with(self);
            }
        }
    }
    let mut uses = Uses {
        functions: HashMap::new(),
    };
    'functions: for item in &module.body {
        let ast::ModuleItem::Stmt(ast::Stmt::Decl(ast::Decl::Fn(decl))) = item else {
            continue;
        };
        if decl.function.type_params.is_some() {
            continue;
        }
        let mut indices = Vec::new();
        for (index, param) in decl.function.params.iter().enumerate() {
            let ast::Pat::Ident(binding) = &param.pat else {
                continue 'functions;
            };
            if binding.id.optional {
                continue 'functions;
            }
            if binding.type_ann.is_none() {
                indices.push(index);
            }
        }
        if !indices.is_empty() {
            uses.functions.insert(
                decl.ident.sym.clone(),
                PrivateParameters {
                    scope: decl.ident.ctxt,
                    arity: decl.function.params.len(),
                    indices,
                    called: false,
                },
            );
        }
    }
    if !uses.functions.is_empty() {
        module.visit_with(&mut uses);
    }
    uses.functions.retain(|_, f| f.called);
    uses.functions
}

/// Mark inferred parameters for the same specialization as explicit TypeScript generics.
pub(super) fn promote_private_parameters(module: &mut HirModule, mut source: PrivateFunctions) {
    if source.is_empty() {
        return;
    }
    for function in module.functions.iter_mut().filter(|f| !f.is_exported) {
        if let Some(params) = source.remove(&swc_atoms::Atom::from(function.name.as_str()))
            && function.params.len() == params.arity
        {
            for index in params.indices {
                if let Some(param) = function.params.get_mut(index)
                    && matches!(param.ty, HirType::Any | HirType::Unknown)
                {
                    let name = format!("@{}", param.name);
                    param.ty = HirType::TypeVar(name.clone());
                    function.type_params.push(perry_hir::types::TypeParam {
                        name,
                        constraint: None,
                        default: None,
                    });
                }
            }
        }
    }
}
