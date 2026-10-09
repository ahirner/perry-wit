//! Pre-order serialization paused between guest replacer calls, with task-owned snapshots.
use super::*;

const MAX_DEPTH: usize = 128;

enum Phase {
    Callback { key: *mut u8, value: *mut u8 },
    Traverse,
    Done,
}

struct Frame {
    value: *mut u8,
    identity: *mut u8,
    keys: *mut u8,
    next: usize,
    object: bool,
    emitted: bool,
}

#[derive(Default)]
struct Text {
    data: *mut u8,
    length: u32,
    capacity: u32,
}

impl Text {
    fn append(&mut self, text: &str) -> Result<(), Error> {
        let size = u32::try_from(text.len()).map_err(|_| Error::Capacity)?;
        let length = self.length.checked_add(size).ok_or(Error::Capacity)?;
        if size == 0 {
            return Ok(());
        }
        // SAFETY: only this builder accesses its buffer; inputs never alias it.
        unsafe {
            if length > self.capacity {
                self.capacity = length.saturating_mul(2).max(64);
                let data = perry_alloc(self.capacity);
                if self.length != 0 {
                    ptr::copy_nonoverlapping(self.data, data, self.length as usize);
                }
                self.data = data;
            }
            ptr::copy_nonoverlapping(
                text.as_ptr(),
                self.data.add(self.length as usize),
                text.len(),
            );
        }
        self.length = length;
        Ok(())
    }

    fn line(&mut self, gap: Result<&str, Error>, depth: usize) -> Result<(), Error> {
        let gap = gap?;
        if !gap.is_empty() {
            self.append("\n")?;
            for _ in 0..depth {
                self.append(gap)?;
            }
        }
        Ok(())
    }
}

struct Replacer {
    frames: [Option<Frame>; MAX_DEPTH],
    depth: usize,
    phase: Phase,
    gap: Result<&'static str, Error>,
    output: Text,
}

impl Replacer {
    unsafe fn apply(&mut self, value: *mut u8) -> Result<(), Error> {
        let Phase::Callback { key, .. } = self.phase else {
            panic!("replacer result without a callback");
        };
        self.phase = Phase::Traverse;
        // SAFETY: callback values and keys stay live for this task; frames own key snapshots.
        unsafe {
            let parent = self
                .depth
                .checked_sub(1)
                .map(|i| self.frames[i].as_mut().expect("active serializer frame"));
            if value.is_null() && parent.as_ref().is_none_or(|frame| frame.object) {
                return Ok(());
            }
            if self.depth >= MAX_DEPTH {
                return Err(Error::Depth(self.depth));
            }
            let container = match RuntimeTree.value(Node::Value(value))? {
                Value::Array(_) => Some(false),
                Value::Object(_) => Some(true),
                _ => None,
            };
            let identity = if container.is_some() {
                Some(RuntimeTree.identity(Node::Value(value))?)
            } else {
                None
            };
            if let Some(parent) = parent {
                if parent.emitted {
                    self.output.append(",")?;
                }
                self.output.line(self.gap, self.depth)?;
                if parent.object {
                    let quoted = perry_json_stringify(perry_json_string(key));
                    self.output.append(string::text(quoted))?;
                    self.output
                        .append(if self.gap?.is_empty() { ":" } else { ": " })?;
                }
                parent.emitted = true;
            }
            if let Some(object) = container {
                let identity = identity.expect("container identity");
                if self.frames[..self.depth].iter().any(|frame| {
                    frame
                        .as_ref()
                        .is_some_and(|frame| frame.identity == identity)
                }) {
                    return Err(Error::Cycle);
                }
                self.output.append(if object { "{" } else { "[" })?;
                self.frames[self.depth] = Some(Frame {
                    value,
                    identity,
                    keys: properties::keys(*value.cast::<Json>()),
                    next: 0,
                    object,
                    emitted: false,
                });
                self.depth += 1;
            } else if value.is_null() {
                self.output.append("null")?;
            } else {
                let scalar = perry_json_stringify(value);
                self.output.append(string::text(scalar))?;
            }
        }
        Ok(())
    }
}

/// # Safety
/// Both arguments are undefined or live JSON values belonging to the current task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_replacer_start(value: *mut u8, space: *mut u8) -> *mut u8 {
    let state = perry_alloc(size_of::<Replacer>() as u32).cast::<Replacer>();
    // SAFETY: fresh task storage owns the cursor; the gap borrows immutable task strings.
    unsafe {
        state.write(Replacer {
            frames: [const { None }; MAX_DEPTH],
            depth: 0,
            phase: Phase::Callback {
                key: string::copy(""),
                value,
            },
            gap: stringify::gap(space),
            output: Text::default(),
        });
    }
    state.cast()
}

/// # Safety
/// `state` is live, newly started or resumed after applying a callback result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_replacer_next(state: *mut u8) -> u32 {
    // SAFETY: generated code exclusively drives this task-owned cursor.
    unsafe {
        let state = &mut *state.cast::<Replacer>();
        if matches!(state.phase, Phase::Callback { .. }) {
            return 1;
        }
        while state.depth > 0 {
            let frame = state.frames[state.depth - 1]
                .as_mut()
                .expect("active serializer frame");
            if let Some(&key) = array::slots(frame.keys).get(frame.next) {
                frame.next += 1;
                let key = key as usize as *mut u8;
                let value = perry_json_get(frame.value, key);
                if exception::perry_exception_status() != 0 {
                    return 0;
                }
                state.phase = Phase::Callback { key, value };
                return 1;
            }
            let result = if frame.emitted {
                state.output.line(state.gap, state.depth - 1)
            } else {
                Ok(())
            }
            .and_then(|()| state.output.append(if frame.object { "}" } else { "]" }));
            if let Err(error) = result {
                stringify::stringify_error(error);
                return 0;
            }
            state.depth -= 1;
            state.frames[state.depth] = None;
        }
        state.phase = Phase::Done;
        0
    }
}

/// # Safety
/// `state` is live and its next callback has been requested.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_replacer_key(state: *mut u8) -> *mut u8 {
    // SAFETY: the active callback retains a live immutable key.
    unsafe {
        let Phase::Callback { key, .. } = (*state.cast::<Replacer>()).phase else {
            panic!("replacer key outside callback");
        };
        key
    }
}

/// # Safety
/// `state` is live and its next callback has been requested.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_replacer_value(state: *mut u8) -> *mut u8 {
    // SAFETY: the value was read from the live holder immediately before this callback.
    unsafe {
        let Phase::Callback { value, .. } = (*state.cast::<Replacer>()).phase else {
            panic!("replacer value outside callback");
        };
        value
    }
}

/// # Safety
/// `state` awaits a callback result; `value` is undefined or a live JSON-compatible value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_replacer_apply(state: *mut u8, value: *mut u8) {
    // SAFETY: each cursor is driven exclusively by generated code within one task.
    if let Err(error) = unsafe { (*state.cast::<Replacer>()).apply(value) } {
        stringify::stringify_error(error);
    }
}

/// # Safety
/// `state` is live and its traversal has completed without an exception.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_replacer_result(state: *mut u8) -> *mut u8 {
    // SAFETY: completed output is valid UTF-8, and its task-owned bytes remain immutable.
    unsafe {
        let state = &*state.cast::<Replacer>();
        assert!(matches!(state.phase, Phase::Done));
        if state.output.length == 0 {
            ptr::null_mut()
        } else {
            string::perry_string_adopt(state.output.data, state.output.length)
        }
    }
}
