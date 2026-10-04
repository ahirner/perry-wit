//! Preserve object operation arguments before builtin HIR lowering.

use super::{SourceCalls, underlying_expression};
use anyhow::{Result, ensure};
use perry_parser::swc_ecma_ast as ast;

impl SourceCalls {
    pub(super) fn validate_object_call(&self, call: &ast::CallExpr) -> Result<()> {
        let ast::Callee::Expr(callee) = &call.callee else {
            return Ok(());
        };
        let ast::Expr::Member(member) = underlying_expression(callee) else {
            return Ok(());
        };
        let ast::Expr::Ident(receiver) = underlying_expression(&member.obj) else {
            return Ok(());
        };
        if receiver.ctxt != self.unresolved || !matches!(receiver.sym.as_ref(), "Object" | "Array")
        {
            return Ok(());
        }
        let name = match &member.prop {
            ast::MemberProp::Ident(name) => Some(name.sym.as_ref()),
            ast::MemberProp::Computed(key) => match underlying_expression(&key.expr) {
                ast::Expr::Lit(ast::Lit::Str(text)) => text.value.as_str(),
                _ => None,
            },
            _ => None,
        };
        if receiver.sym == "Array" {
            if name == Some("isArray") {
                ensure!(
                    call.args.len() == 1 && call.args[0].spread.is_none(),
                    "Array.isArray requires one non-spread argument"
                );
            }
            return Ok(());
        }
        if matches!(name, Some("keys" | "values")) {
            ensure!(
                call.args.len() == 1 && call.args[0].spread.is_none(),
                "Object enumeration requires one non-spread argument"
            );
        }
        if name == Some("assign") {
            ensure!(
                !call.args.is_empty() && call.args.iter().all(|argument| argument.spread.is_none()),
                "Object.assign requires a target and non-spread arguments"
            );
        }
        Ok(())
    }
}
