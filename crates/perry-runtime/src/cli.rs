//! Synchronous CLI value imports. Canonical return storage and lifted values belong to
//! the current task after explicit transfer from the canonical import allocator.
use crate::{array, import_buffer, region::current, string};
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
    // SAFETY: imports initialize the return area; payload ownership is claimed before use.
    unsafe {
        let task = current();
        let mut vars = task.environment as *mut List<Variable>;
        if vars.is_null() {
            vars = task.allocate(size_of::<List<Variable>>()).cast();
            environment(vars);
            import_buffer::claim((*vars).data.cast());
            for index in 0..(*vars).len as usize {
                let value = &*(*vars).data.add(index);
                import_buffer::claim(value.key.data);
                import_buffer::claim(value.value.data);
            }
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
    // SAFETY: canonical import initializes the result before its buffers are claimed.
    unsafe {
        arguments(&mut result);
        import_buffer::claim(result.data.cast());
        for index in 0..result.len as usize {
            import_buffer::claim((*result.data.add(index)).data);
        }
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
