//! Lexically bound builtin constructors and their source operands.
use crate::hir_compat::Locations;
use perry_hir::{ir::Expr, types::Type};
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag, unsupported};
use std::collections::{HashMap, HashSet};
use swc_common::{BytePos, Span, SyntaxContext, util::take::Take};
use swc_ecma_visit::{VisitMut, VisitMutWith};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectionKind {
    Map,
    Set,
}

pub(crate) struct Constructor<'a> {
    pub(crate) kind: CollectionKind,
    pub(crate) types: &'a [Type],
    pub(crate) init: Option<&'a Expr>,
}

#[derive(Default)]
pub(crate) struct Constructors {
    collections: HashMap<Span, CollectionKind>,
    regexp_calls: HashSet<Span>,
}

#[derive(Clone, Copy)]
pub(crate) struct RegExpArguments<'a> {
    pub(crate) pattern: Option<&'a Expr>,
    pub(crate) flags: Option<&'a Expr>,
    pub(crate) extra: &'a [Expr],
}

impl<'a> RegExpArguments<'a> {
    pub(crate) fn operands(self) -> impl Iterator<Item = &'a Expr> {
        self.pattern.into_iter().chain(self.flags).chain(self.extra)
    }
}

impl Constructors {
    pub(crate) fn regexp<'e>(&self, expr: &'e Expr) -> Option<RegExpArguments<'e>> {
        match expr {
            Expr::RegExpDynamic { pattern, flags, .. } => Some(RegExpArguments {
                pattern: Some(pattern),
                flags: flags.as_deref(),
                extra: &[],
            }),
            Expr::Call {
                args,
                byte_offset,
                byte_end,
                ..
            } if self
                .regexp_calls
                .contains(&Span::new(BytePos(*byte_offset), BytePos(*byte_end))) =>
            {
                Some(RegExpArguments {
                    pattern: args.first(),
                    flags: args.get(1),
                    extra: args.get(2..).unwrap_or_default(),
                })
            }
            _ => None,
        }
    }

    pub(crate) fn collection<'e>(&self, expr: &'e Expr) -> Option<Constructor<'e>> {
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
            kind: *self.collections.get(&span)?,
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
                if let ast::Expr::Call(call) = expr
                    && call.args.len() > 2
                    && call.args.iter().all(|arg| arg.spread.is_none())
                    && let ast::Callee::Expr(callee) = &call.callee
                    && matches!(callee.unwrap_parens(), ast::Expr::Ident(name) if name.ctxt == self.unresolved && name.sym == "RegExp")
                {
                    self.sites.regexp_calls.insert(call.span);
                }
                let ast::Expr::New(new) = expr else { return };
                let ast::Expr::Ident(name) = new.callee.unwrap_parens() else {
                    return;
                };
                if name.ctxt != self.unresolved {
                    return;
                }
                let args = new.args.as_deref().unwrap_or_default();
                if name.sym == "RegExp"
                    && (args.is_empty() || args.len() > 2)
                    && args.iter().all(|arg| arg.spread.is_none())
                {
                    // Perry keeps all arguments on ordinary calls. Its new-RegExp form
                    // drops operands after the flags and loses the zero-argument builtin.
                    self.sites.regexp_calls.insert(new.span);
                    *expr = constructor_call(new);
                    return;
                }
                let kind = match name.sym.as_ref() {
                    "Map" => CollectionKind::Map,
                    "Set" => CollectionKind::Set,
                    _ => return,
                };
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
                self.sites.collections.insert(new.span, kind);
                // Perry retains generic arguments and both endpoints on calls. The source
                // site supplies constructor identity even when a lexical class shadows a global.
                *expr = constructor_call(new);
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

fn constructor_call(new: &mut ast::NewExpr) -> ast::Expr {
    ast::Expr::Call(ast::CallExpr {
        span: new.span,
        ctxt: new.ctxt,
        callee: ast::Callee::Expr(new.callee.take()),
        type_args: new.type_args.take(),
        args: new.args.take().unwrap_or_default(),
    })
}
