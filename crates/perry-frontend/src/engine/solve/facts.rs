use super::*;
use std::ops::Index;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Fact {
    Escape(TemplateId),
    Storage(LocId),
}

impl Fact {
    fn key(self) -> (usize, usize) {
        match self {
            Self::Escape(id) => (0, id.index()),
            Self::Storage(id) => (1, id.index()),
        }
    }
}

#[derive(Default)]
pub(super) struct Facts {
    values: IdxVec<LocId, Ty>,
    parents: IdxVec<LocId, LocId>,
    pub(super) dirty: [BitSet; 2],
    pub(super) escaping: BitSet,
    declared: BitSet,
    pub(super) self_nested: BitSet,
}

impl Facts {
    pub(super) fn new(values: IdxVec<LocId, Ty>) -> Self {
        let mut parents = IdxVec::default();
        for id in values.ids() {
            parents.push(id);
        }
        Self {
            values,
            parents,
            ..Self::default()
        }
    }

    pub(super) fn mark(&mut self, fact: Fact) {
        let (kind, id) = fact.key();
        self.dirty[kind].insert(id);
    }
    pub(super) fn len(&self) -> usize {
        self.values.len()
    }

    pub(super) fn alloc(&mut self, initial: Ty) -> LocId {
        let id = self.values.push(initial);
        self.parents.push(id);
        id
    }

    pub(super) fn activation(&mut self, values: Vec<Ty>) -> ActivationSpan {
        let span = self.values.append_activation(values);
        for id in span.locations() {
            self.parents.push(id);
        }
        span
    }

    pub(super) fn root(&mut self, mut loc: LocId) -> LocId {
        while self.parents[loc] != loc {
            let next = self.parents[self.parents[loc]];
            self.parents[loc] = next;
            loc = next;
        }
        loc
    }

    pub(super) fn resolve(&self, mut loc: LocId) -> (LocId, Ty) {
        while self.parents[loc] != loc {
            loc = self.parents[loc];
        }
        (loc, self.values[loc])
    }

    pub(super) fn write(&mut self, types: &mut TypeTable, loc: LocId, value: Ty) {
        let root = self.root(loc);
        if self.join(types, root, value) {
            self.mark(Fact::Storage(root));
        }
    }

    pub(super) fn join(&mut self, types: &mut TypeTable, root: LocId, value: Ty) -> bool {
        if self.declared.contains(root.index()) {
            return false;
        }
        let old = self.values[root];
        let (next, nested) = types.join_bounded(old, value);
        if nested {
            self.self_nested.insert(root.index());
        }
        self.values[root] = next;
        old != next
    }

    pub(super) fn declare(&mut self, loc: LocId, value: Ty) {
        let root = self.root(loc);
        self.set(root, value);
        self.declared.insert(root.index());
    }

    fn set(&mut self, loc: LocId, value: Ty) {
        if self.values[loc] != value {
            self.values[loc] = value;
            self.mark(Fact::Storage(loc));
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
        self.parents[alias] = root;
        // Readers of the retired location must resubscribe even when its type matched.
        self.mark(Fact::Storage(alias));
        if self.self_nested.contains(alias.index()) {
            self.self_nested.insert(root.index());
        }
    }

    pub(super) fn finish(mut self) -> (IdxVec<LocId, Ty>, BitSet, BitSet) {
        for alias in self.parents.ids() {
            let root = self.root(alias);
            self.values[alias] = self.values[root];
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
        &self.values[self.resolve(loc).0]
    }
}

#[derive(Default)]
pub(super) struct Subscriptions {
    pub(super) inputs: IdxVec<InstanceId, Vec<Fact>>,
    readers: [Vec<Vec<InstanceId>>; 2],
}

impl Subscriptions {
    pub(super) fn read(&mut self, reader: InstanceId, fact: Fact) {
        let (kind, id) = fact.key();
        let column = &mut self.readers[kind];
        if column.len() <= id {
            column.resize_with(id + 1, Vec::new);
        }
        let readers = &mut column[id];
        if let Err(index) = readers.binary_search(&reader) {
            readers.insert(index, reader);
            self.inputs[reader].push(fact);
        }
    }

    pub(super) fn forget(&mut self, reader: InstanceId) {
        for fact in self.inputs[reader].drain(..) {
            let (kind, id) = fact.key();
            if let Some(readers) = self.readers[kind].get_mut(id)
                && let Ok(index) = readers.binary_search(&reader)
            {
                readers.remove(index);
            }
        }
    }

    pub(super) fn affected<'a>(
        &'a self,
        dirty: &'a [BitSet; 2],
    ) -> impl Iterator<Item = InstanceId> + 'a {
        dirty
            .iter()
            .zip(&self.readers)
            .flat_map(|(dirty, readers)| {
                dirty
                    .iter()
                    .filter_map(|id| readers.get(id))
                    .flatten()
                    .copied()
            })
    }
}
