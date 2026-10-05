//! Resolve standard HTTP constructors before name-based HIR lowering.
use super::{SourceCalls, underlying_expression};
use anyhow::{Result, ensure};
use perry_parser::swc_ecma_ast as ast;
use swc_common::SyntaxContext;

impl SourceCalls {
    pub(super) fn rewrite_http_constructor(&mut self, expression: &mut ast::Expr) -> Result<()> {
        let ast::Expr::New(constructor) = expression else {
            return Ok(());
        };
        let ast::Expr::Ident(name) = underlying_expression(&constructor.callee) else {
            return Ok(());
        };
        if name.ctxt != self.unresolved {
            return Ok(());
        }
        let request = match name.sym.as_ref() {
            "Headers" => false,
            "Request" => true,
            _ => return Ok(()),
        };
        let arguments = constructor.args.as_deref().unwrap_or_default();
        ensure!(
            arguments.len() <= if request { 2 } else { 1 }
                && (!request || !arguments.is_empty())
                && constructor.type_args.is_none()
                && arguments.iter().all(|argument| argument.spread.is_none()),
            "{} constructor received unsupported arguments",
            name.sym
        );
        let existing = if request {
            &self.request_constructor
        } else {
            &self.headers_constructor
        };
        let name = if let Some(name) = existing {
            name.clone()
        } else {
            let name = self.fresh_name();
            if request {
                self.request_constructor = Some(name.clone());
            } else {
                self.headers_constructor = Some(name.clone());
            }
            name
        };
        *expression = ast::Expr::Call(ast::CallExpr {
            span: constructor.span,
            ctxt: SyntaxContext::empty(),
            callee: ast::Callee::Expr(Box::new(ast::Expr::Ident(ast::Ident::new(
                name.into(),
                constructor.span,
                SyntaxContext::empty(),
            )))),
            args: constructor.args.take().unwrap_or_default(),
            type_args: None,
        });
        Ok(())
    }
}
