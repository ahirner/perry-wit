//! Boundary scopes are decided before lowering inserts stateful runtime calls.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Direct,
    Task,
}

impl Scope {
    pub(crate) fn for_function(state: &ModuleState, name: &Identifier) -> Self {
        if state.initializer.is_some()
            || state.memory_checks
            || state.count_roots
            || state
                .program_facts
                .effects(name)
                .is_none_or(|effects| effects.effects.requires_task())
        {
            Self::Task
        } else {
            Self::Direct
        }
    }

    pub(super) fn for_export(state: &ModuleState, export: &Export, function: &Identifier) -> Self {
        let marshalling = export
            .params
            .iter()
            .map(|field| &field.ty)
            .chain(&export.result)
            .any(|ty| !ty.is_scalar());
        if marshalling || export.indirect_size.is_some() || export.is_async() {
            Self::Task
        } else {
            Self::for_function(state, function)
        }
    }
}
