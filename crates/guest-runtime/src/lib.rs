#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "runtime-adapter",
        generate_all,
    });
}

use bindings::wasi::cli::exit::exit;
use bindings::wasi::cli::stderr::get_stderr;
use bindings::wasi::cli::stdout::get_stdout;
use bindings::wasi::http::outgoing_handler::handle;
use bindings::wasi::http::types::{
    Fields, FutureIncomingResponse, Method, OutgoingBody, OutgoingRequest, Scheme,
};

const STRING_TAG: u64 = 0x7FFF;
const POINTER_TAG: u64 = 0x7FFD;
const TAG_UNDEFINED: u64 = 0x7FFC_0000_0000_0001;
const TAG_NULL: u64 = 0x7FFC_0000_0000_0002;
const TAG_FALSE: u64 = 0x7FFC_0000_0000_0003;
const TAG_TRUE: u64 = 0x7FFC_0000_0000_0004;

fn nanbox_string(id: usize) -> i64 {
    let bits = (STRING_TAG << 48) | (id as u64 & 0xFFFF_FFFF);
    bits as i64
}

fn nanbox_pointer(id: usize) -> i64 {
    let bits = (POINTER_TAG << 48) | (id as u64 & 0xFFFF_FFFF);
    bits as i64
}

fn get_string_id(val: i64) -> Option<usize> {
    let bits = val as u64;
    if (bits >> 48) == STRING_TAG {
        Some((bits & 0xFFFF_FFFF) as usize)
    } else {
        None
    }
}

fn get_pointer_id(val: i64) -> Option<usize> {
    let bits = val as u64;
    if (bits >> 48) == POINTER_TAG {
        Some((bits & 0xFFFF_FFFF) as usize)
    } else {
        None
    }
}

fn print_stdout(text: &str) {
    let stdout = get_stdout();
    let _ = stdout.blocking_write_and_flush(text.as_bytes());
}

fn print_stderr(text: &str) {
    let stderr = get_stderr();
    let _ = stderr.blocking_write_and_flush(text.as_bytes());
}

fn fail_with_error(msg: &str) -> ! {
    print_stderr(&format!("Error: {msg}\n"));
    exit(Err(()));
    core::arch::wasm32::unreachable();
}

fn split_url(url: &str) -> Result<(Scheme, String, String), String> {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (Scheme::Https, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (Scheme::Http, rest)
    } else {
        return Err(format!("Unsupported scheme in URL: {url}"));
    };

    let (authority, path) = match rest.find('/') {
        Some(pos) => (&rest[..pos], &rest[pos..]),
        None => (rest, "/"),
    };

    Ok((scheme, authority.to_string(), path.to_string()))
}

pub enum ResponseEntry {
    InFlight {
        url: String,
        future_resp: FutureIncomingResponse,
    },
    Ready(String),
}

impl ResponseEntry {
    pub fn resolve(&mut self) -> Result<&str, String> {
        match self {
            ResponseEntry::Ready(s) => Ok(s.as_str()),
            ResponseEntry::InFlight { url, future_resp } => {
                let pollable = future_resp.subscribe();
                pollable.block();

                let response = future_resp
                    .get()
                    .ok_or_else(|| format!("Response for {url} not available"))?
                    .map_err(|_| format!("Response for {url} already consumed"))?
                    .map_err(|e| format!("HTTP request to '{url}' failed: {e:?}"))?;

                let status = response.status();
                if status < 200 || status >= 300 {
                    return Err(format!("HTTP request to '{url}' returned status {status}"));
                }

                let body = response
                    .consume()
                    .map_err(|_| format!("Failed to consume response body for {url}"))?;
                let stream = body
                    .stream()
                    .map_err(|_| format!("Failed to get response stream for {url}"))?;

                let mut content = Vec::new();
                loop {
                    match stream.blocking_read(8192) {
                        Ok(chunk) => {
                            if chunk.is_empty() {
                                break;
                            }
                            content.extend_from_slice(&chunk);
                        }
                        Err(bindings::wasi::io::streams::StreamError::Closed) => {
                            break;
                        }
                        Err(e) => {
                            return Err(format!("Stream error reading response from {url}: {e:?}"));
                        }
                    }
                }

                let body_str = String::from_utf8(content)
                    .map_err(|e| format!("Response from {url} was not UTF-8: {e}"))?;
                drop(stream);
                drop(body);
                drop(response);
                drop(pollable);

                *self = ResponseEntry::Ready(body_str);
                match self {
                    ResponseEntry::Ready(s) => Ok(s.as_str()),
                    _ => unreachable!(),
                }
            }
        }
    }
}

fn start_http_get(url: &str) -> Result<FutureIncomingResponse, String> {
    let (scheme, authority, path) = split_url(url)?;

    let headers = Fields::new();
    let request = OutgoingRequest::new(headers);
    request
        .set_method(&Method::Get)
        .map_err(|_| "Failed to set method")?;
    request
        .set_scheme(Some(&scheme))
        .map_err(|_| "Failed to set scheme")?;
    request
        .set_authority(Some(&authority))
        .map_err(|_| "Failed to set authority")?;
    request
        .set_path_with_query(Some(&path))
        .map_err(|_| "Failed to set path")?;

    let outgoing_body = request.body().map_err(|_| "Failed to get request body")?;
    OutgoingBody::finish(outgoing_body, None).map_err(|_| "Failed to finish outgoing body")?;

    let future_resp = handle(request, None).map_err(|e| format!("HTTP handle error: {e:?}"))?;
    Ok(future_resp)
}

#[derive(Clone, Debug)]
pub enum JsHandle {
    Null,
    Json(serde_json::Value),
    Array(Vec<i64>),
    Response(usize),
}

pub struct RuntimeState {
    pub strings: Vec<String>,
    pub responses: Vec<ResponseEntry>,
    pub handles: Vec<JsHandle>,
}

impl RuntimeState {
    fn new() -> Self {
        Self {
            strings: Vec::new(),
            responses: Vec::new(),
            handles: vec![JsHandle::Null],
        }
    }

    pub fn alloc_handle(&mut self, h: JsHandle) -> usize {
        let id = self.handles.len();
        self.handles.push(h);
        id
    }

    pub fn get_handle(&self, val: i64) -> Option<&JsHandle> {
        let id = get_pointer_id(val)?;
        self.handles.get(id)
    }

    pub fn get_handle_mut(&mut self, val: i64) -> Option<&mut JsHandle> {
        let id = get_pointer_id(val)?;
        self.handles.get_mut(id)
    }

    pub fn get_response_body(&mut self, id: usize) -> Result<String, String> {
        if id < self.responses.len() {
            let res = self.responses[id].resolve()?.to_string();
            Ok(res)
        } else {
            Err(format!("Invalid response id {id}"))
        }
    }

    pub fn to_js_value(&self, val: i64) -> serde_json::Value {
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

    pub fn from_js_value(&mut self, v: serde_json::Value) -> i64 {
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

    pub fn get_string(&self, val: i64) -> String {
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

fn get_state() -> &'static mut RuntimeState {
    unsafe {
        if STATE.is_none() {
            STATE = Some(RuntimeState::new());
        }
        STATE.as_mut().unwrap()
    }
}

#[no_mangle]
pub extern "C" fn string_new(offset: i32, len: i32) {
    let state = get_state();
    let slice = unsafe { std::slice::from_raw_parts(offset as *const u8, len as usize) };
    if let Ok(s) = std::str::from_utf8(slice) {
        state.strings.push(s.to_string());
    }
}

#[no_mangle]
pub extern "C" fn console_log(val: i64) {
    let state = get_state();
    let msg = state.get_string(val);
    print_stdout(&format!("{msg}\n"));
}

#[no_mangle]
pub extern "C" fn mem_call(func_name_id: f64, arg_count: f64, base_addr: i32) -> f64 {
    let state = get_state();
    let name_idx = func_name_id as usize;
    let name = state
        .strings
        .get(name_idx)
        .map(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let count = arg_count as usize;
    let mut raw_args = Vec::with_capacity(count);
    let ptr = base_addr as *const i64;
    for i in 0..count {
        raw_args.push(unsafe { *ptr.add(i) });
    }

    let mut result_i64: i64 = 0;

    if name == "fetch_url" || name == "fetch" || name == "fetch_with_options" {
        if let Some(&url_arg) = raw_args.first() {
            let url = state.get_string(url_arg);
            match start_http_get(&url) {
                Ok(fut) => {
                    let id = state.responses.len();
                    state.responses.push(ResponseEntry::InFlight {
                        url,
                        future_resp: fut,
                    });
                    let h_id = state.alloc_handle(JsHandle::Response(id));
                    result_i64 = nanbox_pointer(h_id);
                }
                Err(e) => {
                    fail_with_error(&format!("HTTP fetch initialization error for {url}: {e}"));
                }
            }
        }
    } else if name == "all" {
        // Promise.all(iterable)
        let arr_arg = if raw_args.len() >= 2 {
            raw_args[1]
        } else {
            raw_args.first().copied().unwrap_or(0)
        };
        if let Some(JsHandle::Array(items)) = state.get_handle(arr_arg).cloned() {
            for &item in &items {
                if let Some(JsHandle::Response(resp_id)) = state.get_handle(item) {
                    let resp_id = *resp_id;
                    if let Err(e) = state.get_response_body(resp_id) {
                        fail_with_error(&e);
                    }
                }
            }
            let res_arr_id = state.alloc_handle(JsHandle::Array(items));
            result_i64 = nanbox_pointer(res_arr_id);
        }
    } else if name == "response_json" || name == "json" {
        let handle = raw_args.first().copied().unwrap_or(0);
        let resp_id = match state.get_handle(handle) {
            Some(JsHandle::Response(id)) => Some(*id),
            _ => get_pointer_id(handle),
        };
        if let Some(id) = resp_id {
            match state.get_response_body(id) {
                Ok(body) => match serde_json::from_str::<serde_json::Value>(&body) {
                    Ok(parsed) => {
                        let h_id = state.alloc_handle(JsHandle::Json(parsed));
                        result_i64 = nanbox_pointer(h_id);
                    }
                    Err(e) => {
                        fail_with_error(&format!("JSON parse error: {e}"));
                    }
                },
                Err(e) => {
                    fail_with_error(&e);
                }
            }
        } else {
            fail_with_error("Invalid response handle passed to .json()");
        }
    } else if name == "response_text" || name == "text" {
        let handle = raw_args.first().copied().unwrap_or(0);
        let resp_id = match state.get_handle(handle) {
            Some(JsHandle::Response(id)) => Some(*id),
            _ => get_pointer_id(handle),
        };
        if let Some(id) = resp_id {
            match state.get_response_body(id) {
                Ok(body) => {
                    let str_id = state.strings.len();
                    state.strings.push(body);
                    result_i64 = nanbox_string(str_id);
                }
                Err(e) => {
                    fail_with_error(&e);
                }
            }
        } else {
            fail_with_error("Invalid response handle passed to .text()");
        }
    } else if name == "array_new" {
        let h_id = state.alloc_handle(JsHandle::Array(Vec::new()));
        result_i64 = nanbox_pointer(h_id);
    } else if name == "array_push" {
        if raw_args.len() >= 2 {
            let arr_handle = raw_args[0];
            let item = raw_args[1];
            if let Some(JsHandle::Array(arr)) = state.get_handle_mut(arr_handle) {
                arr.push(item);
            }
            result_i64 = arr_handle;
        }
    } else if name == "array_get" || name == "object_get_dynamic" {
        if raw_args.len() >= 2 {
            let target_handle = raw_args[0];
            let idx_val = raw_args[1];
            let bits = idx_val as u64;
            let idx = if (bits >> 48) < 0x7ff8 {
                f64::from_bits(bits) as usize
            } else {
                (bits & 0xFFFF_FFFF) as usize
            };
            if let Some(h) = state.get_handle(target_handle).cloned() {
                match h {
                    JsHandle::Array(arr) => {
                        if let Some(&elem) = arr.get(idx) {
                            result_i64 = elem;
                        }
                    }
                    JsHandle::Json(serde_json::Value::Array(arr)) => {
                        if let Some(elem) = arr.get(idx) {
                            result_i64 = state.from_js_value(elem.clone());
                        }
                    }
                    JsHandle::Json(serde_json::Value::Object(map)) => {
                        let key_str = state.get_string(idx_val);
                        if let Some(v) = map.get(&key_str) {
                            result_i64 = state.from_js_value(v.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
    } else if name == "string_len" || name == "array_length" {
        let arg = raw_args.first().copied().unwrap_or(0);
        if let Some(h) = state.get_handle(arg) {
            match h {
                JsHandle::Array(arr) => {
                    result_i64 = (arr.len() as f64).to_bits() as i64;
                }
                JsHandle::Json(serde_json::Value::Array(arr)) => {
                    result_i64 = (arr.len() as f64).to_bits() as i64;
                }
                _ => {
                    let len = state.get_string(arg).len();
                    result_i64 = (len as f64).to_bits() as i64;
                }
            }
        } else {
            let len = state.get_string(arg).len();
            result_i64 = (len as f64).to_bits() as i64;
        }
    } else if name == "object_new" {
        let h_id = state.alloc_handle(JsHandle::Json(serde_json::Value::Object(
            serde_json::Map::new(),
        )));
        result_i64 = nanbox_pointer(h_id);
    } else if name == "object_set" {
        if raw_args.len() >= 3 {
            let target_handle = raw_args[0];
            let key_str = state.get_string(raw_args[1]);
            let val_json = state.to_js_value(raw_args[2]);
            if let Some(JsHandle::Json(serde_json::Value::Object(map))) =
                state.get_handle_mut(target_handle)
            {
                map.insert(key_str, val_json);
            }
            result_i64 = target_handle;
        }
    } else if name == "object_assign" {
        if raw_args.len() >= 2 {
            let target_handle = raw_args[0];
            let source_handle = raw_args[1];
            let source_json = state.to_js_value(source_handle);
            if let Some(JsHandle::Json(serde_json::Value::Object(target_map))) =
                state.get_handle_mut(target_handle)
            {
                if let serde_json::Value::Object(src_map) = source_json {
                    for (k, v) in src_map {
                        target_map.insert(k, v);
                    }
                }
            }
            result_i64 = target_handle;
        }
    } else if name == "object_get" {
        if raw_args.len() >= 2 {
            let target_handle = raw_args[0];
            let key_str = state.get_string(raw_args[1]);
            if let Some(JsHandle::Json(serde_json::Value::Object(map))) =
                state.get_handle(target_handle)
            {
                if let Some(v) = map.get(&key_str) {
                    let v_clone = v.clone();
                    result_i64 = state.from_js_value(v_clone);
                }
            }
        }
    } else if name == "json_stringify" {
        let arg = raw_args.first().copied().unwrap_or(0);
        let val_json = state.to_js_value(arg);
        let json_str = serde_json::to_string_pretty(&val_json).unwrap_or_else(|_| "{}".to_string());
        let str_id = state.strings.len();
        state.strings.push(json_str);
        result_i64 = nanbox_string(str_id);
    } else if name == "json_parse" {
        let arg = raw_args.first().copied().unwrap_or(0);
        let s = state.get_string(arg);
        let val_json: serde_json::Value =
            serde_json::from_str(&s).unwrap_or(serde_json::Value::Null);
        result_i64 = state.from_js_value(val_json);
    } else if name == "console_log" || name == "log" {
        let arg = raw_args.last().copied().unwrap_or(0);
        let msg = state.get_string(arg);
        print_stdout(&format!("{msg}\n"));
    } else if name == "string_concat" || name == "js_add" {
        if raw_args.len() >= 2 {
            let s_a = state.get_string(raw_args[0]);
            let s_b = state.get_string(raw_args[1]);
            let res = format!("{s_a}{s_b}");
            let str_id = state.strings.len();
            state.strings.push(res);
            result_i64 = nanbox_string(str_id);
        }
    } else if name == "await_promise" {
        let arg = raw_args.first().copied().unwrap_or(0);
        if let Some(JsHandle::Response(resp_id)) = state.get_handle(arg) {
            let resp_id = *resp_id;
            if let Err(e) = state.get_response_body(resp_id) {
                fail_with_error(&e);
            }
        }
        result_i64 = arg;
    }

    // Write result back to base_addr
    unsafe {
        *(base_addr as *mut i64) = result_i64;
    }

    0.0
}

#[no_mangle]
pub extern "C" fn mem_call_i32(func_name_id: f64, arg_count: f64, base_addr: i32) -> i32 {
    let state = get_state();
    let name_idx = func_name_id as usize;
    let name = state
        .strings
        .get(name_idx)
        .map(|s| s.as_str())
        .unwrap_or("");
    let count = arg_count as usize;

    let mut raw_args = Vec::with_capacity(count);
    let ptr = base_addr as *const i64;
    for i in 0..count {
        raw_args.push(unsafe { *ptr.add(i) });
    }

    if name == "is_truthy" {
        if let Some(&arg) = raw_args.first() {
            let bits = arg as u64;
            if bits == TAG_UNDEFINED || bits == TAG_NULL || bits == TAG_FALSE {
                return 0;
            }
            if bits == TAG_TRUE {
                return 1;
            }
            let f = f64::from_bits(bits);
            if f == 0.0 || f.is_nan() {
                return 0;
            }
            return 1;
        }
    }

    0
}

// -----------------------------------------------------------------------------
// Auto-generated runtime function stubs (208 stubs for static link compatibility)
// -----------------------------------------------------------------------------
#[no_mangle]
pub extern "C" fn console_warn(_a: i64) {}
#[no_mangle]
pub extern "C" fn console_error(_a: i64) {}
#[no_mangle]
pub extern "C" fn string_concat(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn js_add(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn string_eq(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn string_len(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn jsvalue_to_string(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn jsvalue_to_template_string(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn is_truthy(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn js_strict_eq(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn math_floor(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn math_ceil(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn math_round(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn math_abs(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn math_sqrt(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn math_pow(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn math_random() -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn math_log(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn date_now() -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn js_typeof(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn math_min(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn math_max(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn parse_int(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn parse_float(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn js_mod(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn is_null_or_undefined(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn object_new() -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn object_set(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn object_get(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn object_get_dynamic(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn object_set_dynamic(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub extern "C" fn object_delete(_a: i64, _b: i64) {}
#[no_mangle]
pub extern "C" fn object_delete_dynamic(_a: i64, _b: i64) {}
#[no_mangle]
pub extern "C" fn object_keys(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn object_values(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn object_entries(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn object_has_property(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn object_assign(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_new() -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_push(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_pop(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_get(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_set(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub extern "C" fn array_length(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_slice(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_splice(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_shift(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_unshift(_a: i64, _b: i64) {}
#[no_mangle]
pub extern "C" fn array_join(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_index_of(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_includes(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn array_concat(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_reverse(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_flat(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_is_array(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn array_from(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_push_spread(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn string_charAt(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn string_substring(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn string_indexOf(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn string_slice(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn string_toLowerCase(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn string_toUpperCase(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn string_trim(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn string_includes(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn string_startsWith(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn string_endsWith(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn string_replace(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn string_split(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn string_fromCharCode(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn string_padStart(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn string_padEnd(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn string_repeat(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn string_match(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn math_log2(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn math_log10(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn closure_new(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn closure_set_capture(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn closure_call_0(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn closure_call_1(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn closure_call_2(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn closure_call_3(_a: i64, _b: i64, _c: i64, _d: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn closure_call_spread(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_map(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_filter(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_forEach(_a: i64, _b: i64) {}
#[no_mangle]
pub extern "C" fn array_reduce(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_find(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_find_index(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_sort(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn array_some(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn array_every(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn class_new(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn class_set_method(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub extern "C" fn class_call_method(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn class_get_field(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn class_set_field(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub extern "C" fn class_set_static(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub extern "C" fn class_get_static(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn class_instanceof(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn json_parse(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn json_stringify(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn map_new() -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn map_set(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub extern "C" fn map_get(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn map_has(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn map_delete(_a: i64, _b: i64) {}
#[no_mangle]
pub extern "C" fn map_size(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn map_clear(_a: i64) {}
#[no_mangle]
pub extern "C" fn map_entries(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn map_keys(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn map_values(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn set_new() -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn set_new_from_array(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn set_add(_a: i64, _b: i64) {}
#[no_mangle]
pub extern "C" fn set_has(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn set_delete(_a: i64, _b: i64) {}
#[no_mangle]
pub extern "C" fn set_size(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn set_clear(_a: i64) {}
#[no_mangle]
pub extern "C" fn set_values(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn date_new_val(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn date_get_time(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn date_to_iso_string(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn date_get_full_year(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn date_get_month(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn date_get_date(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn date_get_day(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn date_get_hours(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn date_get_minutes(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn date_get_seconds(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn date_get_milliseconds(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn error_new(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn error_message(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn regexp_new(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn regexp_test(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn number_coerce(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn is_nan(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn is_finite(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn console_log_multi(_a: i64) {}
#[no_mangle]
pub extern "C" fn class_set_parent(_a: i64, _b: i64) {}
#[no_mangle]
pub extern "C" fn try_start() {}
#[no_mangle]
pub extern "C" fn try_end() {}
#[no_mangle]
pub extern "C" fn throw_value(_a: i64) {}
#[no_mangle]
pub extern "C" fn has_exception() -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn get_exception() -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn url_parse(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn url_get_href(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn url_get_pathname(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn url_get_hostname(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn url_get_port(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn url_get_search(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn url_get_hash(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn url_get_origin(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn url_get_protocol(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn url_get_search_params(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn searchparams_get(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn searchparams_has(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn searchparams_set(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub extern "C" fn searchparams_append(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub extern "C" fn searchparams_delete(_a: i64, _b: i64) {}
#[no_mangle]
pub extern "C" fn searchparams_to_string(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn crypto_random_uuid() -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn crypto_random_bytes(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn path_join(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn path_dirname(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn path_basename(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn path_extname(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn path_resolve(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn os_platform() -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn process_argv() -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn process_cwd() -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn buffer_alloc(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn buffer_from_string(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn buffer_to_string(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn buffer_get(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn buffer_set(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub extern "C" fn buffer_length(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn buffer_slice(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn buffer_concat(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn uint8array_new(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn uint8array_from(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn uint8array_length(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn uint8array_get(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn uint8array_set(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub extern "C" fn set_timeout(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn set_interval(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn clear_timeout(_a: i64) {}
#[no_mangle]
pub extern "C" fn clear_interval(_a: i64) {}
#[no_mangle]
pub extern "C" fn response_status(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn response_ok(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn response_headers_get(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn response_url(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn buffer_copy(_a: i64, _b: i64, _c: i64, _d: i64, _e: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn buffer_write(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn buffer_equals(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn buffer_is_buffer(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn buffer_byte_length(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn crypto_sha256(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn crypto_md5(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn path_is_absolute(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub extern "C" fn fetch_url(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn fetch_with_options(_a: i64, _b: i64, _c: i64, _d: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn response_json(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn response_text(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn promise_new() -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn promise_resolve(_a: i64, _b: i64) {}
#[no_mangle]
pub extern "C" fn promise_then(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub extern "C" fn await_promise(_a: i64) -> i64 {
    0
}
