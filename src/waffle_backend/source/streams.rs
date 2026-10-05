//! Byte-stream iteration uses the same retained reads and cancellation as readers.
use super::{SourceCalls, ast};
use anyhow::{Context, Result, bail, ensure};
use perry_parser::parse_typescript;
use swc_common::SyntaxContext;
use swc_ecma_visit::{VisitMut, VisitMutWith};

impl SourceCalls {
    pub(super) fn rewrite_stream_iteration(&mut self, loop_: &ast::ForOfStmt) -> Result<ast::Stmt> {
        let reader = self.fresh_name();
        let step = self.fresh_name();
        let value = self.fresh_name();
        let completion = self.fresh_name();
        let error = self.fresh_name();
        let cancelled = self.fresh_name();
        let source = self.fresh_name();
        let binding = self.fresh_name();
        let body = self.fresh_name();
        let chunk = Box::new(ast::Expr::Ident(ast::Ident::new(
            value.clone().into(),
            loop_.span,
            SyntaxContext::empty(),
        )));
        let assignment = match &loop_.left {
            ast::ForHead::VarDecl(declaration) => {
                ensure!(
                    declaration.decls.len() == 1,
                    "for await requires one binding"
                );
                let mut declaration = declaration.clone();
                declaration.decls[0].init = Some(chunk);
                ast::Stmt::Decl(ast::Decl::Var(declaration))
            }
            ast::ForHead::Pat(pattern) => ast::Stmt::Expr(ast::ExprStmt {
                span: loop_.span,
                expr: Box::new(ast::Expr::Assign(ast::AssignExpr {
                    span: loop_.span,
                    op: ast::AssignOp::Assign,
                    left: pattern.clone().try_into().map_err(|_| {
                        anyhow::anyhow!("Unsupported assignment target in for await")
                    })?,
                    right: chunk,
                })),
            }),
            _ => bail!("Resource declarations in for await are unsupported"),
        };
        // 0: reading or finished; 1: loop body; 2: exception from the body.
        // AsyncIteratorClose preserves a body's exception over cancellation failure.
        let template = format!(
            "{{
                const {reader} = {source}.getReader();
                let {completion} = 0;
                try {{
                    while (true) {{
                        {completion} = 0;
                        const {step} = await {reader}.read();
                        if ({step}.done) break;
                        const {value} = {step}.value;
                        if ({value} === undefined) throw 0;
                        {completion} = 1;
                        {binding};
                        {body};
                    }}
                }} catch ({error}) {{
                    if ({completion} === 1) {completion} = 2;
                    throw {error};
                }} finally {{
                    try {{
                        if ({completion} !== 0) {{
                            const {cancelled} = {reader}.cancel();
                            {reader}.releaseLock();
                            if ({completion} === 2) {{
                                try {{ await {cancelled}; }} catch {{}}
                            }} else {{
                                await {cancelled};
                            }}
                        }}
                    }} finally {{
                        {reader}.releaseLock();
                    }}
                }}
            }}"
        );
        let mut parsed = parse_typescript(&template, "stream-iteration.ts")?;
        let ast::ModuleItem::Stmt(mut statement) = parsed.body.pop().context("Iteration block")?
        else {
            unreachable!()
        };
        statement.visit_mut_with(&mut IterationParts {
            source: (&source, &loop_.right),
            binding: (&binding, &assignment),
            body: (&body, &loop_.body),
        });
        Ok(statement)
    }
}

struct IterationParts<'a> {
    source: (&'a str, &'a ast::Expr),
    binding: (&'a str, &'a ast::Stmt),
    body: (&'a str, &'a ast::Stmt),
}
impl VisitMut for IterationParts<'_> {
    fn visit_mut_expr(&mut self, expression: &mut ast::Expr) {
        if matches!(expression, ast::Expr::Ident(name) if name.sym == self.source.0) {
            *expression = self.source.1.clone();
        } else {
            expression.visit_mut_children_with(self);
        }
    }
    fn visit_mut_stmt(&mut self, statement: &mut ast::Stmt) {
        for (name, replacement) in [self.binding, self.body] {
            if matches!(statement, ast::Stmt::Expr(value) if matches!(value.expr.as_ref(), ast::Expr::Ident(id) if id.sym == name))
            {
                *statement = replacement.clone();
                return;
            }
        }
        statement.visit_mut_children_with(self);
    }
}
