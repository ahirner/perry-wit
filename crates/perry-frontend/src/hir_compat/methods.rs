//! Normalize array and string method calls into uniform AST call shapes before HIR lowering.
use perry_parser::swc_ecma_ast as ast;
use std::collections::HashSet;
use swc_common::{Spanned, util::take::Take};
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

pub(super) fn preserve_arguments(module: &mut ast::Module, classes: &HashSet<ast::Id>) {
    struct Arguments<'a> {
        classes: &'a HashSet<ast::Id>,
    }
    impl VisitMut for Arguments<'_> {
        fn visit_mut_call_expr(&mut self, call: &mut ast::CallExpr) {
            call.visit_mut_children_with(self);
            let ast::Callee::Expr(callee) = &mut call.callee else {
                return;
            };
            // Perry's microtask intrinsics discard the call site. An ordinary call retains it.
            let preserve = match &**callee {
                ast::Expr::Ident(id) => id.sym == "queueMicrotask",
                ast::Expr::Member(member) if let ast::MemberProp::Ident(method) = &member.prop => {
                    method.sym == "nextTick"
                        && matches!(&*member.obj, ast::Expr::Ident(_))
                        && !call.args.is_empty()
                        || call.type_args.is_some()
                            && matches!(member.obj.as_ref(), ast::Expr::Ident(id) if self.classes.contains(&id.to_id()))
                        || crate::methods::Method::preserve_arguments(
                            method.sym.as_ref(),
                            call.args.len(),
                            call.args.iter().any(|arg| arg.spread.is_some()),
                        )
                }
                _ => false,
            };
            if preserve {
                **callee = ast::ParenExpr {
                    span: callee.span(),
                    expr: callee.take(),
                }
                .into();
            }
        }
    }
    module.visit_mut_with(&mut Arguments { classes });
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
