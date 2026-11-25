//! Collection construction sites retain the type application and argument expressions.
use crate::hir_compat::Locations;
use perry_hir::{ir::Expr, types::Type};
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag, unsupported};
use std::collections::HashMap;
use swc_common::{BytePos, Span, SyntaxContext, util::take::Take};
use swc_ecma_visit::{VisitMut, VisitMutWith};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectionKind {
    Map,
    Set,
}

pub(crate) struct Constructor<'a> {
    pub(crate) kind: CollectionKind,
    pub(crate) span: Span,
    pub(crate) types: &'a [Type],
    pub(crate) init: Option<&'a Expr>,
}

#[derive(Default)]
pub(crate) struct Constructors(HashMap<Span, CollectionKind>);

impl Constructors {
    pub(crate) fn get<'e>(&self, expr: &'e Expr) -> Option<Constructor<'e>> {
        let Expr::Call {
            args,
            type_args,
            byte_offset,
            byte_end,
            ..
        } = expr
        else {
            return None;
        };
        let span = Span::new(BytePos(*byte_offset), BytePos(*byte_end));
        Some(Constructor {
            kind: *self.0.get(&span)?,
            span,
            types: type_args,
            init: args
                .first()
                .filter(|arg| !matches!(arg, Expr::Null | Expr::Undefined)),
        })
    }

    pub(crate) fn bind(
        module: &mut ast::Module,
        unresolved: SyntaxContext,
        locations: &Locations<'_>,
    ) -> Result<Self> {
        struct Binder {
            unresolved: SyntaxContext,
            sites: Constructors,
            diagnostic: Option<(Span, Diagnostic)>,
        }
        impl VisitMut for Binder {
            fn visit_mut_expr(&mut self, expr: &mut ast::Expr) {
                expr.visit_mut_children_with(self);
                let ast::Expr::New(new) = expr else { return };
                let ast::Expr::Ident(name) = &*new.callee else {
                    return;
                };
                if name.ctxt != self.unresolved {
                    return;
                }
                let kind = match name.sym.as_ref() {
                    "Map" => CollectionKind::Map,
                    "Set" => CollectionKind::Set,
                    _ => return,
                };
                let args = new.args.as_deref().unwrap_or_default();
                let problem = if args.len() > 1 || args.iter().any(|arg| arg.spread.is_some()) {
                    Some(unsupported!(
                        "{} takes at most one iterable argument",
                        name.sym
                    ))
                } else {
                    match (kind, new.type_args.as_ref().map(|args| args.params.len())) {
                        (CollectionKind::Map, Some(n)) if n != 2 => Some(diag!(
                            codes::TYPE_MISMATCH,
                            "new Map takes key and value type arguments"
                        )),
                        (CollectionKind::Set, Some(n)) if n != 1 => Some(diag!(
                            codes::TYPE_MISMATCH,
                            "new Set takes one element type argument"
                        )),
                        _ => None,
                    }
                };
                if let Some(problem) = problem {
                    self.diagnostic.get_or_insert((new.span, problem));
                    return;
                }
                self.sites.0.insert(new.span, kind);
                // Perry retains generic arguments and both endpoints on calls. The source
                // site supplies constructor identity even when a lexical class shadows a global.
                *expr = ast::Expr::Call(ast::CallExpr {
                    span: new.span,
                    ctxt: new.ctxt,
                    callee: ast::Callee::Expr(new.callee.take()),
                    type_args: new.type_args.take(),
                    args: new.args.take().unwrap_or_default(),
                });
            }
        }
        let mut binder = Binder {
            unresolved,
            sites: Self::default(),
            diagnostic: None,
        };
        module.visit_mut_with(&mut binder);
        match binder.diagnostic {
            Some((span, err)) => Err(locations.annotate(err, span)),
            None => Ok(binder.sites),
        }
    }
}
