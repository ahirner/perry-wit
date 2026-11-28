//! Names for bindings that rewrites declare in source scopes.
use std::collections::HashSet;

/// Prefix of every generated binding name.
const PREFIX: &str = "__perry_";

pub(crate) const MODULE_PREFIX: &str = "__perry_module_";

pub(crate) fn module_binding_name(index: usize, name: &str) -> String {
    format!("{MODULE_PREFIX}{index}_{name}")
}

pub(crate) fn is_reserved_binding(name: &str) -> bool {
    name.starts_with(PREFIX)
}

/// Allocates name prefixes that no source identifier starts with. Perry HIR resolves locals
/// by name, so a generated binding must not shadow or be shadowed by a source binding in any
/// scope.
#[derive(Default)]
pub(crate) struct FreshNames {
    /// Source identifiers that start with [PREFIX].
    taken: HashSet<String>,
    next: usize,
}

impl FreshNames {
    pub(super) fn reserve(&mut self, name: &str) {
        if is_reserved_binding(name) {
            self.taken.insert(name.to_owned());
        }
    }

    /// A prefix `__perry_<purpose>_<n>_` that no source identifier starts with and that no
    /// earlier call returned. It is itself an identifier, and so is any extension of it.
    pub(crate) fn prefix(&mut self, purpose: &str) -> String {
        loop {
            self.next += 1;
            let prefix = format!("{PREFIX}{purpose}_{}_", self.next);
            if !self.taken.iter().any(|name| name.starts_with(&prefix)) {
                return prefix;
            }
        }
    }
}
