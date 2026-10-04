//! Resolve process context properties before Perry's builtin lowering.

use super::{SourceCalls, underlying_expression};
use crate::waffle_backend::capabilities::{
    CapabilityOperation, ContextOperation, ProcessOperation,
};
use perry_parser::swc_ecma_ast as ast;
use swc_common::SyntaxContext;

pub(super) fn process_property(
    member: &ast::MemberExpr,
    unresolved: SyntaxContext,
) -> Option<&str> {
    if !matches!(underlying_expression(&member.obj), ast::Expr::Ident(name) if name.sym == "process" && name.ctxt == unresolved)
    {
        return None;
    }
    match &member.prop {
        ast::MemberProp::Ident(name) => Some(name.sym.as_ref()),
        ast::MemberProp::Computed(key) => match underlying_expression(&key.expr) {
            ast::Expr::Lit(ast::Lit::Str(text)) => text.value.as_str(),
            _ => None,
        },
        _ => None,
    }
}

impl SourceCalls {
    pub(super) fn rewrite_process_assignment(&mut self, expression: &mut ast::Expr) {
        let ast::Expr::Assign(assign) = expression else {
            return;
        };
        let ast::AssignTarget::Simple(target) = &assign.left else {
            return;
        };
        let target: Box<ast::Expr> = target.clone().into();
        let ast::Expr::Member(member) = underlying_expression(&target) else {
            return;
        };
        if process_property(member, self.unresolved) != Some("exitCode") {
            return;
        }
        if assign.op != ast::AssignOp::Assign {
            self.error.get_or_insert_with(|| {
                anyhow::anyhow!(
                    "process.exitCode requires a direct numeric or undefined assignment"
                )
            });
            return;
        }
        let clear = matches!(underlying_expression(&assign.right), ast::Expr::Ident(name) if name.sym == "undefined" && name.ctxt == self.unresolved);
        let operation = if clear {
            ProcessOperation::ClearExitCode
        } else {
            ProcessOperation::SetExitCode
        };
        let name = self.capability_name(CapabilityOperation::Process(operation));
        *expression = ast::Expr::Call(ast::CallExpr {
            span: assign.span,
            ctxt: SyntaxContext::empty(),
            callee: ast::Callee::Expr(Box::new(ast::Expr::Ident(ast::Ident::new(
                name.into(),
                assign.span,
                SyntaxContext::empty(),
            )))),
            args: if clear {
                vec![]
            } else {
                vec![ast::ExprOrSpread {
                    spread: None,
                    expr: assign.right.clone(),
                }]
            },
            type_args: None,
        });
    }

    pub(super) fn rewrite_process_value(&mut self, expression: &mut ast::Expr) {
        let ast::Expr::Member(member) = expression else {
            return;
        };
        let operation = match process_property(member, self.unresolved) {
            Some("argv") => CapabilityOperation::Context(ContextOperation::Arguments),
            Some("env") => CapabilityOperation::Context(ContextOperation::Environment),
            Some("exitCode") => CapabilityOperation::Process(ProcessOperation::GetExitCode),
            _ => return,
        };
        let name = self.capability_name(operation);
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
