//! Lower byte-stream async iteration to the supported reader API before HIR erases types.
use anyhow::{Result, anyhow, ensure};
use perry_parser::swc_ecma_ast as ast;
use std::collections::HashSet;
use swc_common::{DUMMY_SP, Span};
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};

pub(super) fn lower(module: &mut ast::Module) -> Result<()> {
    #[derive(Default)]
    struct Names(HashSet<String>);
    impl Visit for Names {
        fn visit_ident(&mut self, ident: &ast::Ident) {
            self.0.insert(ident.sym.to_string());
        }
    }
    let mut names = Names::default();
    module.visit_with(&mut names);
    let mut lower = Loops {
        names: names.0,
        next: 0,
        error: None,
    };
    module.visit_mut_with(&mut lower);
    match lower.error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

struct Loops {
    names: HashSet<String>,
    next: usize,
    error: Option<anyhow::Error>,
}

impl Loops {
    fn rewrite(&mut self, iteration: ast::ForOfStmt) -> Result<ast::Stmt> {
        let ast::ForHead::VarDecl(declaration) = iteration.left else {
            return Err(anyhow!("for await requires a const or let binding"));
        };
        ensure!(
            declaration.decls.len() == 1 && declaration.kind != ast::VarDeclKind::Var,
            "for await requires one const or let binding"
        );
        let kind = declaration.kind;
        let binding = declaration
            .decls
            .into_iter()
            .next()
            .expect("one binding")
            .name;
        let prefix = loop {
            self.next += 1;
            let prefix = format!("__perry_stream_{}_", self.next);
            if !self.names.iter().any(|name| name.starts_with(&prefix)) {
                break prefix;
            }
        };
        let template = format!(
            r#"{{
            const {prefix}reader = {prefix}source.getReader();
            let {prefix}done = false;
            try {{
                while (true) {{
                    const {prefix}step = await {prefix}reader.read();
                    if ({prefix}step.done) {{ {prefix}done = true; break; }}
                    const {prefix}item = {prefix}step.value;
                    {prefix}body();
                }}
            }} finally {{
                try {{ if (!{prefix}done) await {prefix}reader.cancel(); }}
                finally {{ {prefix}reader.releaseLock(); }}
            }}
        }}"#
        );
        let mut parsed = perry_parser::parse_typescript(&template, "stream-iteration.ts")
            .map_err(|e| anyhow!("{e:?}"))?;
        struct SyntheticSpans;
        impl VisitMut for SyntheticSpans {
            fn visit_mut_span(&mut self, span: &mut Span) {
                *span = DUMMY_SP;
            }
        }
        parsed.visit_mut_with(&mut SyntheticSpans);
        let Some(ast::ModuleItem::Stmt(mut statement)) = parsed.body.pop() else {
            unreachable!("block template")
        };
        let mut fill = Fill {
            prefix,
            source: Some(iteration.right),
            body: Some(*iteration.body),
            binding: Some(binding),
            kind,
        };
        statement.visit_mut_with(&mut fill);
        Ok(statement)
    }
}

impl VisitMut for Loops {
    fn visit_mut_stmt(&mut self, statement: &mut ast::Stmt) {
        statement.visit_mut_children_with(self);
        if !matches!(statement, ast::Stmt::ForOf(iteration) if iteration.is_await) {
            return;
        }
        let ast::Stmt::ForOf(iteration) = std::mem::replace(
            statement,
            ast::Stmt::Empty(ast::EmptyStmt { span: DUMMY_SP }),
        ) else {
            unreachable!()
        };
        match self.rewrite(iteration) {
            Ok(replacement) => *statement = replacement,
            Err(error) => {
                self.error.get_or_insert(error);
            }
        }
    }
}

struct Fill {
    prefix: String,
    source: Option<Box<ast::Expr>>,
    body: Option<ast::Stmt>,
    binding: Option<ast::Pat>,
    kind: ast::VarDeclKind,
}

impl VisitMut for Fill {
    fn visit_mut_expr(&mut self, expression: &mut ast::Expr) {
        if matches!(expression, ast::Expr::Ident(id) if id.sym.as_ref() == format!("{}source", self.prefix))
        {
            *expression = *self.source.take().expect("one source placeholder");
        } else {
            expression.visit_mut_children_with(self);
        }
    }
    fn visit_mut_var_decl(&mut self, declaration: &mut ast::VarDecl) {
        if declaration.decls.iter().any(|decl| matches!(&decl.name, ast::Pat::Ident(id) if id.id.sym.as_ref() == format!("{}item", self.prefix))) {
            declaration.kind = self.kind;
            declaration.decls[0].name = self.binding.take().expect("one binding placeholder");
        } else { declaration.visit_mut_children_with(self); }
    }
    fn visit_mut_stmt(&mut self, statement: &mut ast::Stmt) {
        if matches!(statement, ast::Stmt::Expr(expr) if matches!(&*expr.expr, ast::Expr::Call(call) if matches!(&call.callee, ast::Callee::Expr(callee) if matches!(&**callee, ast::Expr::Ident(id) if id.sym.as_ref() == format!("{}body", self.prefix)))))
        {
            *statement = self.body.take().expect("one body placeholder");
        } else {
            statement.visit_mut_children_with(self);
        }
    }
}
