//! Guest object properties retain values rather than JSON snapshots.

use crate::nanbox::{nanbox_pointer, STRING_TAG, TAG_UNDEFINED};
use crate::state::{get_state, JsHandle};

#[no_mangle]
pub(crate) extern "C" fn object_new() -> i64 {
    nanbox_pointer(get_state().alloc_handle(JsHandle::Object(ObjectProperties::default())))
}

#[no_mangle]
pub(crate) extern "C" fn object_set(target: i64, key: i64, value: i64) -> i64 {
    let state = get_state();
    let key = state.get_string(key);
    let stored = if state.process_env == Some(target) {
        let text = state.coerce_string(value);
        state.alloc_string(&text)
    } else {
        value
    };
    if let Some(JsHandle::Object(properties)) = state.get_handle_mut(target) {
        properties.insert(key, stored);
    }
    value
}

#[no_mangle]
pub(crate) extern "C" fn object_get(target: i64, key: i64) -> i64 {
    let state = get_state();
    let key = state.get_string(key);
    match state.get_handle(target) {
        Some(JsHandle::Object(properties)) => properties.get(&key).unwrap_or(TAG_UNDEFINED as i64),
        Some(JsHandle::Uint8Array(view)) => match key.as_str() {
            "length" | "byteLength" => (view.byte_length as f64).to_bits() as i64,
            "byteOffset" => (view.byte_offset as f64).to_bits() as i64,
            _ => TAG_UNDEFINED as i64,
        },
        _ => TAG_UNDEFINED as i64,
    }
}

#[no_mangle]
pub(crate) extern "C" fn object_get_dynamic(target: i64, key: i64) -> i64 {
    let state = get_state();
    let index = state.element_index(key);
    match state.get_handle(target) {
        Some(JsHandle::Object(_)) => object_get(target, key),
        Some(JsHandle::Array(items)) => index
            .and_then(|index| items.get(index).copied())
            .unwrap_or(TAG_UNDEFINED as i64),
        Some(JsHandle::Uint8Array(view)) => index
            .and_then(|index| view.get(index))
            .map_or(TAG_UNDEFINED as i64, |byte| (byte as f64).to_bits() as i64),
        _ if (target as u64) >> 48 == STRING_TAG => {
            let unit = index.and_then(|index| state.string_units(target).get(index).copied());
            unit.map_or(TAG_UNDEFINED as i64, |unit| {
                state.alloc_string_units(vec![unit])
            })
        }
        _ => TAG_UNDEFINED as i64,
    }
}

#[no_mangle]
pub(crate) extern "C" fn object_set_dynamic(target: i64, key: i64, value: i64) {
    let state = get_state();
    if matches!(state.get_handle(target), Some(JsHandle::Object(_))) {
        object_set(target, key, value);
        return;
    }
    let index = state.element_index(key);
    let byte = state.to_uint8(value);
    match (state.get_handle_mut(target), index) {
        (Some(JsHandle::Uint8Array(view)), Some(index)) => view.set(index, byte),
        (Some(JsHandle::Array(items)), Some(index)) if index < items.len() => items[index] = value,
        (Some(JsHandle::Array(items)), Some(index)) if index == items.len() => items.push(value),
        _ => {}
    }
}

#[no_mangle]
pub(crate) extern "C" fn object_delete(target: i64, key: i64) {
    let state = get_state();
    let key = state.get_string(key);
    if let Some(JsHandle::Object(properties)) = state.get_handle_mut(target) {
        properties.remove(&key);
    }
}

#[no_mangle]
pub(crate) extern "C" fn object_delete_dynamic(target: i64, key: i64) {
    object_delete(target, key);
}

#[no_mangle]
pub(crate) extern "C" fn object_keys(target: i64) -> i64 {
    let state = get_state();
    let entries = enumerable_entries(target);
    let keys = entries
        .into_iter()
        .map(|(key, _)| state.alloc_string(&key))
        .collect();
    nanbox_pointer(state.alloc_handle(JsHandle::Array(keys)))
}

#[no_mangle]
pub(crate) extern "C" fn object_values(target: i64) -> i64 {
    let entries = enumerable_entries(target);
    let values = entries.into_iter().map(|(_, value)| value).collect();
    nanbox_pointer(get_state().alloc_handle(JsHandle::Array(values)))
}

#[no_mangle]
pub(crate) extern "C" fn object_entries(target: i64) -> i64 {
    let state = get_state();
    let entries = enumerable_entries(target)
        .into_iter()
        .map(|(key, value)| {
            let key = state.alloc_string(&key);
            nanbox_pointer(state.alloc_handle(JsHandle::Array(vec![key, value])))
        })
        .collect();
    nanbox_pointer(state.alloc_handle(JsHandle::Array(entries)))
}

#[no_mangle]
pub(crate) extern "C" fn object_has_property(target: i64, key: i64) -> i32 {
    let state = get_state();
    let key = state.get_string(key);
    i32::from(
        matches!(state.get_handle(target), Some(JsHandle::Object(properties)) if properties.get(&key).is_some()),
    )
}

#[no_mangle]
pub(crate) extern "C" fn object_assign(target: i64, source: i64) -> i64 {
    for (key, value) in enumerable_entries(source) {
        let key = get_state().alloc_string(&key);
        object_set(target, key, value);
    }
    target
}

fn enumerable_entries(target: i64) -> Vec<(String, i64)> {
    let state = get_state();
    match state.get_handle(target) {
        Some(JsHandle::Object(properties)) => properties
            .entries()
            .map(|(key, value)| (key.clone(), *value))
            .collect(),
        Some(JsHandle::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(index, &value)| (index.to_string(), value))
            .collect(),
        Some(JsHandle::Uint8Array(view)) => view
            .to_vec()
            .into_iter()
            .enumerate()
            .map(|(index, byte)| (index.to_string(), (byte as f64).to_bits() as i64))
            .collect(),
        _ if (target as u64) >> 48 == STRING_TAG => {
            let units = state.string_units(target).into_owned();
            units
                .into_iter()
                .enumerate()
                .map(|(index, unit)| (index.to_string(), state.alloc_string_units(vec![unit])))
                .collect()
        }
        _ => Vec::new(),
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ObjectProperties {
    fields: Vec<(String, i64)>,
}

impl ObjectProperties {
    pub(crate) fn get(&self, key: &str) -> Option<i64> {
        self.fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| *value)
    }

    pub(crate) fn insert(&mut self, key: String, value: i64) {
        if let Some((_, stored)) = self.fields.iter_mut().find(|(name, _)| name == &key) {
            *stored = value;
        } else {
            self.fields.push((key, value));
        }
    }

    pub(crate) fn remove(&mut self, key: &str) {
        self.fields.retain(|(name, _)| name != key);
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &(String, i64)> {
        let mut entries: Vec<_> = self.fields.iter().collect();
        entries.sort_by_key(|(key, _)| {
            key.parse::<u32>()
                .ok()
                .filter(|&index| index != u32::MAX && index.to_string() == *key)
                .map_or((1, 0), |index| (0, index))
        });
        entries.into_iter()
    }
}
