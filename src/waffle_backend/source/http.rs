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
        let (minimum, maximum, existing) = match name.sym.as_ref() {
            "AbortController" => (0, 0, &mut self.abort_constructor),
            "Headers" => (0, 1, &mut self.headers_constructor),
            "Request" => (1, 2, &mut self.request_constructor),
            "Response" => (0, 2, &mut self.response_constructor),
            _ => return Ok(()),
        };
        let arguments = constructor.args.as_deref().unwrap_or_default();
        ensure!(
            (minimum..=maximum).contains(&arguments.len())
                && constructor.type_args.is_none()
                && arguments.iter().all(|argument| argument.spread.is_none()),
            "{} constructor received unsupported arguments",
            name.sym
        );
        let name = if let Some(name) = existing {
            name.clone()
        } else {
            let symbol = name.sym.clone();
            let name = self.fresh_name();
            match symbol.as_ref() {
                "AbortController" => self.abort_constructor = Some(name.clone()),
                "Headers" => self.headers_constructor = Some(name.clone()),
                "Request" => self.request_constructor = Some(name.clone()),
                "Response" => self.response_constructor = Some(name.clone()),
                _ => unreachable!(),
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
