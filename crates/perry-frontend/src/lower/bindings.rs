use super::*;
use crate::engine::types::{IdxVec, LocalIdx};

/// One binding slot per local of the module's local table.
pub(super) struct Bindings<'s, T> {
    locals: &'s hir_compat::Locals<'s>,
    values: IdxVec<LocalIdx, Option<T>>,
    saved: Vec<(LocalIdx, Option<T>)>,
    recording: bool,
}

pub(super) struct Scope {
    start: usize,
    recording: bool,
}

impl<'s, T: Copy> Bindings<'s, T> {
    pub(super) fn new(locals: &'s hir_compat::Locals<'s>) -> Self {
        Self {
            locals,
            values: locals.rows.filled(None),
            saved: Vec::new(),
            recording: false,
        }
    }

    pub(super) fn get(&self, id: LocalId) -> Option<T> {
        self.values[self.locals.index(id)?]
    }

    pub(super) fn insert(&mut self, id: LocalId, value: T) -> Result<()> {
        self.replace(id, Some(value))?;
        Ok(())
    }

    /// Bind `id` to `value`, or unbind it, and return its previous binding.
    pub(super) fn replace(&mut self, id: LocalId, value: Option<T>) -> Result<Option<T>> {
        let local = self
            .locals
            .index(id)
            .ok_or_else(|| internal!("binding of unindexed local {id}"))?;
        let previous = std::mem::replace(&mut self.values[local], value);
        if self.recording {
            self.saved.push((local, previous));
        }
        Ok(previous)
    }

    pub(super) fn scope(&mut self) -> Scope {
        let scope = Scope {
            start: self.saved.len(),
            recording: self.recording,
        };
        self.recording = true;
        scope
    }

    pub(super) fn restore(&mut self, scope: Scope) {
        for (local, previous) in self.saved.drain(scope.start..).rev() {
            self.values[local] = previous;
        }
        self.recording = scope.recording;
    }
}
