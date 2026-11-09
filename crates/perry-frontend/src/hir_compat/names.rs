//! Names for bindings that rewrites declare in source scopes.
use perry_parser::swc_ecma_ast as ast;
use std::collections::HashSet;
use swc_ecma_visit::{Visit, VisitWith};

/// Prefix of every generated binding name.
const PREFIX: &str = "__perry_";

/// Allocates name prefixes that no source identifier starts with. Perry HIR resolves locals
/// by name, so a generated binding must not shadow or be shadowed by a source binding in any
/// scope.
pub(super) struct FreshNames {
    /// Source identifiers that start with [PREFIX].
    taken: HashSet<String>,
    next: usize,
}

impl FreshNames {
    pub(super) fn new(module: &ast::Module) -> Self {
        struct Names(HashSet<String>);
        impl Visit for Names {
            fn visit_ident(&mut self, ident: &ast::Ident) {
                if ident.sym.starts_with(PREFIX) {
                    self.0.insert(ident.sym.to_string());
                }
            }
        }
        let mut names = Names(HashSet::new());
        module.visit_with(&mut names);
        Self {
            taken: names.0,
            next: 0,
        }
    }

    /// A prefix `__perry_<purpose>_<n>_` that no source identifier starts with and that no
    /// earlier call returned. It is itself an identifier, and so is any extension of it.
    pub(super) fn prefix(&mut self, purpose: &str) -> String {
        loop {
            self.next += 1;
            let prefix = format!("{PREFIX}{purpose}_{}_", self.next);
            if !self.taken.iter().any(|name| name.starts_with(&prefix)) {
                return prefix;
            }
        }
    }
}
