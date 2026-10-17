//! Preserve method arguments that upstream specializations discard or reorder.
use super::*;
use perry_parser::swc_ecma_ast as ast;
use swc_ecma_visit::{VisitMut, VisitMutWith};

const PUSH: &str = "$$perryArrayPush";
const CONCAT: &str = "$$perryArrayConcat";
const METHOD_PREFIX: &str = "$$perryMethod_";

/// Keep arguments together before HIR expands a variadic push into separate mutations.
pub(super) fn preserve_arguments(module: &mut ast::Module) {
    struct Arguments;
    impl VisitMut for Arguments {
        fn visit_mut_call_expr(&mut self, call: &mut ast::CallExpr) {
            call.visit_mut_children_with(self);
            if let ast::Callee::Expr(callee) = &mut call.callee
                && let ast::Expr::Member(member) = &mut **callee
                && let ast::MemberProp::Ident(method) = &mut member.prop
            {
                if method.sym == "concat"
                    || (method.sym == "push"
                        && (call.args.len() != 1
                            || call.args.iter().any(|argument| argument.spread.is_some())))
                {
                    method.sym = if method.sym == "concat" { CONCAT } else { PUSH }.into();
                } else if matches!(
                    method.sym.as_ref(),
                    "forEach"
                        | "reduce"
                        | "reduceRight"
                        | "join"
                        | "slice"
                        | "reverse"
                        | "toReversed"
                        | "flat"
                        | "findLast"
                        | "findLastIndex"
                        | "at"
                        | "padStart"
                        | "padEnd"
                        | "trimStart"
                        | "trimLeft"
                        | "trimEnd"
                        | "trimRight"
                        | "replace"
                        | "replaceAll"
                        | "split"
                ) || (method.sym.as_ref() == "toString" && !call.args.is_empty())
                {
                    method.sym = format!("{METHOD_PREFIX}{}", method.sym).into();
                }
            }
        }
    }
    module.visit_mut_with(&mut Arguments);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method<'a> {
    Iteration,
    Push,
    Concat,
    Decode,
    Ordinary(&'a str),
}

pub(crate) fn method(name: &str) -> Method<'_> {
    if let Some(rest) = name.strip_prefix(METHOD_PREFIX) {
        return Method::Ordinary(rest);
    }
    match name {
        streams::ITERATION => Method::Iteration,
        PUSH => Method::Push,
        CONCAT => Method::Concat,
        decoder::DECODE => Method::Decode,
        name => Method::Ordinary(name),
    }
}

impl<'a> Method<'a> {
    pub(crate) fn name(self) -> &'a str {
        match self {
            Self::Iteration => "[iteration]",
            Self::Push => "push",
            Self::Concat => "concat",
            Self::Decode => "decode",
            Self::Ordinary(name) => name,
        }
    }
}
