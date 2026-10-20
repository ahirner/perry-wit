//! Resolve constant RegExp arguments while retaining evaluation of the original arguments.
use perry_parser::swc_ecma_ast as ast;
use std::collections::HashMap;
use swc_common::{Spanned, SyntaxContext};
use swc_ecma_visit::{VisitMut, VisitMutWith};

pub(super) fn fold(module: &mut ast::Module, unresolved: SyntaxContext) {
    module.visit_mut_with(&mut Folder {
        strings: HashMap::new(),
        unresolved,
    });
}

struct Folder {
    strings: HashMap<ast::Id, String>,
    unresolved: SyntaxContext,
}

impl Folder {
    fn remember(&mut self, declaration: &ast::VarDecl) {
        for binding in &declaration.decls {
            if declaration.kind == ast::VarDeclKind::Const
                && let ast::Pat::Ident(id) = &binding.name
                && !self.strings.contains_key(&id.to_id())
                && let Some(value) = binding.init.as_deref().and_then(|expr| self.string(expr))
            {
                self.strings.insert(id.to_id(), value);
            }
        }
    }

    fn string(&self, expr: &ast::Expr) -> Option<String> {
        let mut text = String::new();
        self.append(expr, &mut text)?;
        Some(text)
    }

    fn append(&self, expr: &ast::Expr, text: &mut String) -> Option<()> {
        match expr {
            ast::Expr::Ident(id) => text.push_str(self.strings.get(&id.to_id())?),
            ast::Expr::Lit(ast::Lit::Str(value)) => text.push_str(value.value.as_str()?),
            ast::Expr::Paren(paren) => self.append(&paren.expr, text)?,
            ast::Expr::TsAs(cast) => self.append(&cast.expr, text)?,
            ast::Expr::TsConstAssertion(cast) => self.append(&cast.expr, text)?,
            ast::Expr::TsSatisfies(cast) => self.append(&cast.expr, text)?,
            ast::Expr::Bin(binary) if binary.op == ast::BinaryOp::Add => {
                self.append(&binary.left, text)?;
                self.append(&binary.right, text)?;
            }
            ast::Expr::Tpl(template) => {
                for (index, part) in template.quasis.iter().enumerate() {
                    text.push_str(part.cooked.as_ref()?.as_str()?);
                    if let Some(expr) = template.exprs.get(index) {
                        self.append(expr, text)?;
                    }
                }
            }
            _ => return None,
        }
        Some(())
    }
}

impl VisitMut for Folder {
    fn visit_mut_module(&mut self, module: &mut ast::Module) {
        for item in &module.body {
            let declaration = match item {
                ast::ModuleItem::Stmt(ast::Stmt::Decl(decl)) => decl,
                ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportDecl(export)) => &export.decl,
                _ => continue,
            };
            if let ast::Decl::Var(declaration) = declaration {
                self.remember(declaration);
            }
        }
        module.visit_mut_children_with(self);
    }

    fn visit_mut_stmts(&mut self, statements: &mut Vec<ast::Stmt>) {
        for statement in &*statements {
            if let ast::Stmt::Decl(ast::Decl::Var(declaration)) = statement {
                self.remember(declaration);
            }
        }
        statements.visit_mut_children_with(self);
    }

    fn visit_mut_var_decl(&mut self, declaration: &mut ast::VarDecl) {
        self.remember(declaration);
        declaration.visit_mut_children_with(self);
    }

    fn visit_mut_expr(&mut self, expr: &mut ast::Expr) {
        expr.visit_mut_children_with(self);
        let span = expr.span();
        let (callee, args) = match expr {
            ast::Expr::New(new) => (&*new.callee, new.args.as_mut()),
            ast::Expr::Call(call) => match &call.callee {
                ast::Callee::Expr(callee) => (&**callee, Some(&mut call.args)),
                _ => return,
            },
            _ => return,
        };
        if !matches!(callee, ast::Expr::Ident(id) if id.sym == "RegExp" && id.ctxt == self.unresolved)
        {
            return;
        }
        let Some(args) = args else { return };
        if args.len() > 2 || args.iter().any(|arg| arg.spread.is_some()) {
            return;
        }
        let mut strings = args.iter().map(|arg| self.string(&arg.expr));
        let Some(pattern) = strings.next().unwrap_or_else(|| Some(String::new())) else {
            return;
        };
        let Some(flags) = strings.next().unwrap_or_else(|| Some(String::new())) else {
            return;
        };
        let regex = Box::new(ast::Expr::Lit(ast::Lit::Regex(ast::Regex {
            span,
            exp: pattern.into(),
            flags: flags.into(),
        })));
        // Reads can throw before a captured const's initialization. Preserve those reads;
        // unused string construction disappears through the existing MIR optimizations.
        let mut exprs: Vec<_> = std::mem::take(args)
            .into_iter()
            .map(|arg| arg.expr)
            .collect();
        exprs.push(regex);
        *expr = ast::Expr::Seq(ast::SeqExpr { span, exprs });
    }
}
