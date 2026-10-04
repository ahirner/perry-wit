//! Preserve Date constructor and method arguments before builtin HIR folding.

use super::{SourceCalls, underlying_expression};
use anyhow::{Result, ensure};
use perry_parser::swc_ecma_ast as ast;
use swc_common::SyntaxContext;

impl SourceCalls {
    pub(super) fn rewrite_date_constructor(&mut self, expression: &mut ast::Expr) -> Result<()> {
        let ast::Expr::New(constructor) = expression else {
            return Ok(());
        };
        if !matches!(underlying_expression(&constructor.callee), ast::Expr::Ident(name) if name.sym == "Date" && name.ctxt == self.unresolved)
        {
            return Ok(());
        }
        let arguments = constructor.args.as_deref().unwrap_or_default();
        ensure!(
            arguments.len() == 1 && constructor.type_args.is_none(),
            "Date construction requires one numeric epoch-millisecond argument; use Date.now() for the current time"
        );
        ensure!(
            arguments.iter().all(|argument| argument.spread.is_none()),
            "Spread Date arguments are unsupported"
        );
        let name = if let Some(name) = &self.date_constructor {
            name.clone()
        } else {
            let name = self.fresh_name();
            self.date_constructor = Some(name.clone());
            name
        };
        let args = constructor.args.take().unwrap_or_default();
        *expression = ast::Expr::Call(ast::CallExpr {
            span: constructor.span,
            ctxt: SyntaxContext::empty(),
            callee: ast::Callee::Expr(Box::new(ast::Expr::Ident(ast::Ident::new(
                name.into(),
                constructor.span,
                SyntaxContext::empty(),
            )))),
            args,
            type_args: None,
        });
        Ok(())
    }
}
