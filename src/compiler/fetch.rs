//! Preserve complete fetch options before Perry's specialized lowering drops them.

use perry_parser::swc_ecma_ast::{
    CallExpr, Callee, Expr, IdentName, MemberExpr, MemberProp, Module,
};
use swc_ecma_visit::{VisitMut, VisitMutWith};

pub(super) fn preserve_options(module: &mut Module) {
    module.visit_mut_with(&mut FetchCalls);
}

struct FetchCalls;

impl VisitMut for FetchCalls {
    fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
        call.visit_mut_children_with(self);
        if let Callee::Expr(callee) = &mut call.callee
            && let Expr::Ident(ident) = callee.as_ref()
            && ident.sym == "fetch"
        {
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
        }
    }
}
