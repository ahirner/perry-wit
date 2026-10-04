//! Resolve process context properties before Perry's builtin lowering.

use super::{SourceCalls, underlying_expression};
use crate::waffle_backend::capabilities::{CapabilityOperation, ContextOperation};
use perry_parser::swc_ecma_ast as ast;
use swc_common::SyntaxContext;

impl SourceCalls {
    pub(super) fn rewrite_process_value(&mut self, expression: &mut ast::Expr) {
        let ast::Expr::Member(member) = expression else {
            return;
        };
        if !matches!(underlying_expression(&member.obj), ast::Expr::Ident(name) if name.sym == "process" && name.ctxt == self.unresolved)
        {
            return;
        }
        let property = match &member.prop {
            ast::MemberProp::Ident(name) => Some(name.sym.as_ref()),
            ast::MemberProp::Computed(key) => match underlying_expression(&key.expr) {
                ast::Expr::Lit(ast::Lit::Str(text)) => text.value.as_str(),
                _ => None,
            },
            _ => None,
        };
        let operation = match property {
            Some("argv") => ContextOperation::Arguments,
            Some("env") => ContextOperation::Environment,
            _ => return,
        };
        let name = self.capability_name(CapabilityOperation::Context(operation));
        *expression = ast::Expr::Call(ast::CallExpr {
            span: member.span,
            ctxt: SyntaxContext::empty(),
            callee: ast::Callee::Expr(Box::new(ast::Expr::Ident(ast::Ident::new(
                name.into(),
                member.span,
                SyntaxContext::empty(),
            )))),
            args: vec![],
            type_args: None,
        });
    }
}
