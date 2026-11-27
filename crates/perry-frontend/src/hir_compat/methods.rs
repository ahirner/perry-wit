//! Normalize array and string method calls into uniform AST call shapes before HIR lowering.
use perry_parser::swc_ecma_ast as ast;
use swc_common::{DUMMY_SP, util::take::Take};
use swc_ecma_utils::ExprFactory;
use swc_ecma_visit::{VisitMut, VisitMutWith};

pub(crate) enum Preservation {
    Upstream,
    Always,
    Push,
    Arguments,
    Options,
}

impl Preservation {
    pub(crate) fn needed(&self, count: usize, spread: bool) -> bool {
        match self {
            Self::Upstream => false,
            Self::Always => true,
            Self::Push => count != 1 || spread,
            Self::Arguments => count > 0,
            Self::Options => count > 1,
        }
    }
}

/// Preserve call operands and array-rest bindings across Perry's HIR rewrites.
pub(super) fn preserve_arguments(module: &mut ast::Module, analysis: &mut crate::source::Analysis) {
    struct Arguments<'n> {
        analysis: &'n mut crate::source::Analysis,
    }
    impl VisitMut for Arguments<'_> {
        fn visit_mut_var_decl(&mut self, var_decl: &mut ast::VarDecl) {
            let mut declarations = Vec::with_capacity(var_decl.decls.len());
            for mut decl in var_decl.decls.take() {
                if let ast::Pat::Array(pattern) = &mut decl.name
                    && let Some(init) = &mut decl.init
                    && let Some(Some(ast::Pat::Rest(rest))) = pattern.elems.last_mut()
                {
                    let mut name = *rest.arg.take();
                    if let ast::Pat::Ident(binding) = &mut name
                        && binding.type_ann.is_none()
                    {
                        binding.type_ann = rest.type_ann.take();
                    }
                    pattern.elems.pop();
                    let start = pattern.elems.len();
                    let span = decl.span;
                    let source = if start == 0 {
                        init.take()
                    } else {
                        // A default in the prefix can reassign even an identifier initializer.
                        let temp = ast::Ident::new_no_ctxt(
                            self.analysis.names.prefix("arr_rest").into(),
                            DUMMY_SP,
                        );
                        declarations.push(ast::VarDeclarator {
                            span: DUMMY_SP,
                            name: ast::BindingIdent {
                                id: temp.clone(),
                                type_ann: pattern.type_ann.take(),
                            }
                            .into(),
                            init: Some(std::mem::replace(init, temp.clone().into())),
                            definite: false,
                        });
                        declarations.push(decl);
                        temp.into()
                    };
                    let slice = source
                        .make_member("slice".into())
                        .as_call(DUMMY_SP, vec![(start as f64).as_arg()]);
                    declarations.push(ast::VarDeclarator {
                        span,
                        name,
                        init: Some(slice.into()),
                        definite: false,
                    });
                } else {
                    declarations.push(decl);
                }
            }
            var_decl.decls = declarations;
            var_decl.visit_mut_children_with(self);
        }

        fn visit_mut_call_expr(&mut self, call: &mut ast::CallExpr) {
            call.visit_mut_children_with(self);
            if let ast::Callee::Expr(callee) = &mut call.callee
                && let ast::Expr::Member(member) = &mut **callee
                && let ast::MemberProp::Ident(method) = &mut member.prop
                && ((call.type_args.is_some()
                    && matches!(member.obj.as_ref(), ast::Expr::Ident(id) if self.analysis.classes.contains(&id.to_id())))
                    || crate::methods::Method::preserve_arguments(
                        method.sym.as_ref(),
                        call.args.len(),
                        call.args.iter().any(|argument| argument.spread.is_some()),
                    ))
            {
                // A parenthesized member retains its receiver and arguments when Perry
                // would otherwise replace the call with a fixed-arity builtin.
                **callee = ast::Expr::Paren(ast::ParenExpr {
                    span: member.span,
                    expr: callee.take(),
                });
            }
        }
    }
    module.visit_mut_with(&mut Arguments { analysis });
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method<'a> {
    UsingCheck,
    Push,
    Ordinary(&'a str),
}

pub(crate) fn method(name: &str) -> Method<'_> {
    match name {
        "__perry_using_check__" => Method::UsingCheck,
        "push" => Method::Push,
        name => Method::Ordinary(name),
    }
}

impl<'a> Method<'a> {
    pub(crate) fn name(self) -> &'a str {
        match self {
            Self::UsingCheck => "using",
            Self::Push => "push",
            Self::Ordinary(name) => name,
        }
    }
}
