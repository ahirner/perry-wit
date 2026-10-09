//! Post-order JSON traversal suspended between inlined guest callback invocations.
use super::*;

const MAX_DEPTH: usize = 128;

struct Frame {
    holder: *mut u8,
    key: *mut u8,
    value: *mut u8,
    keys: *mut u8,
    next: usize,
    original_node: u32,
    original_array: Option<<Graph<'static> as Tree>::Children>,
}

impl Frame {
    unsafe fn new(snapshot: &Snapshot, holder: *mut u8, key: *mut u8, node: u32) -> Self {
        // SAFETY: each holder is a live aggregate and the key remains task-owned.
        unsafe {
            let value = perry_json_get(holder, key);
            let original_node = if node != 0 {
                let index = snapshot
                    .graph
                    .node_index(node)
                    .expect("validated snapshot index");
                let original = snapshot.values.add(index as usize).read();
                let unchanged = match (
                    node_value(Node::Value(original)),
                    node_value(Node::Value(value)),
                ) {
                    (Some(Json::Number(a)), Some(Json::Number(b))) => {
                        a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
                    }
                    _ => perry_json_equal(original, value) != 0,
                };
                if unchanged { node } else { 0 }
            } else {
                0
            };
            let original_array = if original_node == 0 {
                None
            } else {
                match snapshot
                    .graph
                    .value(original_node)
                    .expect("validated snapshot")
                {
                    Value::Array(children) => Some(children),
                    _ => None,
                }
            };
            let keys = match node_value(Node::Value(value)) {
                Some(value @ (Json::Array(_) | Json::Object(_) | Json::Native { .. })) => {
                    properties::keys(value)
                }
                _ => ptr::null_mut(),
            };
            Self {
                holder,
                key,
                value,
                keys,
                next: 0,
                original_node,
                original_array,
            }
        }
    }
}

enum Phase {
    Traverse,
    Callback,
    Done,
}

struct Reviver {
    snapshot: Snapshot,
    frames: [Option<Frame>; MAX_DEPTH],
    depth: usize,
    phase: Phase,
    result: *mut u8,
}

struct Snapshot {
    graph: Graph<'static>,
    values: *mut *mut u8,
    text: *mut u8,
}

/// # Safety
/// `text` is a live string in the current task. Parse failures leave a pending exception.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_reviver_start(text: *mut u8) -> *mut u8 {
    // SAFETY: parsing and source snapshots borrow this immutable task string.
    let (graph, root) = match parse_graph(unsafe { string::text(text) }) {
        Ok(parsed) => parsed,
        Err(error) => return parse_failure(error),
    };
    let slots = graph.node_index(root).expect("validated root index") as usize + 1;
    let bytes = slots
        .checked_mul(size_of::<*mut u8>())
        .and_then(|size| u32::try_from(size).ok());
    let Some(bytes) = bytes else {
        return parse_failure(Error::Capacity);
    };
    let values = perry_alloc(bytes).cast::<*mut u8>();
    let value = from_graph(&graph, root, &mut |node, value| {
        let index = graph.node_index(node).expect("validated snapshot index");
        // SAFETY: each value has one in-range post-order slot, initialized before traversal.
        unsafe {
            values.add(index as usize).write(value);
        }
    })
    .expect("validated snapshot graph");
    let snapshot = Snapshot {
        graph,
        values,
        text,
    };
    let state = perry_alloc(size_of::<Reviver>() as u32).cast::<Reviver>();
    let holder = Object::new(1);
    let key = string::copy("");
    // SAFETY: the wrapper and state are fresh task allocations; all referenced values stay live.
    unsafe {
        (*holder).set(key, value as usize as u64);
        state.write(Reviver {
            snapshot,
            frames: [const { None }; MAX_DEPTH],
            depth: 1,
            phase: Phase::Traverse,
            result: value,
        });
        (*state).frames[0] = Some(Frame::new(
            &(*state).snapshot,
            allocate(Json::Object(holder)),
            key,
            root,
        ));
    }
    state.cast()
}

/// # Safety
/// `state` is a live traversal, started or advanced by applying the previous callback result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_reviver_next(state: *mut u8) -> u32 {
    // SAFETY: generated code exclusively drives this task-owned state in the documented order.
    unsafe {
        let state = &mut *state.cast::<Reviver>();
        if matches!(state.phase, Phase::Done) {
            return 0;
        }
        assert!(matches!(state.phase, Phase::Traverse));
        loop {
            let frame = state.frames[state.depth - 1]
                .as_mut()
                .expect("active reviver frame");
            let key = if frame.keys.is_null() {
                None
            } else {
                array::slots(frame.keys).get(frame.next).copied()
            };
            let Some(key) = key else {
                state.phase = Phase::Callback;
                return 1;
            };
            if state.depth == MAX_DEPTH {
                failure(
                    exception::RANGE_ERROR,
                    "JSON reviver traversal is too deeply nested",
                );
                return 0;
            }
            frame.next += 1;
            let key = key as usize as *mut u8;
            let original = if let Some(children) = &mut frame.original_array {
                children
                    .next()
                    .transpose()
                    .expect("validated snapshot children")
                    .unwrap_or(0)
            } else if frame.original_node != 0 {
                state
                    .snapshot
                    .graph
                    .property(frame.original_node, string::text(key))
                    .expect("validated snapshot properties")
                    .unwrap_or(0)
            } else {
                0
            };
            let child = Frame::new(&state.snapshot, frame.value, key, original);
            if exception::perry_exception_status() != 0 {
                return 0;
            }
            state.frames[state.depth] = Some(child);
            state.depth += 1;
        }
    }
}

/// # Safety
/// `state` is live and its next callback has been requested.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_reviver_key(state: *mut u8) -> *mut u8 {
    // SAFETY: the traversal retains the active frame and its task-owned string.
    unsafe {
        let state = &*state.cast::<Reviver>();
        assert!(matches!(state.phase, Phase::Callback));
        state.frames[state.depth - 1]
            .as_ref()
            .expect("active reviver frame")
            .key
    }
}

/// # Safety
/// `state` is live and its next callback has been requested.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_reviver_value(state: *mut u8) -> *mut u8 {
    // SAFETY: this is the value captured on entry, even if a descendant changed its holder.
    unsafe {
        let state = &*state.cast::<Reviver>();
        assert!(matches!(state.phase, Phase::Callback));
        state.frames[state.depth - 1]
            .as_ref()
            .expect("active reviver frame")
            .value
    }
}

/// # Safety
/// `state` is live and its next callback has been requested.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_reviver_context(state: *mut u8) -> *mut u8 {
    // SAFETY: metadata and source bytes are immutable task storage independent of guest mutations.
    unsafe {
        let state = &*state.cast::<Reviver>();
        assert!(matches!(state.phase, Phase::Callback));
        let frame = state.frames[state.depth - 1]
            .as_ref()
            .expect("active reviver frame");
        let context = Object::new(1);
        if frame.original_node != 0 && frame.keys.is_null() {
            let source = state
                .snapshot
                .graph
                .source(frame.original_node, string::text(state.snapshot.text))
                .expect("validated source token");
            let text = string::perry_string_adopt(source.as_ptr().cast_mut(), source.len() as u32);
            (*context).set(
                string::copy("source"),
                perry_json_string(text) as usize as u64,
            );
        }
        allocate(Json::Object(context))
    }
}

/// # Safety
/// `state` awaits a callback result; `value` is undefined or a live JSON-compatible value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_reviver_apply(state: *mut u8, value: *mut u8) {
    // SAFETY: no frame retains property-table entries across guest callbacks or mutations.
    unsafe {
        let state = &mut *state.cast::<Reviver>();
        assert!(matches!(state.phase, Phase::Callback));
        let frame = state.frames[state.depth - 1]
            .take()
            .expect("active reviver frame");
        if value.is_null() {
            perry_json_delete(frame.holder, frame.key);
        } else if let Json::Object(holder) = *frame.holder.cast::<Json>() {
            // InternalizeJSONProperty creates an own data property, including __proto__.
            (*holder).set(frame.key, value as usize as u64);
        } else {
            perry_json_set(frame.holder, frame.key, value);
        }
        state.depth -= 1;
        state.phase = if state.depth == 0 {
            state.result = value;
            Phase::Done
        } else {
            Phase::Traverse
        };
    }
}

/// # Safety
/// `state` is live and traversal has completed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_reviver_result(state: *mut u8) -> *mut u8 {
    // SAFETY: the result is task-owned and survives release of the traversal state.
    unsafe {
        let state = &*state.cast::<Reviver>();
        assert!(matches!(state.phase, Phase::Done));
        state.result
    }
}

#[cfg(test)]
pub(crate) fn reviver_snapshot_mutations() {
    crate::region::perry_task_enter();
    // SAFETY: the fixture drives the same cursor protocol as codegen in one live task.
    unsafe {
        let state = perry_json_reviver_start(string::copy(
            r#"{"first":1,"number":-0,"same":2.00,"child":{"value":3},"gone":5,"__proto__":4}"#,
        ));
        let root = (*state.cast::<Reviver>()).frames[0].as_ref().unwrap().value;
        let mut visited = 0;
        while perry_json_reviver_next(state) != 0 {
            let key = string::text(perry_json_reviver_key(state));
            let mut value = perry_json_reviver_value(state);
            let context = perry_json_reviver_context(state);
            let source = perry_json_get(context, string::copy("source"));
            match key {
                "first" => {
                    assert_eq!(string::text(perry_json_as_string(source)), "1");
                    perry_json_set(root, string::copy("number"), perry_json_number(0.0));
                    perry_json_set(root, string::copy("same"), perry_json_number(2.0));
                    let replacement = perry_json_parse(string::copy(r#"{"value":3}"#));
                    perry_json_set(root, string::copy("child"), replacement);
                    perry_json_delete(root, string::copy("gone"));
                    perry_json_set(root, string::copy("added"), perry_json_number(9.0));
                }
                "same" => assert_eq!(string::text(perry_json_as_string(source)), "2.00"),
                "__proto__" => {
                    assert_eq!(string::text(perry_json_as_string(source)), "4");
                    perry_json_delete(root, string::copy("__proto__"));
                    value = perry_json_number(44.0);
                }
                "gone" => assert!(source.is_null() && value.is_null()),
                "number" | "child" | "value" | "" => assert!(source.is_null()),
                _ => panic!("new keys must not enter the traversal snapshot"),
            }
            perry_json_reviver_apply(state, value);
            assert_eq!(exception::perry_exception_status(), 0);
            visited += 1;
        }
        assert_eq!(visited, 8);
        assert_eq!(perry_json_reviver_result(state), root);
        assert_eq!(
            string::text(perry_json_stringify(root)),
            r#"{"first":1,"number":0,"same":2,"child":{"value":3},"added":9,"__proto__":44}"#
        );
    }
    crate::region::perry_task_free();
}
