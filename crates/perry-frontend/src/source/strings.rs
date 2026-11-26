//! Bounded RegExp constructors use the constant text of their lexical operands.
use super::ConstructorSites;
use perry_hir::{
    ir::{BinaryOp, Expr},
    types::LocalId,
};
use std::{borrow::Cow, collections::HashMap};

enum Constant<'s> {
    Initializer(&'s Expr),
    Text(String),
}

#[derive(Default)]
pub(crate) struct RegexInputs<'s> {
    pub(crate) sites: Vec<&'s Expr>,
    strings: HashMap<LocalId, Constant<'s>>,
    bindings: HashMap<LocalId, &'s Expr>,
}

pub(crate) struct RegexPattern<'s> {
    pub(crate) pattern: Cow<'s, str>,
    pub(crate) flags: Cow<'s, str>,
    pub(crate) captures: (usize, bool),
}

#[derive(Default)]
pub(crate) struct RegexSites<'s> {
    patterns: HashMap<*const Expr, RegexPattern<'s>>,
    bindings: HashMap<LocalId, &'s Expr>,
}

impl RegexSites<'_> {
    pub(crate) fn get(&self, expr: &Expr) -> Option<&RegexPattern<'_>> {
        let expr = match expr {
            Expr::LocalGet(id) => *self.bindings.get(id)?,
            _ => expr,
        };
        self.patterns.get(&std::ptr::from_ref(expr))
    }
}

impl<'s> RegexInputs<'s> {
    pub(crate) fn bind(&mut self, id: LocalId, init: &'s Expr, regex: bool, immutable: bool) {
        if regex {
            self.bindings.insert(id, init);
        } else if immutable {
            self.strings.insert(id, Constant::Initializer(init));
        }
    }

    pub(crate) fn finish(mut self, constructors: &impl ConstructorSites) -> RegexSites<'s> {
        let sites = std::mem::take(&mut self.sites);
        let patterns = sites
            .into_iter()
            .filter_map(|expr| {
                let (pattern, flags) = if let Expr::RegExp { pattern, flags } = expr {
                    (Cow::from(pattern.as_str()), Cow::from(flags.as_str()))
                } else {
                    let args = constructors.regexp(expr)?;
                    (self.operand(args.pattern)?, self.operand(args.flags)?)
                };
                let captures = crate::methods::regex_captures(&pattern);
                Some((
                    std::ptr::from_ref(expr),
                    RegexPattern {
                        pattern,
                        flags,
                        captures,
                    },
                ))
            })
            .collect();
        RegexSites {
            patterns,
            bindings: self.bindings,
        }
    }

    fn operand(&mut self, arg: Option<&Expr>) -> Option<Cow<'s, str>> {
        let mut value = String::new();
        if let Some(arg) = arg
            && !matches!(arg, Expr::Undefined)
        {
            self.append(arg, &mut value)?;
        }
        Some(Cow::Owned(value))
    }

    fn append(&mut self, expr: &Expr, output: &mut String) -> Option<()> {
        match expr {
            Expr::String(text) => output.push_str(text),
            Expr::LocalGet(id) => match *self.strings.get(id)? {
                Constant::Text(ref text) => output.push_str(text),
                Constant::Initializer(init) => {
                    self.strings.remove(id);
                    let mut value = String::new();
                    self.append(init, &mut value)?;
                    output.push_str(&value);
                    self.strings.insert(*id, Constant::Text(value));
                }
            },
            Expr::TemplateStringCoerce(value) => self.append(value, output)?,
            Expr::Sequence(parts) => self.append(parts.last()?, output)?,
            Expr::Binary {
                op: BinaryOp::Add,
                left,
                right,
            } => {
                self.append(left, output)?;
                self.append(right, output)?;
            }
            _ => return None,
        }
        Some(())
    }
}
