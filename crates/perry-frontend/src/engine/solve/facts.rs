use super::*;
use std::{cell::Cell, collections::BTreeSet, ops::Index};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Fact {
    Escape(TemplateId),
    Storage(LocId),
}

#[derive(Default)]
pub(super) struct Facts {
    values: IdxVec<LocId, Ty>,
    parents: HashMap<LocId, Cell<LocId>>,
    pub(super) dirty: BTreeSet<Fact>,
    pub(super) escaping: BitSet,
    declared: BitSet,
    pub(super) self_nested: BitSet,
}

impl Facts {
    pub(super) fn new(values: IdxVec<LocId, Ty>) -> Self {
        Self {
            values,
            ..Self::default()
        }
    }

    pub(super) fn len(&self) -> usize {
        self.values.len()
    }

    pub(super) fn alloc(&mut self, initial: Ty) -> LocId {
        self.values.push(initial)
    }

    pub(super) fn root(&self, mut loc: LocId) -> LocId {
        while let Some(parent) = self.parents.get(&loc) {
            loc = parent.get();
            if let Some(grandparent) = self.parents.get(&loc) {
                loc = grandparent.get();
                parent.set(loc);
            }
        }
        loc
    }

    pub(super) fn resolve(&self, loc: LocId) -> (LocId, Ty) {
        let root = self.root(loc);
        (root, self.values[root])
    }

    pub(super) fn write(&mut self, types: &mut TypeTable, loc: LocId, value: Ty) {
        let (root, old) = self.resolve(loc);
        if self.declared.contains(root.index()) {
            return;
        }
        let (next, nested) = types.join_bounded(old, value);
        if nested {
            self.self_nested.insert(root.index());
        }
        self.set(root, next);
    }

    pub(super) fn declare(&mut self, loc: LocId, value: Ty) {
        let root = self.root(loc);
        self.set(root, value);
        self.declared.insert(root.index());
    }

    fn set(&mut self, loc: LocId, value: Ty) {
        if self.values[loc] != value {
            self.values[loc] = value;
            self.dirty.insert(Fact::Storage(loc));
        }
    }

    pub(super) fn share_elements(&mut self, types: &mut TypeTable, left: LocId, right: LocId) {
        let left = self.root(left);
        let right = self.root(right);
        if left == right {
            return;
        }
        let root = left.min(right);
        let alias = left.max(right);
        self.write(types, root, self.values[alias]);
        self.parents.insert(alias, Cell::new(root));
        // Readers of the retired location must resubscribe even when its type matched.
        self.dirty.insert(Fact::Storage(alias));
        if self.self_nested.contains(alias.index()) {
            self.self_nested.insert(root.index());
        }
    }

    pub(super) fn finish(mut self) -> (IdxVec<LocId, Ty>, BitSet, BitSet) {
        for &alias in self.parents.keys() {
            let (root, ty) = self.resolve(alias);
            self.values[alias] = ty;
            if self.self_nested.contains(root.index()) {
                self.self_nested.insert(alias.index());
            }
        }
        (self.values, self.self_nested, self.escaping)
    }
}

impl Index<LocId> for Facts {
    type Output = Ty;

    fn index(&self, loc: LocId) -> &Ty {
        &self.values[self.root(loc)]
    }
}

#[derive(Default)]
pub(super) struct Subscriptions {
    pub(super) inputs: IdxVec<InstanceId, Vec<Fact>>,
    readers: HashMap<Fact, Vec<InstanceId>>,
}

impl Subscriptions {
    pub(super) fn read(&mut self, reader: InstanceId, fact: Fact) {
        let readers = self.readers.entry(fact).or_default();
        if let Err(index) = readers.binary_search(&reader) {
            readers.insert(index, reader);
            self.inputs[reader].push(fact);
        }
    }

    pub(super) fn forget(&mut self, reader: InstanceId) {
        for fact in self.inputs[reader].drain(..) {
            if let Some(readers) = self.readers.get_mut(&fact)
                && let Ok(index) = readers.binary_search(&reader)
            {
                readers.remove(index);
            }
        }
    }

    pub(super) fn readers(&self, fact: Fact) -> &[InstanceId] {
        self.readers.get(&fact).map_or(&[], Vec::as_slice)
    }
}
