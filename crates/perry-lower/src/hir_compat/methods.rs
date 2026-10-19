//! Preserve method arguments that upstream specializations discard or reorder.
use super::*;
use perry_parser::swc_ecma_ast as ast;
use swc_common::{DUMMY_SP, SyntaxContext};
use swc_ecma_visit::{VisitMut, VisitMutWith};

use crate::methods::{CONCAT, METHOD_PREFIX, PUSH};

fn slice_call(obj: Box<ast::Expr>, start: usize) -> Box<ast::Expr> {
    Box::new(ast::Expr::Call(ast::CallExpr {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        callee: ast::Callee::Expr(Box::new(ast::Expr::Member(ast::MemberExpr {
            span: DUMMY_SP,
            obj,
            prop: ast::MemberProp::Ident(ast::IdentName::new("slice".into(), DUMMY_SP)),
        }))),
        args: vec![ast::ExprOrSpread {
            spread: None,
            expr: Box::new(ast::Expr::Lit(ast::Lit::Num(ast::Number {
                span: DUMMY_SP,
                value: start as f64,
                raw: None,
            }))),
        }],
        type_args: None,
    }))
}

/// Keep arguments together before HIR expands a variadic push into separate mutations.
pub(super) fn preserve_arguments(module: &mut ast::Module) {
    #[derive(Default)]
    struct Arguments {
        rest_counter: usize,
    }
    impl VisitMut for Arguments {
        fn visit_mut_var_decl(&mut self, var_decl: &mut ast::VarDecl) {
            let mut new_decls = Vec::with_capacity(var_decl.decls.len());
            for mut decl in std::mem::take(&mut var_decl.decls) {
                let rest_pat = match (&mut decl.name, decl.init.is_some()) {
                    (ast::Pat::Array(arr_pat), true)
                        if matches!(arr_pat.elems.last(), Some(Some(ast::Pat::Rest(_)))) =>
                    {
                        match arr_pat.elems.pop().flatten() {
                            Some(ast::Pat::Rest(rest)) => Some((arr_pat.elems.len(), rest)),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if let Some((rest_index, rest_pat)) = rest_pat
                    && let Some(init) = decl.init.take()
                {
                    let mut rest_name = *rest_pat.arg;
                    if let ast::Pat::Ident(binding) = &mut rest_name
                        && binding.type_ann.is_none()
                    {
                        binding.type_ann = rest_pat.type_ann;
                    }
                    if rest_index == 0 {
                        new_decls.push(ast::VarDeclarator {
                            span: decl.span,
                            name: rest_name,
                            init: Some(slice_call(init, 0)),
                            definite: false,
                        });
                    } else if let ast::Expr::Ident(src_ident) = *init {
                        decl.init = Some(Box::new(ast::Expr::Ident(src_ident.clone())));
                        new_decls.push(decl);
                        new_decls.push(ast::VarDeclarator {
                            span: DUMMY_SP,
                            name: rest_name,
                            init: Some(slice_call(
                                Box::new(ast::Expr::Ident(src_ident)),
                                rest_index,
                            )),
                            definite: false,
                        });
                    } else {
                        self.rest_counter += 1;
                        let temp_ident = ast::Ident::new(
                            format!("__perry_arr_rest_{}", self.rest_counter).into(),
                            DUMMY_SP,
                            SyntaxContext::empty(),
                        );
                        let temp_type_ann = match &mut decl.name {
                            ast::Pat::Array(arr_pat) => arr_pat.type_ann.take(),
                            _ => None,
                        };
                        new_decls.push(ast::VarDeclarator {
                            span: DUMMY_SP,
                            name: ast::Pat::Ident(ast::BindingIdent {
                                id: temp_ident.clone(),
                                type_ann: temp_type_ann,
                            }),
                            init: Some(init),
                            definite: false,
                        });
                        decl.init = Some(Box::new(ast::Expr::Ident(temp_ident.clone())));
                        new_decls.push(decl);
                        new_decls.push(ast::VarDeclarator {
                            span: DUMMY_SP,
                            name: rest_name,
                            init: Some(slice_call(
                                Box::new(ast::Expr::Ident(temp_ident)),
                                rest_index,
                            )),
                            definite: false,
                        });
                    }
                } else {
                    new_decls.push(decl);
                }
            }
            var_decl.decls = new_decls;
            var_decl.visit_mut_children_with(self);
        }

        fn visit_mut_call_expr(&mut self, call: &mut ast::CallExpr) {
            call.visit_mut_children_with(self);
            if let ast::Callee::Expr(callee) = &mut call.callee
                && let ast::Expr::Member(member) = &mut **callee
                && let ast::MemberProp::Ident(method) = &mut member.prop
                && let Some(name) = crate::methods::Method::rewrite(
                    method.sym.as_ref(),
                    call.args.len(),
                    call.args.iter().any(|argument| argument.spread.is_some()),
                )
            {
                method.sym = name.into();
            }
        }
    }
    module.visit_mut_with(&mut Arguments::default());
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method<'a> {
    Iteration,
    Push,
    Concat,
    Decode,
    Ordinary(&'a str),
}

pub(crate) fn method(name: &str) -> Method<'_> {
    if let Some(rest) = name.strip_prefix(METHOD_PREFIX) {
        return Method::Ordinary(rest);
    }
    match name {
        streams::ITERATION => Method::Iteration,
        PUSH => Method::Push,
        CONCAT => Method::Concat,
        decoder::DECODE => Method::Decode,
        name => Method::Ordinary(name),
    }
}

impl<'a> Method<'a> {
    pub(crate) fn name(self) -> &'a str {
        match self {
            Self::Iteration => "[iteration]",
            Self::Push => "push",
            Self::Concat => "concat",
            Self::Decode => "decode",
            Self::Ordinary(name) => name,
        }
    }
}
