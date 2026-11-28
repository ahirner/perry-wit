//! Lower array/string iteration and byte-stream async iteration before HIR erases types.
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Diagnostic, Result};
use perry_wit_diagnostics::{bail_unsupported, ensure_unsupported};
use std::collections::HashSet;
use swc_common::{DUMMY_SP, SyntaxContext, util::take::Take};
use swc_ecma_utils::ExprFactory;
use swc_ecma_visit::{VisitMut, VisitMutWith};

pub(crate) fn is_writable_to_web(expr: &perry_hir::ir::Expr) -> bool {
    matches!(expr, perry_hir::ir::Expr::NativeMethodCall {
        module, class_name: Some(class), object: None, method, ..
    } if matches!(module.as_str(), "stream" | "node:stream") && class == "Writable" && method == "toWeb")
}

impl super::Prepare<'_> {
    pub(super) fn lower_for_of(&mut self, mut iteration: ast::ForOfStmt) -> Result<ast::Stmt> {
        let ast::ForHead::VarDecl(declaration) = &mut iteration.left else {
            bail_unsupported!("iteration requires a const or let binding");
        };
        const ONE_BINDING: &str = "iteration requires one const or let binding";
        let kind = declaration.kind;
        ensure_unsupported!(kind != ast::VarDeclKind::Var, "{ONE_BINDING}");
        let [declarator] = declaration.decls.as_mut_slice() else {
            bail_unsupported!("{ONE_BINDING}");
        };
        if !iteration.is_await && self.analysis.array_source(&iteration.right) {
            return Ok(iteration.into());
        }
        let binding = declarator.name.take();
        let prefix = self.analysis.names.prefix("loop");
        let ident = |suffix| ast::Ident::new_no_ctxt(format!("{prefix}{suffix}").into(), DUMMY_SP);
        let source = iteration.right;
        let body = *iteration.body;
        let statements = if iteration.is_await {
            let reader = ident("reader");
            let done = ident("done");
            let step = ident("step");
            let acquired = method(source, "getReader")
                .into_var_decl(ast::VarDeclKind::Const, reader.clone().into());
            let exhausted = false.into_var_decl(ast::VarDeclKind::Let, done.clone().into());
            let read = awaited(method(reader.clone(), "read"))
                .into_var_decl(ast::VarDeclKind::Const, step.clone().into());
            let finish = when(
                step.clone().make_member("done".into()),
                block(vec![
                    true.make_assign_to(ast::AssignOp::Assign, done.clone().into())
                        .into_stmt(),
                    ast::BreakStmt::default().into(),
                ]),
            );
            let item = step
                .make_member("value".into())
                .into_var_decl(kind, binding);
            let read_loop = ast::WhileStmt {
                span: DUMMY_SP,
                test: true.into(),
                body: Box::new(block(vec![read.into(), finish, item.into(), body]).into()),
            };
            let cancel = when(
                done.make_eq(false),
                awaited(method(reader.clone(), "cancel")).into_stmt(),
            );
            let cleanup = try_finally(cancel, method(reader, "releaseLock").into_stmt());
            vec![
                acquired.into(),
                exhausted.into(),
                try_finally(read_loop.into(), cleanup),
            ]
        } else {
            // Perry resolves the lexical scope and destructuring pattern. MIR supplies
            // the current element and owns the loop bounds, cursor, and control flow.
            let element =
                ast::Ident::new_no_ctxt(format!("{prefix}element").into(), iteration.span);
            self.analysis.record_iteration(iteration.span);
            let init = source.into_var_decl(ast::VarDeclKind::Const, element.clone().into());
            let item = element.into_var_decl(kind, binding);
            return Ok(ast::ForStmt {
                span: iteration.span,
                init: Some(ast::VarDeclOrExpr::VarDecl(Box::new(init))),
                test: None,
                update: None,
                body: Box::new(block(vec![item.into(), body]).into()),
            }
            .into());
        };
        Ok(ast::BlockStmt {
            span: iteration.span,
            stmts: statements,
            ..Default::default()
        }
        .into())
    }
}

fn when(test: impl ExprFactory, body: impl Into<ast::Stmt>) -> ast::Stmt {
    ast::IfStmt {
        span: DUMMY_SP,
        test: test.into(),
        cons: Box::new(body.into()),
        alt: None,
    }
    .into()
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
