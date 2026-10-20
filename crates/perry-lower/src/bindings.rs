use super::*;

pub(super) struct Bindings<T> {
    values: HashMap<LocalId, T>,
    saved: Vec<(LocalId, Option<T>)>,
    recording: bool,
}

pub(super) struct Scope {
    start: usize,
    recording: bool,
}

impl<T> Default for Bindings<T> {
    fn default() -> Self {
        Self {
            values: HashMap::new(),
            saved: Vec::new(),
            recording: false,
        }
    }
}

impl<T: Copy> Bindings<T> {
    pub(super) fn get(&self, id: &LocalId) -> Option<&T> {
        self.values.get(id)
    }

    pub(super) fn insert(&mut self, id: LocalId, value: T) -> Option<T> {
        let previous = self.values.insert(id, value);
        if self.recording {
            self.saved.push((id, previous));
        }
        previous
    }

    pub(super) fn remove(&mut self, id: &LocalId) {
        let previous = self.values.remove(id);
        if self.recording {
            self.saved.push((*id, previous));
        }
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
        for (id, previous) in self.saved.drain(scope.start..).rev() {
            match previous {
                Some(value) => {
                    self.values.insert(id, value);
                }
                None => {
                    self.values.remove(&id);
                }
            }
        }
        self.recording = scope.recording;
    }
}
