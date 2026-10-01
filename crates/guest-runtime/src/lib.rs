#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "merge-docs",
        generate_all,
    });
}

use bindings::exports::wasi::cli::run::Guest;
use bindings::wasi::cli::stdout::get_stdout;
use bindings::wasi::http::outgoing_handler::handle;
use bindings::wasi::http::types::{
    Fields, FutureIncomingResponse, Method, OutgoingBody, OutgoingRequest, Scheme,
};

use wasmi::{Caller, Engine, ExternType, Func, Linker, Memory, Module, Store, Val, ValType};

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

pub struct RuntimeState {
    memory: Option<Memory>,
    strings: Vec<String>,
    responses: Vec<ResponseEntry>,
    handles: Vec<serde_json::Value>,
}

impl RuntimeState {
    fn new() -> Self {
        Self {
            memory: None,
            strings: Vec::new(),
            responses: Vec::new(),
            handles: vec![serde_json::Value::Null],
        }
    }

    fn alloc_handle(&mut self, val: serde_json::Value) -> usize {
        let id = self.handles.len();
        self.handles.push(val);
        id
    }

    fn get_response_body(&mut self, id: usize) -> Result<String, String> {
        if id < self.responses.len() {
            let res = self.responses[id].resolve()?.to_string();
            Ok(res)
        } else {
            Err(format!("Invalid response id {id}"))
        }
    }

    fn to_js_value(&self, val: i64) -> serde_json::Value {
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
            self.handles.get(id).cloned().unwrap_or(serde_json::Value::Null)
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

    fn from_js_value(&mut self, v: serde_json::Value) -> i64 {
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
            serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                let id = self.alloc_handle(v);
                nanbox_pointer(id)
            }
        }
    }

    fn get_string(&self, val: i64) -> String {
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
                    serde_json::Value::String(s) => s.clone(),
                    other => serde_json::to_string(other).unwrap_or_default(),
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

fn print_stdout(text: &str) {
    let stdout = get_stdout();
    let _ = stdout.blocking_write_and_flush(text.as_bytes());
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
    InFlight(FutureIncomingResponse),
    Ready(String),
}

impl ResponseEntry {
    pub fn resolve(&mut self) -> Result<&str, String> {
        match self {
            ResponseEntry::Ready(s) => Ok(s.as_str()),
            ResponseEntry::InFlight(future_resp) => {
                let pollable = future_resp.subscribe();
                pollable.block();

                let response = future_resp
                    .get()
                    .ok_or_else(|| "Response not available".to_string())?
                    .map_err(|_| "Response already consumed".to_string())?
                    .map_err(|e| format!("HTTP request error: {e:?}"))?;

                let status = response.status();
                if status < 200 || status >= 300 {
                    return Err(format!("HTTP status {status}"));
                }

                let body = response.consume().map_err(|_| "Failed to consume response body")?;
                let stream = body.stream().map_err(|_| "Failed to get response stream")?;

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
                            return Err(format!("Stream error reading response: {e:?}"));
                        }
                    }
                }

                let body_str = String::from_utf8(content).map_err(|e| format!("Response was not UTF-8: {e}"))?;
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
    request.set_method(&Method::Get).map_err(|_| "Failed to set method")?;
    request.set_scheme(Some(&scheme)).map_err(|_| "Failed to set scheme")?;
    request.set_authority(Some(&authority)).map_err(|_| "Failed to set authority")?;
    request.set_path_with_query(Some(&path)).map_err(|_| "Failed to set path")?;

    let outgoing_body = request.body().map_err(|_| "Failed to get request body")?;
    OutgoingBody::finish(outgoing_body, None).map_err(|_| "Failed to finish outgoing body")?;

    let future_resp = handle(request, None).map_err(|e| format!("HTTP handle error: {e:?}"))?;
    Ok(future_resp)
}

// Embedded core wasm bytecode generated by perry from examples/merge_docs.ts
static CORE_WASM: &[u8] = include_bytes!("../../../dist/merge_docs.core.wasm");

struct Component;

impl Guest for Component {
    fn run() -> Result<(), ()> {
        let engine = Engine::default();
        let module = match Module::new(&engine, CORE_WASM) {
            Ok(m) => m,
            Err(e) => {
                print_stdout(&format!("Module error: {e:?}\n"));
                return Err(());
            }
        };
        let mut store = Store::new(&engine, RuntimeState::new());
        let mut linker = Linker::new(&engine);

        // Define string_new(offset: i32, len: i32)
        linker
            .define(
                "rt",
                "string_new",
                Func::wrap(&mut store, |mut caller: Caller<'_, RuntimeState>, offset: i32, len: i32| {
                    let memory = match caller.get_export("memory").and_then(wasmi::Extern::into_memory).or(caller.data().memory) {
                        Some(m) => m,
                        None => {
                            print_stdout("string_new: memory not found!\n");
                            return;
                        }
                    };
                    let mut buf = vec![0u8; len as usize];
                    if memory.read(&caller, offset as usize, &mut buf).is_ok() {
                        if let Ok(s) = std::str::from_utf8(&buf) {
                            caller.data_mut().strings.push(s.to_string());
                        }
                    }
                }),
            )
            .map_err(|_| ())?;

        // Define string_concat(a: i64, b: i64) -> i64
        linker
            .define(
                "rt",
                "string_concat",
                Func::wrap(&mut store, |mut caller: Caller<'_, RuntimeState>, a: i64, b: i64| -> i64 {
                    let s_a = caller.data().get_string(a);
                    let s_b = caller.data().get_string(b);
                    let res = format!("{s_a}{s_b}");
                    let id = caller.data().strings.len();
                    caller.data_mut().strings.push(res);
                    nanbox_string(id)
                }),
            )
            .map_err(|_| ())?;

        // Define js_add(a: i64, b: i64) -> i64
        linker
            .define(
                "rt",
                "js_add",
                Func::wrap(&mut store, |mut caller: Caller<'_, RuntimeState>, a: i64, b: i64| -> i64 {
                    if get_string_id(a).is_some() || get_string_id(b).is_some() {
                        let s_a = caller.data().get_string(a);
                        let s_b = caller.data().get_string(b);
                        let res = format!("{s_a}{s_b}");
                        let id = caller.data().strings.len();
                        caller.data_mut().strings.push(res);
                        nanbox_string(id)
                    } else {
                        a + b
                    }
                }),
            )
            .map_err(|_| ())?;

        // Define console_log(val: i64)
        linker
            .define(
                "rt",
                "console_log",
                Func::wrap(&mut store, |caller: Caller<'_, RuntimeState>, val: i64| {
                    let msg = caller.data().get_string(val);
                    print_stdout(&format!("{msg}\n"));
                }),
            )
            .map_err(|_| ())?;

        // Define fetch_url(url_val: i64) -> i64
        linker
            .define(
                "rt",
                "fetch_url",
                Func::wrap(&mut store, |mut caller: Caller<'_, RuntimeState>, url_val: i64| -> i64 {
                    let url = caller.data().get_string(url_val);
                    match start_http_get(&url) {
                        Ok(fut) => {
                            let id = caller.data().responses.len();
                            caller.data_mut().responses.push(ResponseEntry::InFlight(fut));
                            nanbox_pointer(id)
                        }
                        Err(e) => {
                            print_stdout(&format!("HTTP Error: {e}\n"));
                            0
                        }
                    }
                }),
            )
            .map_err(|_| ())?;

        // Define fetch_with_options(url_val, ...) -> i64
        linker
            .define(
                "rt",
                "fetch_with_options",
                Func::wrap(
                    &mut store,
                    |mut caller: Caller<'_, RuntimeState>,
                     url_val: i64,
                     _method: i64,
                     _body: i64,
                     _headers: i64|
                     -> i64 {
                        let url = caller.data().get_string(url_val);
                        match start_http_get(&url) {
                            Ok(fut) => {
                                let id = caller.data().responses.len();
                                caller.data_mut().responses.push(ResponseEntry::InFlight(fut));
                                nanbox_pointer(id)
                            }
                            Err(e) => {
                                print_stdout(&format!("HTTP Error: {e}\n"));
                                0
                            }
                        }
                    },
                ),
            )
            .map_err(|_| ())?;

        // Define response_text(resp_val: i64) -> i64
        linker
            .define(
                "rt",
                "response_text",
                Func::wrap(&mut store, |mut caller: Caller<'_, RuntimeState>, resp_val: i64| -> i64 {
                    if let Some(id) = get_pointer_id(resp_val) {
                        if let Ok(body) = caller.data_mut().get_response_body(id) {
                            let str_id = caller.data().strings.len();
                            caller.data_mut().strings.push(body);
                            return nanbox_string(str_id);
                        }
                    }
                    0
                }),
            )
            .map_err(|_| ())?;

        // Define response_json(resp_val: i64) -> i64
        linker
            .define(
                "rt",
                "response_json",
                Func::wrap(&mut store, |mut caller: Caller<'_, RuntimeState>, resp_val: i64| -> i64 {
                    if let Some(id) = get_pointer_id(resp_val) {
                        if let Ok(body) = caller.data_mut().get_response_body(id) {
                            let parsed: serde_json::Value =
                                serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                            let h_id = caller.data_mut().alloc_handle(parsed);
                            return nanbox_pointer(h_id);
                        }
                    }
                    0
                }),
            )
            .map_err(|_| ())?;

        // Define json_stringify(val: i64) -> i64
        linker
            .define(
                "rt",
                "json_stringify",
                Func::wrap(&mut store, |mut caller: Caller<'_, RuntimeState>, val: i64| -> i64 {
                    let val_json = caller.data().to_js_value(val);
                    let json_str = serde_json::to_string_pretty(&val_json).unwrap_or_else(|_| "{}".to_string());
                    let str_id = caller.data().strings.len();
                    caller.data_mut().strings.push(json_str);
                    nanbox_string(str_id)
                }),
            )
            .map_err(|_| ())?;

        // Define jsvalue_to_template_string(val: i64) -> i64
        linker
            .define(
                "rt",
                "jsvalue_to_template_string",
                Func::wrap(&mut store, |_caller: Caller<'_, RuntimeState>, val: i64| -> i64 {
                    val
                }),
            )
            .map_err(|_| ())?;

        // Define jsvalue_to_string(val: i64) -> i64
        linker
            .define(
                "rt",
                "jsvalue_to_string",
                Func::wrap(&mut store, |_caller: Caller<'_, RuntimeState>, val: i64| -> i64 {
                    val
                }),
            )
            .map_err(|_| ())?;

        // Define mem_call(name_id: f64, arg_count: f64, base_addr: i32) -> f64
        linker
            .define(
                "rt",
                "mem_call",
                Func::wrap(
                    &mut store,
                    |mut caller: Caller<'_, RuntimeState>,
                     name_id: f64,
                     arg_count: f64,
                     base_addr: i32|
                     -> f64 {
                        let name = caller
                            .data()
                            .strings
                            .get(name_id as usize)
                            .cloned()
                            .unwrap_or_else(|| format!("unknown_id_{name_id}"));
                        let argc = arg_count as usize;
                        let base = base_addr as usize;

                        let memory = match caller
                            .get_export("memory")
                            .and_then(wasmi::Extern::into_memory)
                            .or(caller.data().memory)
                        {
                            Some(m) => m,
                            None => {
                                print_stdout("mem_call: no memory!\n");
                                return 0.0;
                            }
                        };

                        let mut raw_args = Vec::new();
                        for i in 0..argc {
                            let mut buf = [0u8; 8];
                            if memory.read(&caller, base + i * 8, &mut buf).is_ok() {
                                raw_args.push(i64::from_le_bytes(buf));
                            }
                        }

                        let mut result_i64: i64 = 0;

                        if name == "fetch_url" || name == "fetch" || name == "fetch_with_options" {
                            if let Some(&url_arg) = raw_args.first() {
                                let url = caller.data().get_string(url_arg);
                                match start_http_get(&url) {
                                    Ok(fut) => {
                                        let id = caller.data().responses.len();
                                        caller.data_mut().responses.push(ResponseEntry::InFlight(fut));
                                        result_i64 = nanbox_pointer(id);
                                    }
                                    Err(e) => {
                                        print_stdout(&format!("HTTP Error: {e}\n"));
                                    }
                                }
                            }
                        } else if name == "response_json" || name == "json" {
                            let handle = raw_args.first().copied().unwrap_or(0);
                            if let Some(id) = get_pointer_id(handle) {
                                match caller.data_mut().get_response_body(id) {
                                    Ok(body) => {
                                        let parsed: serde_json::Value =
                                            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                                        let h_id = caller.data_mut().alloc_handle(parsed);
                                        result_i64 = nanbox_pointer(h_id);
                                    }
                                    Err(e) => {
                                        print_stdout(&format!("Response JSON error: {e}\n"));
                                    }
                                }
                            }
                        } else if name == "response_text" || name == "text" {
                            let handle = raw_args.first().copied().unwrap_or(0);
                            if let Some(id) = get_pointer_id(handle) {
                                match caller.data_mut().get_response_body(id) {
                                    Ok(body) => {
                                        let str_id = caller.data().strings.len();
                                        caller.data_mut().strings.push(body);
                                        result_i64 = nanbox_string(str_id);
                                    }
                                    Err(e) => {
                                        print_stdout(&format!("Response text error: {e}\n"));
                                    }
                                }
                            }
                        } else if name == "object_new" {
                            let h_id = caller
                                .data_mut()
                                .alloc_handle(serde_json::Value::Object(serde_json::Map::new()));
                            result_i64 = nanbox_pointer(h_id);
                        } else if name == "object_set" {
                            if raw_args.len() >= 3 {
                                let target_handle = raw_args[0];
                                let key_str = caller.data().get_string(raw_args[1]);
                                let val_json = caller.data().to_js_value(raw_args[2]);
                                if let Some(id) = get_pointer_id(target_handle) {
                                    if let Some(serde_json::Value::Object(map)) =
                                        caller.data_mut().handles.get_mut(id)
                                    {
                                        map.insert(key_str, val_json);
                                    }
                                }
                                result_i64 = target_handle;
                            }
                        } else if name == "object_assign" {
                            if raw_args.len() >= 2 {
                                let target_handle = raw_args[0];
                                let source_handle = raw_args[1];
                                let source_json = caller.data().to_js_value(source_handle);
                                if let Some(id) = get_pointer_id(target_handle) {
                                    if let Some(serde_json::Value::Object(target_map)) =
                                        caller.data_mut().handles.get_mut(id)
                                    {
                                        if let serde_json::Value::Object(src_map) = source_json {
                                            for (k, v) in src_map {
                                                target_map.insert(k, v);
                                            }
                                        }
                                    }
                                }
                                result_i64 = target_handle;
                            }
                        } else if name == "object_get" {
                            if raw_args.len() >= 2 {
                                let target_handle = raw_args[0];
                                let key_str = caller.data().get_string(raw_args[1]);
                                if let Some(id) = get_pointer_id(target_handle) {
                                    if let Some(serde_json::Value::Object(map)) =
                                        caller.data().handles.get(id)
                                    {
                                        if let Some(v) = map.get(&key_str) {
                                            let v_clone = v.clone();
                                            result_i64 = caller.data_mut().from_js_value(v_clone);
                                        }
                                    }
                                }
                            }
                        } else if name == "json_stringify" {
                            let arg = raw_args.first().copied().unwrap_or(0);
                            let val_json = caller.data().to_js_value(arg);
                            let json_str = serde_json::to_string_pretty(&val_json)
                                .unwrap_or_else(|_| "{}".to_string());
                            let str_id = caller.data().strings.len();
                            caller.data_mut().strings.push(json_str);
                            result_i64 = nanbox_string(str_id);
                        } else if name == "json_parse" {
                            let arg = raw_args.first().copied().unwrap_or(0);
                            let s = caller.data().get_string(arg);
                            let val_json: serde_json::Value =
                                serde_json::from_str(&s).unwrap_or(serde_json::Value::Null);
                            result_i64 = caller.data_mut().from_js_value(val_json);
                        } else if name == "console_log" || name == "log" {
                            let arg = raw_args.last().copied().unwrap_or(0);
                            let msg = caller.data().get_string(arg);
                            print_stdout(&format!("{msg}\n"));
                        } else if name == "string_concat" || name == "js_add" {
                            if raw_args.len() >= 2 {
                                let s_a = caller.data().get_string(raw_args[0]);
                                let s_b = caller.data().get_string(raw_args[1]);
                                let res = format!("{s_a}{s_b}");
                                let str_id = caller.data().strings.len();
                                caller.data_mut().strings.push(res);
                                result_i64 = nanbox_string(str_id);
                            }
                        } else if name == "await_promise" {
                            let arg = raw_args.first().copied().unwrap_or(0);
                            if let Some(id) = get_pointer_id(arg) {
                                let _ = caller.data_mut().get_response_body(id);
                            }
                            result_i64 = arg;
                        }

                        // Write result back to base_addr
                        let _ = memory.write(&mut caller, base, &result_i64.to_le_bytes());

                        0.0
                    },
                ),
            )
            .map_err(|_| ())?;

        // Define mem_call_i32(name_id: f64, arg_count: f64, base_addr: i32) -> i32
        linker
            .define(
                "rt",
                "mem_call_i32",
                Func::wrap(
                    &mut store,
                    |_caller: Caller<'_, RuntimeState>,
                     _name_id: f64,
                     _arg_count: f64,
                     _base_addr: i32|
                     -> i32 {
                        0
                    },
                ),
            )
            .map_err(|_| ())?;

        // Stubs for any remaining imported functions
        for import in module.imports() {
            if import.module() == "rt" {
                if let ExternType::Func(func_type) = import.ty() {
                    let results_types = func_type.results().to_vec();
                    let _ = linker.define(
                        import.module(),
                        import.name(),
                        Func::new(&mut store, func_type.clone(), move |_caller, _params, results| {
                            for (i, res_ty) in results_types.iter().enumerate() {
                                results[i] = match res_ty {
                                    ValType::I32 => Val::I32(0),
                                    ValType::I64 => Val::I64(0),
                                    ValType::F32 => Val::F32(0.0.into()),
                                    ValType::F64 => Val::F64(0.0.into()),
                                    _ => Val::I64(0),
                                };
                            }
                            Ok(())
                        }),
                    );
                }
            }
        }

        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|_| ())?;

        if let Some(memory) = instance.get_memory(&store, "memory") {
            store.data_mut().memory = Some(memory);
        }

        // Call _start to execute the Perry program
        if let Some(start_func) = instance.get_func(&store, "_start") {
            let _ = start_func.call(&mut store, &[], &mut []);
        }

        // Call run if exported
        if let Some(run_func) = instance.get_func(&store, "run") {
            if let Err(e) = run_func.call(&mut store, &[], &mut []) {
                print_stdout(&format!("run call error: {e:?}\n"));
                return Err(());
            }
        }

        Ok(())
    }
}

bindings::export!(Component with_types_in bindings);
