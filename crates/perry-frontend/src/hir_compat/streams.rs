//! Lower array/string iteration and byte-stream async iteration before HIR erases types.
use super::names::FreshNames;
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Diagnostic, Result};
use perry_wit_diagnostics::{
    bail_internal, bail_unsupported, ensure_internal, ensure_unsupported, internal,
};
use std::collections::HashSet;
use swc_common::{DUMMY_SP, Span, SyntaxContext};
use swc_ecma_visit::{VisitMut, VisitMutWith};

pub fn is_writable_to_web(expr: &perry_hir::ir::Expr) -> bool {
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
            .map_err(|e| internal!("{e:?}"))?;
        struct SyntheticSpans;
        impl VisitMut for SyntheticSpans {
            fn visit_mut_span(&mut self, span: &mut Span) {
                *span = DUMMY_SP;
            }
        }
        parsed.visit_mut_with(&mut SyntheticSpans);
        let Some(ast::ModuleItem::Stmt(mut statement)) = parsed.body.pop() else {
            bail_internal!("the iteration template is one block statement");
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

/// Fills a parsed loop template's placeholders (`<prefix>source`, `<prefix>item`, and
/// `<prefix>body()`) with the loop's parts in a single pass.
/// [Fill::check] verifies that every placeholder was substituted exactly once.
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
        ensure_internal!(
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

pub(super) const DECODE: &str = "$$perryDecodeOptions";

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
        fn visit_mut_call_expr(&mut self, call: &mut ast::CallExpr) {
            call.visit_mut_children_with(self);
            if call.args.len() > 1
                && let ast::Callee::Expr(callee) = &mut call.callee
                && let ast::Expr::Member(member) = &mut **callee
                && let ast::MemberProp::Ident(method) = &mut member.prop
                && method.sym == "decode"
            {
                method.sym = DECODE.into();
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
