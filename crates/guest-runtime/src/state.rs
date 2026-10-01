//! Runtime handle storage and JS value conversion.

use crate::http::ResponseEntry;
use crate::nanbox::{
    get_pointer_id, nanbox_pointer, nanbox_string, POINTER_TAG, STRING_TAG, TAG_FALSE, TAG_NULL,
    TAG_TRUE, TAG_UNDEFINED,
};

#[derive(Clone, Debug)]
pub(crate) enum JsHandle {
    Null,
    Json(serde_json::Value),
    Array(Vec<i64>),
    Response(usize),
}

pub(crate) struct RuntimeState {
    pub(crate) strings: Vec<String>,
    pub(crate) responses: Vec<ResponseEntry>,
    pub(crate) handles: Vec<JsHandle>,
}

impl RuntimeState {
    fn new() -> Self {
        Self {
            strings: Vec::new(),
            responses: Vec::new(),
            handles: vec![JsHandle::Null],
        }
    }

    pub(crate) fn alloc_handle(&mut self, h: JsHandle) -> usize {
        let id = self.handles.len();
        self.handles.push(h);
        id
    }

    pub(crate) fn get_handle(&self, val: i64) -> Option<&JsHandle> {
        let id = get_pointer_id(val)?;
        self.handles.get(id)
    }

    pub(crate) fn get_handle_mut(&mut self, val: i64) -> Option<&mut JsHandle> {
        let id = get_pointer_id(val)?;
        self.handles.get_mut(id)
    }

    pub(crate) fn get_response_body(&mut self, id: usize) -> Result<String, String> {
        if id < self.responses.len() {
            let res = self.responses[id].resolve()?.to_string();
            Ok(res)
        } else {
            Err(format!("Invalid response id {id}"))
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
            serde_json::Value::String(self.strings.get(id).cloned().unwrap_or_default())
        } else if (bits >> 48) == POINTER_TAG {
            let id = (bits & 0xFFFF_FFFF) as usize;
            match self.handles.get(id) {
                Some(JsHandle::Json(v)) => v.clone(),
                Some(JsHandle::Array(arr)) => {
                    let items: Vec<serde_json::Value> =
                        arr.iter().map(|&elem| self.to_js_value(elem)).collect();
                    serde_json::Value::Array(items)
                }
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
            serde_json::Value::String(s) => {
                let id = self.strings.len();
                self.strings.push(s);
                nanbox_string(id)
            }
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
                return s.clone();
            }
        } else if (bits >> 48) == POINTER_TAG {
            let id = (bits & 0xFFFF_FFFF) as usize;
            if let Some(h) = self.handles.get(id) {
                return match h {
                    JsHandle::Json(serde_json::Value::String(s)) => s.clone(),
                    JsHandle::Json(other) => serde_json::to_string(other).unwrap_or_default(),
                    JsHandle::Array(arr) => format!("[array len {}]", arr.len()),
                    JsHandle::Response(_) => "[Response]".to_string(),
                    JsHandle::Null => "null".to_string(),
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
