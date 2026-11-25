//! Lower array/string iteration and byte-stream async iteration before HIR erases types.
use crate::source::FreshNames;
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Diagnostic, Result};
use perry_wit_diagnostics::{bail_unsupported, ensure_unsupported};
use std::collections::HashSet;
use swc_common::{DUMMY_SP, SyntaxContext};
use swc_ecma_utils::ExprFactory;
use swc_ecma_visit::{VisitMut, VisitMutWith};

pub(crate) fn is_writable_to_web(expr: &perry_hir::ir::Expr) -> bool {
    matches!(expr, perry_hir::ir::Expr::NativeMethodCall {
        module, class_name: Some(class), object: None, method, ..
    } if matches!(module.as_str(), "stream" | "node:stream") && class == "Writable" && method == "toWeb")
}

/// The synthetic method a synchronous `for...of` calls on its source: lowering checks that
/// the receiver is an array or string and yields it unchanged.
pub(super) const ITERATION: &str = "$$perryIteration";

/// Rewrite every `for...of` and `for await...of` into the indexed or reader loop it means.
pub(super) fn lower(module: &mut ast::Module, names: &mut FreshNames) -> Result<()> {
    let mut lower = Loops { names, error: None };
    module.visit_mut_with(&mut lower);
    lower.error.map_or(Ok(()), Err)
}

struct Loops<'n> {
    names: &'n mut FreshNames,
    error: Option<Diagnostic>,
}

impl Loops<'_> {
    fn rewrite(&mut self, iteration: ast::ForOfStmt) -> Result<ast::Stmt> {
        let asynchronous = iteration.is_await;
        let ast::ForHead::VarDecl(declaration) = iteration.left else {
            bail_unsupported!("iteration requires a const or let binding");
        };
        const ONE_BINDING: &str = "iteration requires one const or let binding";
        let kind = declaration.kind;
        ensure_unsupported!(kind != ast::VarDeclKind::Var, "{ONE_BINDING}");
        let Ok([declarator]) = <[ast::VarDeclarator; 1]>::try_from(declaration.decls) else {
            bail_unsupported!("{ONE_BINDING}");
        };
        let binding = declarator.name;
        let prefix = self.names.prefix("loop");
        let ident = |suffix| ast::Ident::new_no_ctxt(format!("{prefix}{suffix}").into(), DUMMY_SP);
        let source = iteration.right;
        let body = *iteration.body;
        let statements = if asynchronous {
            let reader = ident("reader");
            let done = ident("done");
            let step = ident("step");
            let acquired = method(source, "getReader")
                .into_var_decl(ast::VarDeclKind::Const, reader.clone().into());
            let exhausted = false.into_var_decl(ast::VarDeclKind::Let, done.clone().into());
            let read = awaited(method(reader.clone(), "read"))
                .into_var_decl(ast::VarDeclKind::Const, step.clone().into());
            let finish = ast::IfStmt {
                span: DUMMY_SP,
                test: step.clone().make_member("done".into()).into(),
                cons: Box::new(
                    block(vec![
                        true.make_assign_to(ast::AssignOp::Assign, done.clone().into())
                            .into_stmt(),
                        ast::BreakStmt::default().into(),
                    ])
                    .into(),
                ),
                alt: None,
            };
            let item = step
                .make_member("value".into())
                .into_var_decl(kind, binding);
            let read_loop = ast::WhileStmt {
                span: DUMMY_SP,
                test: true.into(),
                body: Box::new(block(vec![read.into(), finish.into(), item.into(), body]).into()),
            };
            let cancel = ast::IfStmt {
                span: DUMMY_SP,
                test: ast::UnaryExpr {
                    span: DUMMY_SP,
                    op: ast::UnaryOp::Bang,
                    arg: done.into(),
                }
                .into(),
                cons: Box::new(awaited(method(reader.clone(), "cancel")).into_stmt()),
                alt: None,
            };
            let cleanup = try_finally(cancel.into(), method(reader, "releaseLock").into_stmt());
            vec![
                acquired.into(),
                exhausted.into(),
                try_finally(read_loop.into(), cleanup),
            ]
        } else {
            let items = ident("items");
            let index = ident("index");
            let source = method(source, ITERATION)
                .into_var_decl(ast::VarDeclKind::Const, items.clone().into());
            let init = 0.0.into_var_decl(ast::VarDeclKind::Let, index.clone().into());
            let test = index.clone().make_bin(
                ast::BinaryOp::Lt,
                items.clone().make_member("length".into()),
            );
            let item = items
                .computed_member(index.clone())
                .into_var_decl(kind, binding);
            let update = ast::UpdateExpr {
                span: DUMMY_SP,
                op: ast::UpdateOp::PlusPlus,
                prefix: false,
                arg: index.into(),
            };
            vec![
                source.into(),
                ast::ForStmt {
                    span: DUMMY_SP,
                    init: Some(ast::VarDeclOrExpr::VarDecl(Box::new(init))),
                    test: Some(test.into()),
                    update: Some(update.into()),
                    body: Box::new(block(vec![item.into(), body]).into()),
                }
                .into(),
            ]
        };
        Ok(ast::BlockStmt {
            span: iteration.span,
            stmts: statements,
            ..Default::default()
        }
        .into())
    }
}

fn block(stmts: Vec<ast::Stmt>) -> ast::BlockStmt {
    ast::BlockStmt {
        stmts,
        ..Default::default()
    }
}

fn method(receiver: impl ExprFactory, name: &str) -> ast::Expr {
    receiver
        .make_member(name.into())
        .as_call(DUMMY_SP, Vec::new())
}

fn awaited(call: ast::Expr) -> ast::AwaitExpr {
    ast::AwaitExpr {
        span: DUMMY_SP,
        arg: call.into(),
    }
}

fn try_finally(body: ast::Stmt, finalizer: ast::Stmt) -> ast::Stmt {
    ast::TryStmt {
        span: DUMMY_SP,
        block: block(vec![body]),
        handler: None,
        finalizer: Some(block(vec![finalizer])),
    }
    .into()
}

impl VisitMut for Loops<'_> {
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

pub(super) fn preserve_options(
    module: &mut ast::Module,
    unresolved: SyntaxContext,
    locations: &super::Locations,
) -> Result<()> {
    struct Options<'a> {
        locations: &'a super::Locations<'a>,
        error: Option<Diagnostic>,
        unresolved: SyntaxContext,
    }
    impl VisitMut for Options<'_> {
        fn visit_mut_new_expr(&mut self, new: &mut ast::NewExpr) {
            new.visit_mut_children_with(self);
            if matches!(&*new.callee, ast::Expr::Ident(name) if name.sym == "TextDecoder" && name.ctxt == self.unresolved)
                && let Err(error) = normalize_decoder_constructor(new, self.locations)
                && self.error.is_none()
            {
                self.error = Some(error);
            }
        }
    }
    let mut options = Options {
        locations,
        error: None,
        unresolved,
    };
    module.visit_mut_with(&mut options);
    options.error.map_or(Ok(()), Err)
}

fn normalize_decoder_constructor(
    new: &mut ast::NewExpr,
    locations: &super::Locations,
) -> Result<()> {
    let Some(args) = new.args.as_mut() else {
        return Ok(());
    };
    ensure_unsupported!(
        args.len() <= 2 && args.iter().all(|a| a.spread.is_none()),
        "TextDecoder takes a literal UTF-8 label and inline options"
    );
    let Some(options) = args.get_mut(1) else {
        return Ok(());
    };
    if matches!(&*options.expr, ast::Expr::Ident(i) if i.sym == "undefined") {
        return Ok(());
    }
    let ast::Expr::Object(options) = &mut *options.expr else {
        bail_unsupported!("TextDecoder options must be an inline object");
    };
    let mut names = HashSet::new();
    for property in &mut options.props {
        let ast::PropOrSpread::Prop(property) = property else {
            bail_unsupported!("TextDecoder option spreads are unsupported");
        };
        if let ast::Prop::Shorthand(id) = &**property {
            **property = ast::Prop::KeyValue(ast::KeyValueProp {
                key: ast::PropName::Ident(ast::IdentName::new(id.sym.clone(), id.span)),
                value: Box::new(ast::Expr::Ident(id.clone())),
            });
        }
        let ast::Prop::KeyValue(field) = &**property else {
            bail_unsupported!("TextDecoder options need ordinary fields");
        };
        let name = match &field.key {
            ast::PropName::Ident(id) => id.sym.as_ref(),
            ast::PropName::Str(text) => text.value.as_str().ok_or_else(|| {
                locations.annotate(
                    Diagnostic::unsupported("TextDecoder option key is not valid UTF-8"),
                    text.span,
                )
            })?,
            _ => bail_unsupported!("TextDecoder option keys must be literal"),
        };
        ensure_unsupported!(
            matches!(name, "fatal" | "ignoreBOM") && names.insert(name.to_owned()),
            "unknown or repeated TextDecoder option {name}"
        );
        ensure_unsupported!(
            matches!(
                &*field.value,
                ast::Expr::Ident(_) | ast::Expr::Lit(ast::Lit::Bool(_))
            ),
            "TextDecoder constructor flags must be booleans or local bindings"
        );
    }
    Ok(())
}
