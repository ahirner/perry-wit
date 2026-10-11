//! Lower array/string iteration and byte-stream async iteration before HIR erases types.
use anyhow::{Result, anyhow, bail, ensure};
use perry_parser::swc_ecma_ast as ast;
use std::collections::HashSet;
use swc_common::{DUMMY_SP, Span};
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};

/// The synthetic method a synchronous `for...of` calls on its source: lowering checks that
/// the receiver is an array or string and yields it unchanged.
pub(super) const ITERATION: &str = "$$perryIteration";

/// Prefix of the names a rewritten loop declares; numbered past any source name using it.
const LOOP_PREFIX: &str = "__perry_loop_";

/// Rewrite every `for...of` and `for await...of` into the indexed or reader loop it means.
pub(super) fn lower(module: &mut ast::Module) -> Result<()> {
    #[derive(Default)]
    struct Names(HashSet<String>);
    impl Visit for Names {
        fn visit_ident(&mut self, ident: &ast::Ident) {
            // Only names that could collide with generated loop names.
            if ident.sym.starts_with(LOOP_PREFIX) {
                self.0.insert(ident.sym.to_string());
            }
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
    lower.error.map_or(Ok(()), Err)
}

struct Loops {
    names: HashSet<String>,
    next: usize,
    error: Option<anyhow::Error>,
}

impl Loops {
    fn rewrite(&mut self, iteration: ast::ForOfStmt) -> Result<ast::Stmt> {
        let asynchronous = iteration.is_await;
        let ast::ForHead::VarDecl(declaration) = iteration.left else {
            bail!("iteration requires a const or let binding");
        };
        const ONE_BINDING: &str = "iteration requires one const or let binding";
        let kind = declaration.kind;
        ensure!(kind != ast::VarDeclKind::Var, ONE_BINDING);
        let Ok([declarator]) = <[ast::VarDeclarator; 1]>::try_from(declaration.decls) else {
            bail!(ONE_BINDING);
        };
        let binding = declarator.name;
        let prefix = loop {
            self.next += 1;
            let prefix = format!("{LOOP_PREFIX}{}_", self.next);
            if !self.names.iter().any(|name| name.starts_with(&prefix)) {
                break prefix;
            }
        };
        let template = if asynchronous {
            format!(
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
            )
        } else {
            format!(
                r#"{{
            const {prefix}items = {prefix}source.{ITERATION}();
            for (let {prefix}index = 0; {prefix}index < {prefix}items.length; {prefix}index++) {{
                const {prefix}item = {prefix}items[{prefix}index];
                {prefix}body();
            }}
        }}"#
            )
        };
        let mut parsed = perry_parser::parse_typescript(&template, "iteration.ts")
            .map_err(|e| anyhow!("{e:?}"))?;
        struct SyntheticSpans;
        impl VisitMut for SyntheticSpans {
            fn visit_mut_span(&mut self, span: &mut Span) {
                *span = DUMMY_SP;
            }
        }
        parsed.visit_mut_with(&mut SyntheticSpans);
        let Some(ast::ModuleItem::Stmt(mut statement)) = parsed.body.pop() else {
            bail!("the iteration template is one block statement");
        };
        let mut fill = Fill {
            prefix,
            source: Some(iteration.right),
            body: Some(*iteration.body),
            binding: Some(binding),
            kind,
            repeated: false,
        };
        statement.visit_mut_with(&mut fill);
        fill.check()?;
        Ok(statement)
    }
}

impl VisitMut for Loops {
    fn visit_mut_stmt(&mut self, statement: &mut ast::Stmt) {
        statement.visit_mut_children_with(self);
        let empty = ast::Stmt::Empty(ast::EmptyStmt { span: DUMMY_SP });
        *statement = match std::mem::replace(statement, empty) {
            ast::Stmt::ForOf(iteration) => self.rewrite(iteration).unwrap_or_else(|error| {
                self.error.get_or_insert(error);
                ast::Stmt::Empty(ast::EmptyStmt { span: DUMMY_SP })
            }),
            other => other,
        };
    }
}

/// Fills a parsed loop template's placeholders (`<prefix>source`, `<prefix>item` and
/// `<prefix>body()`) with the loop's parts; the user's parts are not revisited.
/// [Fill::check] rejects a template that misses or repeats a placeholder.
struct Fill {
    prefix: String,
    source: Option<Box<ast::Expr>>,
    body: Option<ast::Stmt>,
    binding: Option<ast::Pat>,
    kind: ast::VarDeclKind,
    /// A placeholder appeared after its part was already placed.
    repeated: bool,
}

impl Fill {
    /// Whether `ident` is the template placeholder `<prefix><name>`.
    fn is_placeholder(&self, ident: &ast::Ident, name: &str) -> bool {
        ident.sym.strip_prefix(self.prefix.as_str()) == Some(name)
    }

    /// Every part was placed exactly once.
    fn check(&self) -> Result<()> {
        ensure!(
            !self.repeated
                && self.source.is_none()
                && self.body.is_none()
                && self.binding.is_none(),
            "the iteration template names each placeholder exactly once"
        );
        Ok(())
    }
}

impl VisitMut for Fill {
    fn visit_mut_expr(&mut self, expression: &mut ast::Expr) {
        if let ast::Expr::Ident(id) = expression
            && self.is_placeholder(id, "source")
        {
            match self.source.take() {
                Some(source) => *expression = *source,
                None => self.repeated = true,
            }
        } else {
            expression.visit_mut_children_with(self);
        }
    }

    fn visit_mut_var_decl(&mut self, declaration: &mut ast::VarDecl) {
        if let Some(item) = declaration.decls.iter_mut().find(
            |decl| matches!(&decl.name, ast::Pat::Ident(id) if self.is_placeholder(&id.id, "item")),
        ) {
            match self.binding.take() {
                Some(binding) => {
                    item.name = binding;
                    declaration.kind = self.kind;
                }
                None => self.repeated = true,
            }
        } else {
            declaration.visit_mut_children_with(self);
        }
    }

    fn visit_mut_stmt(&mut self, statement: &mut ast::Stmt) {
        if let ast::Stmt::Expr(expr) = statement
            && let ast::Expr::Call(call) = &*expr.expr
            && let ast::Callee::Expr(callee) = &call.callee
            && let ast::Expr::Ident(id) = &**callee
            && self.is_placeholder(id, "body")
        {
            match self.body.take() {
                Some(body) => *statement = body,
                None => self.repeated = true,
            }
        } else {
            statement.visit_mut_children_with(self);
        }
    }
}
