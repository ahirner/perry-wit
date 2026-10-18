use perry_hir::{
    analysis::{collect_local_refs_expr, collect_local_refs_stmt},
    ir::{Module, Stmt},
    types::LocalId,
};
use std::collections::HashSet;

/// Only bindings used outside module initialization need persistent storage.
pub(super) fn captured(module: &Module) -> HashSet<LocalId> {
    let methods = module.classes.iter().flat_map(|class| {
        class
            .constructor
            .iter()
            .chain(&class.methods)
            .chain(&class.static_methods)
            .chain(class.getters.iter().map(|(_, method)| method))
            .chain(class.setters.iter().map(|(_, method)| method))
    });
    let mut references = Vec::new();
    let mut visited = HashSet::new();
    for function in module.functions.iter().chain(methods) {
        for statement in &function.body {
            collect_local_refs_stmt(statement, &mut references, &mut visited);
        }
        for param in &function.params {
            if let Some(default) = &param.default {
                collect_local_refs_expr(default, &mut references, &mut visited);
            }
        }
    }
    for class in &module.classes {
        for field in class.fields.iter().chain(&class.static_fields) {
            if let Some(init) = &field.init {
                collect_local_refs_expr(init, &mut references, &mut visited);
            }
        }
    }
    let captures: HashSet<_> = references.into_iter().collect();
    module
        .init
        .iter()
        .filter_map(|statement| match statement {
            Stmt::Let { id, .. } if captures.contains(id) => Some(*id),
            _ => None,
        })
        .collect()
}
