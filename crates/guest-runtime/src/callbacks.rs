//! Guest closures retain raw values; the linker supplies the TypeScript-table call.

use crate::nanbox::{nanbox_pointer, TAG_UNDEFINED};
use crate::state::{get_state, JsHandle};

#[derive(Clone, Debug)]
pub(crate) struct Closure {
    pub(crate) table_index: u32,
    pub(crate) captures: Vec<i64>,
}

pub(crate) fn dispatch_call(name: &str, args: &[i64]) -> Option<i64> {
    let args =
        if name == "closure_call_spread" && args.len() == 3 && args[0] == TAG_UNDEFINED as i64 {
            &args[1..]
        } else {
            args
        };
    let argument = |index| args.get(index).copied().unwrap_or(TAG_UNDEFINED as i64);
    Some(match name {
        "closure_new" => closure_new(argument(0), argument(1)),
        "closure_set_capture" => closure_set_capture(argument(0), argument(1), argument(2)),
        "closure_call_0" | "closure_call_1" | "closure_call_2" | "closure_call_3" => {
            invoke_with_arguments(argument(0), args.get(1..).unwrap_or_default())
        }
        "closure_call_spread" => guest_callback_invoke(argument(0), argument(1)),
        _ => return None,
    })
}

#[no_mangle]
pub(crate) extern "C" fn closure_call_0(handle: i64) -> i64 {
    invoke_with_arguments(handle, &[])
}

#[no_mangle]
pub(crate) extern "C" fn closure_call_1(handle: i64, a: i64) -> i64 {
    invoke_with_arguments(handle, &[a])
}

#[no_mangle]
pub(crate) extern "C" fn closure_call_2(handle: i64, a: i64, b: i64) -> i64 {
    invoke_with_arguments(handle, &[a, b])
}

#[no_mangle]
pub(crate) extern "C" fn closure_call_3(handle: i64, a: i64, b: i64, c: i64) -> i64 {
    invoke_with_arguments(handle, &[a, b, c])
}

#[no_mangle]
pub(crate) extern "C" fn closure_call_spread(handle: i64, arguments: i64) -> i64 {
    guest_callback_invoke(handle, arguments)
}

fn invoke_with_arguments(handle: i64, arguments: &[i64]) -> i64 {
    let arguments = get_state().alloc_handle(JsHandle::Array(arguments.to_vec()));
    guest_callback_invoke(handle, nanbox_pointer(arguments))
}

#[no_mangle]
pub(crate) extern "C" fn closure_new(table: i64, count: i64) -> i64 {
    let state = get_state();
    let Some(table_index) = state
        .element_index(table)
        .and_then(|index| u32::try_from(index).ok())
    else {
        return guest_callback_invalid();
    };
    let Some(count) = state.element_index(count) else {
        return guest_callback_invalid();
    };
    let mut captures = Vec::new();
    if captures.try_reserve_exact(count).is_err() {
        state.current_exception = Some("RangeError: Too many closure captures".into());
        return TAG_UNDEFINED as i64;
    }
    captures.resize(count, TAG_UNDEFINED as i64);
    let id = state.alloc_handle(JsHandle::Closure(Closure {
        table_index,
        captures,
    }));
    nanbox_pointer(id)
}

#[no_mangle]
pub(crate) extern "C" fn closure_set_capture(handle: i64, index: i64, value: i64) -> i64 {
    let state = get_state();
    let index = state.element_index(index);
    if let (Some(JsHandle::Closure(closure)), Some(index)) = (state.get_handle_mut(handle), index) {
        if let Some(capture) = closure.captures.get_mut(index) {
            *capture = value;
            return handle;
        }
    }
    guest_callback_invalid()
}

/// Replaced during linking with typed call_indirect instructions for table 0.
/// Both opaque arguments must remain observable so LTO preserves callers' values.
#[no_mangle]
#[inline(never)]
pub(crate) extern "C" fn guest_callback_invoke(handle: i64, arguments: i64) -> i64 {
    let (handle, arguments) = std::hint::black_box((handle, arguments));
    if std::hint::black_box(false) {
        handle ^ arguments
    } else {
        get_state().current_exception =
            Some("TypeError: Guest callback bridge is unavailable".into());
        TAG_UNDEFINED as i64
    }
}

#[no_mangle]
/// Returns a TypeScript table slot, -2 for builtin resolvers, or -1 for invalid values.
pub(crate) extern "C" fn guest_callback_table(handle: i64, arguments: i64) -> i32 {
    let state = get_state();
    match (state.get_handle(handle), state.get_handle(arguments)) {
        (Some(JsHandle::Closure(closure)), Some(JsHandle::Array(_))) => closure.table_index as i32,
        (Some(JsHandle::PromiseResolver(_)), Some(JsHandle::Array(_))) => -2,
        _ => -1,
    }
}

#[no_mangle]
pub(crate) extern "C" fn guest_callback_builtin(handle: i64, arguments: i64) -> i64 {
    crate::promises::invoke_resolver(handle, arguments)
}

#[no_mangle]
pub(crate) extern "C" fn guest_callback_argument(handle: i64, arguments: i64, index: i32) -> i64 {
    let state = get_state();
    let (Some(JsHandle::Closure(closure)), Some(JsHandle::Array(arguments))) =
        (state.get_handle(handle), state.get_handle(arguments))
    else {
        return TAG_UNDEFINED as i64;
    };
    let index = index as usize;
    closure
        .captures
        .get(index)
        .copied()
        .or_else(|| {
            arguments
                .get(index.checked_sub(closure.captures.len())?)
                .copied()
        })
        .unwrap_or(TAG_UNDEFINED as i64)
}

#[no_mangle]
pub(crate) extern "C" fn guest_callback_invalid() -> i64 {
    if get_state().current_exception.is_none() {
        get_state().current_exception =
            Some("TypeError: Value is not a callable guest function".into());
    }
    TAG_UNDEFINED as i64
}
