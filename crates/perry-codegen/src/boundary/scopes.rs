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
            || state.root_metrics.is_some()
            || state
                .call_effects
                .get(name)
                .is_none_or(|effects| effects.requires_task())
        {
            Self::Task
        } else {
            Self::Direct
        }
    }

    pub(super) fn for_export(state: &ModuleState, export: &Export, function: &Identifier) -> Self {
        let marshalling = export.params.iter().chain(export.result.iter()).any(|ty| {
            !matches!(
                ty,
                AbiType::Bool
                    | AbiType::S8
                    | AbiType::U8
                    | AbiType::S16
                    | AbiType::U16
                    | AbiType::S32
                    | AbiType::U32
                    | AbiType::S64
                    | AbiType::U64
                    | AbiType::F32
                    | AbiType::F64
            )
        });
        if marshalling || export.params.len() > MAX_FLAT_PARAMS || export.task_return.is_some() {
            Self::Task
        } else {
            Self::for_function(state, function)
        }
    }
}
