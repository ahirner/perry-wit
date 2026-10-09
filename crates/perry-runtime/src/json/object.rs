//! Ordered property tables with native eight-byte slots and task-owned hash storage.
use super::*;

#[derive(Clone, Copy)]
pub(super) struct Field {
    pub key: *mut u8,
    pub value: u64,
    next: usize,
}

pub(super) struct Object {
    fields: *mut Field,
    buckets: *mut usize,
    capacity: usize,
    used: usize,
    length: usize,
}

fn storage<T>(count: usize) -> *mut T {
    let bytes = count
        .checked_mul(size_of::<T>())
        .and_then(|size| u32::try_from(size).ok())
        .expect("JSON object too large");
    perry_alloc(bytes).cast()
}

fn hash(key: &str) -> usize {
    key.bytes().fold(2_166_136_261_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    }) as usize
}

impl Object {
    pub fn new(capacity: usize) -> *mut Self {
        let capacity = capacity
            .max(4)
            .checked_next_power_of_two()
            .expect("JSON object too large");
        let object = storage::<Self>(1);
        let fields = storage::<Field>(capacity);
        let buckets = storage::<usize>(capacity);
        // SAFETY: fresh storage has sufficient size and alignment; zero marks an empty bucket.
        unsafe {
            buckets.write_bytes(0, capacity);
            object.write(Self {
                fields,
                buckets,
                capacity,
                used: 0,
                length: 0,
            });
        }
        object
    }

    /// The object and its keys remain live and unchanged while the iterator is in use.
    pub unsafe fn fields(&self) -> &[Field] {
        // SAFETY: only the initialized span is exposed; null keys mark deleted entries.
        unsafe { core::slice::from_raw_parts(self.fields, self.used) }
    }

    /// The object and all keys are live task storage.
    pub unsafe fn find(&self, key: &str) -> Option<*mut Field> {
        // SAFETY: bucket links are one-based indices into initialized, non-deleted fields.
        unsafe {
            let mut index = *self.buckets.add(hash(key) & (self.capacity - 1));
            while index != 0 {
                let field = self.fields.add(index - 1);
                if string::text((*field).key) == key {
                    return Some(field);
                }
                index = (*field).next;
            }
            None
        }
    }

    /// Both key and optional value remain live for this object's task.
    pub unsafe fn set(&mut self, key: *mut u8, value: u64) {
        // SAFETY: existing entries keep their position; new entries append after any compaction.
        unsafe {
            if let Some(field) = self.find(string::text(key)) {
                (*field).value = value;
                return;
            }
            if self.used == self.capacity {
                self.reserve();
            }
            let bucket = self
                .buckets
                .add(hash(string::text(key)) & (self.capacity - 1));
            self.fields.add(self.used).write(Field {
                key,
                value,
                next: *bucket,
            });
            self.used += 1;
            self.length += 1;
            *bucket = self.used;
        }
    }

    /// The object and all keys are live task storage; no field iterator may be in use.
    pub unsafe fn delete(&mut self, key: &str) {
        // SAFETY: unlinking a live entry makes its tombstone unreachable from every bucket.
        unsafe {
            let mut link = self.buckets.add(hash(key) & (self.capacity - 1));
            while *link != 0 {
                let field = self.fields.add(*link - 1);
                if string::text((*field).key) == key {
                    *link = (*field).next;
                    (*field).key = ptr::null_mut();
                    (*field).value = 0;
                    self.length -= 1;
                    return;
                }
                link = &raw mut (*field).next;
            }
        }
    }

    unsafe fn reserve(&mut self) {
        // SAFETY: retained entries compact in insertion order, in place or into a larger span.
        unsafe {
            let old = self.fields;
            if self.length > self.capacity / 2 {
                self.capacity = self.capacity.checked_mul(2).expect("JSON object too large");
                self.fields = storage(self.capacity);
                self.buckets = storage(self.capacity);
            }
            self.buckets.write_bytes(0, self.capacity);
            let mut used = 0;
            for index in 0..self.used {
                let mut field = old.add(index).read();
                if field.key.is_null() {
                    continue;
                }
                let bucket = self
                    .buckets
                    .add(hash(string::text(field.key)) & (self.capacity - 1));
                field.next = *bucket;
                self.fields.add(used).write(field);
                used += 1;
                *bucket = used;
            }
            self.used = used;
        }
    }
}
