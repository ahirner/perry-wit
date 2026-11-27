//! Lexically bound builtin constructors and their source operands.
use crate::hir_compat::Locations;
use perry_hir::{ir::Expr, types::Type};
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag, unsupported};
use swc_common::{DUMMY_SP, Span, SyntaxContext, util::take::Take};
use swc_ecma_visit::{VisitMut, VisitMutWith};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectionKind {
    Map,
    Set,
}

impl CollectionKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Map => "Map",
            Self::Set => "Set",
        }
    }
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

pub(crate) struct Constructors;

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
            types: type_args.split_last()?.1,
            init: args
                .first()
                .filter(|arg| !matches!(arg, Expr::Null | Expr::Undefined)),
        })
    }
}

impl ConstructorSites for Constructors {
    fn constructor(&self, expr: &Expr) -> Option<BuiltinConstructor> {
        let Expr::Call { type_args, .. } = expr else {
            return None;
        };
        let Type::Named(marker) = type_args.last()? else {
            return None;
        };
        match marker.as_str() {
            "perry:new Map" => Some(BuiltinConstructor::Collection(CollectionKind::Map)),
            "perry:new Set" => Some(BuiltinConstructor::Collection(CollectionKind::Set)),
            "perry:RegExp" => Some(BuiltinConstructor::RegExp),
            _ => None,
        }
    }
}

impl Constructors {
    pub(crate) fn bind(
        module: &mut ast::Module,
        unresolved: SyntaxContext,
        locations: &Locations<'_>,
    ) -> Result<()> {
        struct Binder {
            unresolved: SyntaxContext,
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
                    mark(call, "perry:RegExp");
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
                    *expr = constructor_call(new, "perry:RegExp");
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
                *expr = constructor_call(
                    new,
                    match kind {
                        CollectionKind::Map => "perry:new Map",
                        CollectionKind::Set => "perry:new Set",
                    },
                );
            }
        }
        let mut binder = Binder {
            unresolved,
            diagnostic: None,
        };
        module.visit_mut_with(&mut binder);
        match binder.diagnostic {
            Some((span, err)) => Err(locations.annotate(err, span)),
            None => Ok(()),
        }
    }
}

/// A source identifier cannot contain a colon or space. Perry retains named type
/// arguments on a Call, including its original arguments and diagnostic span.
fn mark(call: &mut ast::CallExpr, marker: &str) {
    let types = call.type_args.get_or_insert_with(|| {
        Box::new(ast::TsTypeParamInstantiation {
            span: DUMMY_SP,
            params: Vec::new(),
        })
    });
    types
        .params
        .push(Box::new(ast::TsType::TsTypeRef(ast::TsTypeRef {
            span: DUMMY_SP,
            type_name: ast::TsEntityName::Ident(ast::Ident::new_no_ctxt(marker.into(), DUMMY_SP)),
            type_params: None,
        })));
}

fn constructor_call(new: &mut ast::NewExpr, marker: &str) -> ast::Expr {
    let mut call = ast::CallExpr {
        span: new.span,
        ctxt: new.ctxt,
        callee: ast::Callee::Expr(new.callee.take()),
        type_args: new.type_args.take(),
        args: new.args.take().unwrap_or_default(),
    };
    mark(&mut call, marker);
    ast::Expr::Call(call)
}
