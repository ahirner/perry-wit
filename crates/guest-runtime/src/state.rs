use std::borrow::Cow;
use std::fmt::Write;

use crate::nanbox::{
    get_pointer_id, nanbox_pointer, nanbox_string, POINTER_TAG, STRING_TAG, TAG_FALSE, TAG_NULL,
    TAG_TRUE, TAG_UNDEFINED,
};

#[derive(Clone, Debug)]
pub(crate) enum JsHandle {
    Null,
    Json(serde_json::Value),
    Array(Vec<i64>),
    Response { id: usize, headers: Option<i64> },
    Headers(Vec<(String, String)>),
    Date(f64),
    Uint8Array(crate::buffer::Uint8ArrayView),
}

pub(crate) struct RuntimeState {
    pub(crate) strings: Vec<Vec<u16>>,
    pub(crate) handles: Vec<JsHandle>,
    pub(crate) current_exception: Option<String>,
    pub(crate) process_env: Option<i64>,
    pub(crate) process_argv: Option<i64>,
    pub(crate) init_strings_len: usize,
    pub(crate) init_handles_len: usize,
    pub(crate) global_roots: Vec<i64>,
    pub(crate) free_strings: Vec<usize>,
    pub(crate) free_handles: Vec<usize>,
    pub(crate) invocation_in_progress: bool,
    pub(crate) worklist: Vec<i64>,
    pub(crate) reachable_strings: Vec<bool>,
    pub(crate) reachable_handles: Vec<bool>,
    pub(crate) pending_return_area: Option<(i32, usize)>,
}

impl RuntimeState {
    fn new() -> Self {
        Self {
            strings: Vec::new(),
            handles: vec![JsHandle::Null],
            current_exception: None,
            process_env: None,
            process_argv: None,
            init_strings_len: 0,
            init_handles_len: 1,
            global_roots: Vec::new(),
            free_strings: Vec::new(),
            free_handles: Vec::new(),
            invocation_in_progress: false,
            worklist: Vec::new(),
            reachable_strings: Vec::new(),
            reachable_handles: Vec::new(),
            pending_return_area: None,
        }
    }

    pub(crate) fn record_init_checkpoint(&mut self) {
        self.init_strings_len = self.strings.len();
        self.init_handles_len = self.handles.len();
    }

    pub(crate) fn reset_invocation_state(&mut self) {
        self.current_exception = None;
        if let Some((ptr, words)) = self.pending_return_area.take() {
            crate::cabi::free_pending_return_area(ptr, words);
        }
        if self.invocation_in_progress {
            self.reclaim_temporaries();
        }
        self.invocation_in_progress = true;
    }

    pub(crate) fn register_root(&mut self, val: i64) {
        self.global_roots.push(val);
    }

    pub(crate) fn reclaim_temporaries(&mut self) {
        self.invocation_in_progress = false;
        self.pending_return_area = None;

        let num_strings = self.strings.len();
        let num_handles = self.handles.len();

        self.reachable_strings.clear();
        self.reachable_strings.resize(num_strings, false);
        self.reachable_handles.clear();
        self.reachable_handles.resize(num_handles, false);

        // 1. Initial module strings are roots.
        let init_strings = self.init_strings_len.min(num_strings);
        for i in 0..init_strings {
            self.reachable_strings[i] = true;
        }

        // 2. Initial handles, registered global roots, and environment handles.
        self.worklist.clear();
        let init_handles = self.init_handles_len.min(num_handles);
        for i in 0..init_handles {
            self.worklist.push(nanbox_pointer(i));
        }
        self.worklist.extend_from_slice(&self.global_roots);
        self.global_roots.clear();
        if let Some(env) = self.process_env {
            self.worklist.push(env);
        }
        if let Some(argv) = self.process_argv {
            self.worklist.push(argv);
        }

        // 3. Mark reachable strings and handles.
        while let Some(val) = self.worklist.pop() {
            let bits = val as u64;
            if (bits >> 48) == STRING_TAG {
                let id = (bits & 0xFFFF_FFFF) as usize;
                if id < num_strings && !self.reachable_strings[id] {
                    self.reachable_strings[id] = true;
                }
            } else if (bits >> 48) == POINTER_TAG {
                let id = (bits & 0xFFFF_FFFF) as usize;
                if id < num_handles && !self.reachable_handles[id] {
                    self.reachable_handles[id] = true;
                    if let Some(handle) = self.handles.get(id) {
                        match handle {
                            JsHandle::Array(arr) => {
                                for &item in arr {
                                    self.worklist.push(item);
                                }
                            }
                            JsHandle::Response {
                                headers: Some(headers),
                                ..
                            } => self.worklist.push(*headers),
                            _ => {}
                        }
                    }
                }
            }
        }

        // 4. Find the high-water mark of surviving items.
        let max_alive_string = self
            .reachable_strings
            .iter()
            .rposition(|&r| r)
            .map(|i| i + 1)
            .unwrap_or(init_strings)
            .max(init_strings);

        let max_alive_handle = self
            .reachable_handles
            .iter()
            .rposition(|&r| r)
            .map(|i| i + 1)
            .unwrap_or(init_handles)
            .max(init_handles);

        // 5. Clean up dead items between init and max_alive, recycling slots.
        self.free_strings.clear();
        self.free_handles.clear();

        for i in init_strings..max_alive_string {
            if !self.reachable_strings[i] {
                self.strings[i] = Vec::new();
                self.free_strings.push(i);
            }
        }
        for i in init_handles..max_alive_handle {
            if !self.reachable_handles[i] {
                self.handles[i] = JsHandle::Null;
                self.free_handles.push(i);
            }
        }

        // 6. Truncate vectors past the highest surviving index.
        self.strings.truncate(max_alive_string);
        self.handles.truncate(max_alive_handle);
    }

    pub(crate) fn alloc_string(&mut self, text: &str) -> i64 {
        self.alloc_string_units(text.encode_utf16().collect())
    }

    pub(crate) fn alloc_string_units(&mut self, units: Vec<u16>) -> i64 {
        if let Some(id) = self.free_strings.pop() {
            self.strings[id] = units;
            nanbox_string(id)
        } else {
            let id = self.strings.len();
            self.strings.push(units);
            nanbox_string(id)
        }
    }

    pub(crate) fn string_units(&self, value: i64) -> Cow<'_, [u16]> {
        let bits = value as u64;
        if bits >> 48 == STRING_TAG {
            Cow::Borrowed(
                self.strings
                    .get((bits & 0xffff_ffff) as usize)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            )
        } else {
            Cow::Owned(self.get_string(value).encode_utf16().collect())
        }
    }

    /// String methods apply ToIntegerOrInfinity; property access uses element_index instead.
    pub(crate) fn string_code_unit(&self, value: i64, index: i64) -> Option<u16> {
        let number = self.to_number(index);
        let index = if number.is_nan() { 0.0 } else { number.trunc() };
        let units = self.string_units(value);
        if index >= 0.0 && index < units.len() as f64 {
            units.get(index as usize).copied()
        } else {
            None
        }
    }

    pub(crate) fn stringify(&self, value: i64) -> String {
        if (value as u64) >> 48 == STRING_TAG {
            quote_utf16(&self.string_units(value))
        } else if let Some(JsHandle::Array(items)) = self.get_handle(value) {
            format!(
                "[{}]",
                items
                    .iter()
                    .map(|&item| self.stringify(item))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        } else {
            serde_json::to_string(&self.to_js_value(value)).unwrap_or_else(|_| "null".into())
        }
    }

    pub(crate) fn alloc_handle(&mut self, h: JsHandle) -> usize {
        if let Some(id) = self.free_handles.pop() {
            self.handles[id] = h;
            id
        } else {
            let id = self.handles.len();
            self.handles.push(h);
            id
        }
    }

    pub(crate) fn get_handle(&self, val: i64) -> Option<&JsHandle> {
        let id = get_pointer_id(val)?;
        self.handles.get(id)
    }

    pub(crate) fn get_handle_mut(&mut self, val: i64) -> Option<&mut JsHandle> {
        let id = get_pointer_id(val)?;
        self.handles.get_mut(id)
    }

    pub(crate) fn object_property_value(&self, target: i64, value: i64) -> serde_json::Value {
        if self.process_env == Some(target) {
            serde_json::Value::String(self.coerce_string(value))
        } else {
            self.to_js_value(value)
        }
    }

    /// JavaScript string coercion for the runtime's primitive, array, and JSON values.
    pub(crate) fn coerce_string(&self, value: i64) -> String {
        match self.get_handle(value) {
            Some(JsHandle::Array(items)) => items
                .iter()
                .map(|&item| {
                    if matches!(item as u64, TAG_NULL | TAG_UNDEFINED) {
                        String::new()
                    } else {
                        self.coerce_string(item)
                    }
                })
                .collect::<Vec<_>>()
                .join(","),
            Some(JsHandle::Json(value)) => json_string_coercion(value),
            _ => match f64::from_bits(value as u64) {
                f64::INFINITY => "Infinity".into(),
                f64::NEG_INFINITY => "-Infinity".into(),
                _ => self.get_string(value),
            },
        }
    }

    pub(crate) fn to_js_value(&self, val: i64) -> serde_json::Value {
        let bits = val as u64;
        if bits == TAG_UNDEFINED || bits == TAG_NULL {
            serde_json::Value::Null
        } else if bits == TAG_TRUE {
            serde_json::Value::Bool(true)
        } else if bits == TAG_FALSE {
            serde_json::Value::Bool(false)
        } else if (bits >> 48) == STRING_TAG {
            let id = (bits & 0xFFFF_FFFF) as usize;
            serde_json::Value::String(String::from_utf16_lossy(
                self.strings.get(id).map(Vec::as_slice).unwrap_or_default(),
            ))
        } else if (bits >> 48) == POINTER_TAG {
            let id = (bits & 0xFFFF_FFFF) as usize;
            match self.handles.get(id) {
                Some(JsHandle::Json(v)) => v.clone(),
                Some(JsHandle::Array(arr)) => {
                    let items: Vec<serde_json::Value> =
                        arr.iter().map(|&elem| self.to_js_value(elem)).collect();
                    serde_json::Value::Array(items)
                }
                Some(JsHandle::Date(ts)) => {
                    if let Some(iso) = crate::date::format_iso(*ts) {
                        serde_json::Value::String(iso)
                    } else {
                        serde_json::Value::Null
                    }
                }
                Some(JsHandle::Uint8Array(view)) => {
                    let items = view
                        .to_vec()
                        .into_iter()
                        .enumerate()
                        .map(|(index, byte)| (index.to_string(), serde_json::Value::from(byte)))
                        .collect();
                    serde_json::Value::Object(items)
                }
                Some(JsHandle::Headers(_)) => serde_json::Value::Object(serde_json::Map::new()),
                _ => serde_json::Value::Null,
            }
        } else {
            let f = f64::from_bits(bits);
            if f.fract() == 0.0 && f >= (i64::MIN as f64) && f <= (i64::MAX as f64) {
                serde_json::Value::Number(serde_json::Number::from(f as i64))
            } else if let Some(num) = serde_json::Number::from_f64(f) {
                serde_json::Value::Number(num)
            } else {
                serde_json::Value::Null
            }
        }
    }

    /// Numeric coercion for the primitive values accepted by byte operations.
    pub(crate) fn to_number(&self, value: i64) -> f64 {
        match value as u64 {
            TAG_UNDEFINED => f64::NAN,
            TAG_NULL | TAG_FALSE => 0.0,
            TAG_TRUE => 1.0,
            bits if bits >> 48 == STRING_TAG => {
                crate::equality::string_number(&self.get_string(value))
            }
            bits if bits >> 48 == POINTER_TAG => match self.get_handle(value) {
                Some(JsHandle::Date(timestamp)) => *timestamp,
                Some(JsHandle::Array(items)) if items.is_empty() => 0.0,
                Some(JsHandle::Array(items)) if items.len() == 1 => {
                    if matches!(items[0] as u64, TAG_NULL | TAG_UNDEFINED) {
                        0.0
                    } else {
                        self.to_number(items[0])
                    }
                }
                _ => crate::equality::string_number(&self.get_string(value)),
            },
            bits => f64::from_bits(bits),
        }
    }

    /// Accepts only canonical, finite, nonnegative integer element keys.
    pub(crate) fn element_index(&self, key: i64) -> Option<usize> {
        let bits = key as u64;
        let number = if bits >> 48 == STRING_TAG {
            let text = self.get_string(key);
            let number = text.parse::<f64>().ok()?;
            if text == "-0" || number.to_string() != text {
                return None;
            }
            number
        } else if matches!(bits >> 48, 0x7ffc | POINTER_TAG) {
            return None;
        } else {
            f64::from_bits(bits)
        };
        (number.is_finite() && number >= 0.0 && number.fract() == 0.0 && number < usize::MAX as f64)
            .then_some(number as usize)
    }

    /// ECMAScript ToUint8: truncate finite numbers, then wrap modulo 256.
    pub(crate) fn to_uint8(&self, value: i64) -> u8 {
        let number = self.to_number(value);
        if number.is_finite() {
            number.trunc().rem_euclid(256.0) as u8
        } else {
            0
        }
    }

    pub(crate) fn from_js_value(&mut self, v: serde_json::Value) -> i64 {
        match v {
            serde_json::Value::Null => TAG_NULL as i64,
            serde_json::Value::Bool(true) => TAG_TRUE as i64,
            serde_json::Value::Bool(false) => TAG_FALSE as i64,
            serde_json::Value::Number(n) => {
                if let Some(f) = n.as_f64() {
                    f.to_bits() as i64
                } else if let Some(i) = n.as_i64() {
                    (i as f64).to_bits() as i64
                } else {
                    0
                }
            }
            serde_json::Value::String(s) => self.alloc_string(&s),
            serde_json::Value::Array(arr) => {
                let items: Vec<i64> = arr
                    .into_iter()
                    .map(|item| self.from_js_value(item))
                    .collect();
                let id = self.alloc_handle(JsHandle::Array(items));
                nanbox_pointer(id)
            }
            serde_json::Value::Object(_) => {
                let id = self.alloc_handle(JsHandle::Json(v));
                nanbox_pointer(id)
            }
        }
    }

    pub(crate) fn get_string(&self, val: i64) -> String {
        let bits = val as u64;
        if bits == TAG_UNDEFINED {
            return "undefined".to_string();
        }
        if bits == TAG_NULL {
            return "null".to_string();
        }
        if bits == TAG_TRUE {
            return "true".to_string();
        }
        if bits == TAG_FALSE {
            return "false".to_string();
        }
        if (bits >> 48) == STRING_TAG {
            let id = (bits & 0xFFFF_FFFF) as usize;
            if let Some(s) = self.strings.get(id) {
                return String::from_utf16_lossy(s);
            }
        } else if (bits >> 48) == POINTER_TAG {
            let id = (bits & 0xFFFF_FFFF) as usize;
            if let Some(h) = self.handles.get(id) {
                return match h {
                    JsHandle::Json(serde_json::Value::String(s)) => s.clone(),
                    JsHandle::Json(other) => serde_json::to_string(other).unwrap_or_default(),
                    JsHandle::Array(arr) => format!("[array len {}]", arr.len()),
                    JsHandle::Response { .. } => "[Response]".to_string(),
                    JsHandle::Headers(_) => "[object Headers]".to_string(),
                    JsHandle::Null => "null".to_string(),
                    JsHandle::Date(ts) => {
                        crate::date::format_iso(*ts).unwrap_or_else(|| "Invalid Date".to_string())
                    }
                    JsHandle::Uint8Array(view) => {
                        let bytes = view.to_vec();
                        let parts: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
                        parts.join(",")
                    }
                };
            }
        } else {
            let f = f64::from_bits(bits);
            if f.fract() == 0.0 && f >= (i64::MIN as f64) && f <= (i64::MAX as f64) {
                return format!("{}", f as i64);
            } else {
                return format!("{f}");
            }
        }
        format!("{val}")
    }
}

pub(crate) fn json_string_coercion(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| {
                if value.is_null() {
                    String::new()
                } else {
                    json_string_coercion(value)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        serde_json::Value::Object(_) => "[object Object]".into(),
        value => value.to_string(),
    }
}

/// JSON escapes unpaired UTF-16 surrogates instead of replacing their code units.
fn quote_utf16(units: &[u16]) -> String {
    let mut quoted = String::from("\"");
    for decoded in char::decode_utf16(units.iter().copied()) {
        match decoded {
            Ok('"') => quoted.push_str("\\\""),
            Ok('\\') => quoted.push_str("\\\\"),
            Ok('\n') => quoted.push_str("\\n"),
            Ok('\r') => quoted.push_str("\\r"),
            Ok('\t') => quoted.push_str("\\t"),
            Ok('\u{8}') => quoted.push_str("\\b"),
            Ok('\u{c}') => quoted.push_str("\\f"),
            Ok(c) if c < '\u{20}' => {
                write!(quoted, "\\u{:04x}", c as u32).unwrap();
            }
            Ok(c) => quoted.push(c),
            Err(error) => {
                write!(quoted, "\\u{:04x}", error.unpaired_surrogate()).unwrap();
            }
        }
    }
    quoted.push('"');
    quoted
}

static mut STATE: Option<RuntimeState> = None;

pub(crate) fn get_state() -> &'static mut RuntimeState {
    unsafe {
        let state_ptr = core::ptr::addr_of_mut!(STATE);
        if (*state_ptr).is_none() {
            *state_ptr = Some(RuntimeState::new());
        }
        (*state_ptr).as_mut().unwrap()
    }
}
