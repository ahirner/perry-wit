//! Preserve complete fetch options before Perry's specialized lowering drops them.

use perry_parser::swc_ecma_ast::{
    CallExpr, Callee, Expr, IdentName, MemberExpr, MemberProp, Module,
};
use swc_ecma_visit::{VisitMut, VisitMutWith};

pub(super) fn preserve_options(module: &mut Module) {
    module.visit_mut_with(&mut PreservedCalls);
}

struct PreservedCalls;

impl VisitMut for PreservedCalls {
    fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
        call.visit_mut_children_with(self);
        if let Callee::Expr(callee) = &mut call.callee {
            if let Expr::Ident(ident) = callee.as_ref() {
                if ident.sym == "fetch" {
                    let mut receiver = ident.clone();
                    receiver.sym = "undefined".into();
                    **callee = Expr::Member(MemberExpr {
                        span: ident.span,
                        obj: Box::new(Expr::Ident(receiver)),
                        prop: MemberProp::Ident(IdentName {
                            span: ident.span,
                            sym: "fetch_request".into(),
                        }),
                    });
                    return;
                }
            }

            let unlink_ident = match callee.as_ref() {
                Expr::Ident(ident) if ident.sym == "unlinkSync" => Some(ident.clone()),
                Expr::Member(member) => match &member.prop {
                    MemberProp::Ident(ident) if ident.sym == "unlinkSync" => {
                        Some(perry_parser::swc_ecma_ast::Ident {
                            span: ident.span,
                            sym: ident.sym.clone(),
                            ctxt: Default::default(),
                            optional: false,
                        })
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(mut ident) = unlink_ident {
                let span = ident.span;
                ident.sym = "undefined".into();
                **callee = Expr::Member(MemberExpr {
                    span,
                    obj: Box::new(Expr::Ident(ident)),
                    prop: MemberProp::Ident(IdentName {
                        span,
                        sym: "fs_unlink_sync".into(),
                    }),
                });
            }
        }
    }
}
