//! AST pass rewriting performance.now() to (undefined).performance_now().

use perry_parser::swc_ecma_ast::{CallExpr, Callee, Expr, IdentName, MemberProp, Module};
use swc_ecma_visit::{VisitMut, VisitMutWith};

pub(super) fn rewrite_performance_now(module: &mut Module) {
    module.visit_mut_with(&mut PerformanceNowCalls);
}

struct PerformanceNowCalls;

impl VisitMut for PerformanceNowCalls {
    fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
        call.visit_mut_children_with(self);
        if let Callee::Expr(callee) = &mut call.callee
            && let Expr::Member(member) = callee.as_mut()
            && let Expr::Ident(obj) = member.obj.as_ref()
            && obj.sym == "performance"
            && let MemberProp::Ident(prop) = &member.prop
            && prop.sym == "now"
        {
            let mut receiver = obj.clone();
            receiver.sym = "undefined".into();
            *member.obj = Expr::Ident(receiver);
            member.prop = MemberProp::Ident(IdentName {
                span: prop.span,
                sym: "performance_now".into(),
            });
        }
    }
}
