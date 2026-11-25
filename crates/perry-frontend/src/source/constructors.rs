//! Lexically bound builtin constructors and their source operands.
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
    pub(crate) types: &'a [Type],
    pub(crate) init: Option<&'a Expr>,
}

#[derive(Clone, Copy)]
pub(crate) enum BuiltinConstructor {
    Collection(CollectionKind),
    RegExp,
}

#[derive(Default)]
pub(crate) struct Constructors(HashMap<Span, BuiltinConstructor>);

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

pub(crate) trait ConstructorSites {
    fn constructor(&self, expr: &Expr) -> Option<BuiltinConstructor>;

    fn regexp<'e>(&self, expr: &'e Expr) -> Option<RegExpArguments<'e>> {
        match expr {
            Expr::RegExpDynamic { pattern, flags, .. } => Some(RegExpArguments {
                pattern: Some(pattern),
                flags: flags.as_deref(),
                extra: &[],
            }),
            Expr::Call { args, .. }
                if matches!(self.constructor(expr), Some(BuiltinConstructor::RegExp)) =>
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

    fn collection<'e>(&self, expr: &'e Expr) -> Option<Constructor<'e>> {
        let Expr::Call {
            args, type_args, ..
        } = expr
        else {
            return None;
        };
        let BuiltinConstructor::Collection(kind) = self.constructor(expr)? else {
            return None;
        };
        Some(Constructor {
            kind,
            types: type_args,
            init: args
                .first()
                .filter(|arg| !matches!(arg, Expr::Null | Expr::Undefined)),
        })
    }
}

impl ConstructorSites for Constructors {
    fn constructor(&self, expr: &Expr) -> Option<BuiltinConstructor> {
        let Expr::Call {
            byte_offset,
            byte_end,
            ..
        } = expr
        else {
            return None;
        };
        self.0
            .get(&Span::new(BytePos(*byte_offset), BytePos(*byte_end)))
            .copied()
    }
}

impl Constructors {
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
                    self.sites.0.insert(call.span, BuiltinConstructor::RegExp);
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
                    self.sites.0.insert(new.span, BuiltinConstructor::RegExp);
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
                self.sites
                    .0
                    .insert(new.span, BuiltinConstructor::Collection(kind));
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
