//! Materialize storage required by shared MIR aggregate slots.
use super::*;
use crate::{FuncId, LocalId};
use pliron::value::Value;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Binding {
    Local(LocalId),
    Result(FuncId),
}

pub(crate) enum Storage {
    Source {
        has_views: std::cell::Cell<bool>,
        bindings: RefCell<Vec<(Binding, Value)>>,
    },
    Materialized(HashMap<Binding, TypeHandle>),
}

impl Default for Storage {
    fn default() -> Self {
        Self::Source {
            has_views: Default::default(),
            bindings: Default::default(),
        }
    }
}

impl Storage {
    pub(crate) fn record(&self, id: LocalId, slot: Value) {
        if let Self::Source { bindings, .. } = self {
            bindings.borrow_mut().push((Binding::Local(id), slot));
        }
    }
}
impl Shapes<'_> {
    pub(crate) fn storage(&self, binding: Binding, ty: TypeHandle) -> TypeHandle {
        match &self.byte_slots {
            Storage::Materialized(layouts) => layouts.get(&binding).copied().unwrap_or(ty),
            Storage::Source { .. } => ty,
        }
    }
}
