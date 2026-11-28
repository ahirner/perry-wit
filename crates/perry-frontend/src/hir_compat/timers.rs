//! Node callback timer imports, including source uses that lose their span in HIR.
use super::Locations;
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Diagnostic, Result};
use swc_common::{Span, Spanned};
use swc_ecma_visit::{Visit, VisitWith};

pub(crate) fn is_timer_module(source: &str) -> bool {
    matches!(source, "node:timers" | "timers")
}

pub(super) fn check_imports(module: &ast::Module, locations: &Locations<'_>) -> Result<()> {
    let mut bindings = Vec::new();
    for item in &module.body {
        let ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) = item else {
            continue;
        };
        let source = import.src.value.to_string_lossy();
        if import.type_only || !is_timer_module(&source) {
            continue;
        }
        for spec in &import.specifiers {
            if let ast::ImportSpecifier::Named(named) = spec {
                let imported = named.imported.as_ref().map(ast::ModuleExportName::atom);
                let name: &str = imported.as_deref().unwrap_or(&named.local.sym);
                if named.is_type_only
                    || matches!(
                        name,
                        "setTimeout"
                            | "setInterval"
                            | "setImmediate"
                            | "clearTimeout"
                            | "clearInterval"
                    )
                {
                    continue;
                }
            }
            bindings.push(spec.local().to_id());
        }
    }
    if bindings.is_empty() {
        return Ok(());
    }
    struct Uses<'a> {
        bindings: &'a [ast::Id],
        first: Option<Span>,
    }
    impl Visit for Uses<'_> {
        fn visit_expr(&mut self, expr: &ast::Expr) {
            if self.first.is_some() {
                return;
            }
            let value = match expr {
                ast::Expr::Member(member) => member.obj.as_ref(),
                other => other,
            };
            if let ast::Expr::Ident(id) = value
                && self.bindings.contains(&id.to_id())
            {
                self.first = Some(expr.span());
            } else {
                expr.visit_children_with(self);
            }
        }
        fn visit_prop(&mut self, prop: &ast::Prop) {
            if let ast::Prop::Shorthand(id) = prop
                && self.bindings.contains(&id.to_id())
            {
                self.first.get_or_insert(id.span);
            } else {
                prop.visit_children_with(self);
            }
        }
    }
    let mut uses = Uses {
        bindings: &bindings,
        first: None,
    };
    module.visit_with(&mut uses);
    if let Some(span) = uses.first {
        let name = locations
            .source_text(span)
            .ok_or_else(|| Diagnostic::internal("timer import use has no source text"))?;
        return Err(locations.annotate(
            Diagnostic::unsupported(format!("timer import '{name}' is not supported")),
            span,
        ));
    }
    Ok(())
}
