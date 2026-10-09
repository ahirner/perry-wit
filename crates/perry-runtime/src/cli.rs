//! Synchronous CLI value imports. Canonical return storage and lifted values belong to
//! the current task, including allocations made by the host through cabi_realloc.
use crate::{array, region::current, string};
use core::ptr;

#[repr(C)]
struct Text {
    data: *mut u8,
    len: u32,
}
#[repr(C)]
struct List<T> {
    data: *mut T,
    len: u32,
}
#[repr(C)]
struct Variable {
    key: Text,
    value: Text,
}

#[link(wasm_import_module = "wasi:cli/environment@0.3.0")]
unsafe extern "C" {
    #[link_name = "get-environment"]
    fn environment(result: *mut List<Variable>);
    #[link_name = "get-arguments"]
    fn arguments(result: *mut List<Text>);
}
#[link(wasm_import_module = "wasi:clocks/monotonic-clock@0.3.0")]
unsafe extern "C" {
    #[link_name = "now"]
    fn monotonic_now() -> u64;
}
#[link(wasm_import_module = "wasi:clocks/system-clock@0.3.0")]
unsafe extern "C" {
    #[link_name = "now"]
    fn wall_now(result: *mut Timestamp);
}
#[repr(C)]
struct Timestamp {
    seconds: i64,
    nanoseconds: u32,
}

fn variables() -> &'static [Variable] {
    // SAFETY: imports write their canonical return area and allocate payloads in this task.
    unsafe {
        let task = current();
        let mut vars = task.environment as *mut List<Variable>;
        if vars.is_null() {
            vars = task.allocate(size_of::<List<Variable>>()).cast();
            environment(vars);
            current().environment = vars.cast();
        }
        if (*vars).len == 0 {
            &[]
        } else {
            core::slice::from_raw_parts((*vars).data, (*vars).len as usize)
        }
    }
}

/// # Safety
/// `name` must be a live string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_env_get(name: *mut u8) -> *mut u8 {
    // SAFETY: name and imported text remain live throughout the current task.
    unsafe {
        let name = string::bytes(name);
        for var in variables() {
            let key = if var.key.len == 0 {
                &[]
            } else {
                core::slice::from_raw_parts(var.key.data, var.key.len as usize)
            };
            if name == key {
                return string::perry_string_adopt(var.value.data, var.value.len);
            }
        }
    }
    ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn perry_env_keys() -> *mut u8 {
    array::from_refs(variables().iter().map(|var| {
        // SAFETY: the host supplies valid UTF-8 in task-owned storage.
        unsafe { string::perry_string_adopt(var.key.data, var.key.len) }
    }))
}

#[unsafe(no_mangle)]
pub extern "C" fn perry_arguments() -> *mut u8 {
    let mut result = List {
        data: ptr::null_mut(),
        len: 0,
    };
    // SAFETY: canonical import writes result; host allocations belong to this task.
    unsafe {
        arguments(&mut result);
        let values = if result.len == 0 {
            &[]
        } else {
            core::slice::from_raw_parts(result.data, result.len as usize)
        };
        array::from_refs(
            values
                .iter()
                .map(|value| string::perry_string_adopt(value.data, value.len)),
        )
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn perry_monotonic_now() -> f64 {
    // SAFETY: synchronous clock import has no guest memory arguments.
    unsafe { monotonic_now() as f64 / 1_000_000.0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn perry_date_now() -> f64 {
    let mut result = Timestamp {
        seconds: 0,
        nanoseconds: 0,
    };
    // SAFETY: `result` has the canonical timestamp layout.
    unsafe { wall_now(&mut result) };
    (result.seconds as f64) * 1_000.0 + (result.nanoseconds / 1_000_000) as f64
}
