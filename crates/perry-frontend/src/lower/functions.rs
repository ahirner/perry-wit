//! Emitted functions in module order with one lookup for HIR function references.
use super::*;
use crate::engine::types::{EmittedId, IdxVec};

pub(super) enum Callee<'s> {
    Source(&'s Function<'s>, Signature<'s>),
    Native(&'s crate::engine::NativeFunction, Signature<'s>),
}

#[derive(Default)]
pub(super) struct Functions<'s> {
    ids: HashMap<FuncId, EmittedId>,
    rows: IdxVec<EmittedId, Callee<'s>>,
}

impl<'s> Functions<'s> {
    pub(super) fn insert(&mut self, id: FuncId, callee: Callee<'s>) {
        self.ids.entry(id).or_insert_with(|| self.rows.push(callee));
    }

    pub(super) fn get(&self, id: &FuncId) -> Option<&Signature<'s>> {
        match self.callee(*id)? {
            Callee::Source(_, signature) | Callee::Native(_, signature) => Some(signature),
        }
    }

    pub(super) fn callee(&self, id: FuncId) -> Option<&Callee<'s>> {
        Some(&self.rows[*self.ids.get(&id)?])
    }

    pub(super) fn source(&self, id: FuncId) -> Option<&'s Function<'s>> {
        match self.callee(id)? {
            Callee::Source(function, _) => Some(function),
            Callee::Native(..) => None,
        }
    }
}
