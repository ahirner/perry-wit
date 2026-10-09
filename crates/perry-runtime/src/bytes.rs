//! Fixed-size byte buffers and shared views, owned by the current task.
use crate::{UNDEFINED_NUMBER, exception, region::perry_alloc};
use core::ptr;

#[repr(C)]
struct Buffer {
    length: u32,
    data: *mut u8,
}
#[repr(C)]
struct View {
    buffer: *mut Buffer,
    offset: u32,
    length: u32,
}

fn index(value: f64) -> Option<u32> {
    let value = if value.is_nan() {
        0.0
    } else {
        libm::trunc(value)
    };
    (value >= 0.0 && value <= u32::MAX as f64).then_some(value as u32)
}

fn byte(value: f64) -> u8 {
    if !value.is_finite() {
        0
    } else {
        (libm::fmod(libm::trunc(value), 256.0) as i32) as u8
    }
}

fn relative(value: f64, length: u32) -> u32 {
    let value = if value.is_nan() {
        0.0
    } else {
        libm::trunc(value)
    };
    let value = if value < 0.0 {
        length as f64 + value
    } else {
        value
    };
    value.max(0.0).min(length as f64) as u32
}

fn range_error() -> i32 {
    exception::raise_error(exception::RANGE_ERROR, "byte buffer range is invalid")
}

fn view(buffer: *mut Buffer, offset: u32, length: u32) -> *mut u8 {
    let result = perry_alloc(size_of::<View>() as u32).cast::<View>();
    // SAFETY: freshly allocated; callers validate the range against the buffer.
    unsafe {
        result.write(View {
            buffer,
            offset,
            length,
        });
    }
    result.cast()
}

#[unsafe(no_mangle)]
pub extern "C" fn perry_buffer_new(length: f64) -> *mut u8 {
    let Some(length) = index(length) else {
        range_error();
        return ptr::null_mut();
    };
    let buffer = perry_alloc(size_of::<Buffer>() as u32).cast::<Buffer>();
    let data = perry_alloc(length.max(1));
    // SAFETY: both allocations are fresh and have the requested capacity.
    unsafe {
        data.write_bytes(0, length as usize);
        buffer.write(Buffer { length, data });
    }
    buffer.cast()
}

#[unsafe(no_mangle)]
pub extern "C" fn perry_bytes_new(length: f64) -> *mut u8 {
    let buffer = perry_buffer_new(length).cast::<Buffer>();
    if buffer.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: the buffer was successfully allocated above.
    view(buffer, 0, unsafe { (*buffer).length })
}

/// # Safety
/// `data` is writable storage for `length` bytes and remains live until task end.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_bytes_adopt(data: *mut u8, length: u32) -> *mut u8 {
    let data = if length == 0 { perry_alloc(1) } else { data };
    let buffer = perry_alloc(size_of::<Buffer>() as u32).cast::<Buffer>();
    // SAFETY: the descriptor is fresh; the caller provides the backing storage.
    unsafe {
        buffer.write(Buffer { length, data });
    }
    view(buffer, 0, length)
}

/// # Safety
/// `array` is a live Uint8Array view. The returned span remains live until task end.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_bytes_data(array: *mut u8) -> *mut u8 {
    // SAFETY: the view's range is within its live buffer.
    unsafe {
        let array = &*array.cast::<View>();
        (*array.buffer).data.add(array.offset as usize)
    }
}

/// # Safety
/// `buffer` is a live ArrayBuffer in this task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_bytes_view(buffer: *mut u8, offset: f64, length: f64) -> *mut u8 {
    let buffer = buffer.cast::<Buffer>();
    // SAFETY: guaranteed by the caller.
    let capacity = unsafe { (*buffer).length };
    let Some(offset) = index(offset).filter(|offset| *offset <= capacity) else {
        range_error();
        return ptr::null_mut();
    };
    let length = if length.to_bits() == UNDEFINED_NUMBER {
        Some(capacity - offset)
    } else {
        index(length)
    };
    let Some(length) = length.filter(|length| *length <= capacity - offset) else {
        range_error();
        return ptr::null_mut();
    };
    view(buffer, offset, length)
}

/// # Safety
/// `array` is a live Uint8Array view in this task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_bytes_length(array: *mut u8) -> f64 {
    // SAFETY: guaranteed by the caller.
    unsafe { (*array.cast::<View>()).length as f64 }
}

/// # Safety
/// `array` is a live Uint8Array view in this task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_bytes_offset(array: *mut u8) -> f64 {
    // SAFETY: guaranteed by the caller.
    unsafe { (*array.cast::<View>()).offset as f64 }
}

/// # Safety
/// `array` is a live Uint8Array view in this task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_bytes_buffer(array: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    unsafe { (*array.cast::<View>()).buffer.cast() }
}

/// # Safety
/// `buffer` is a live ArrayBuffer in this task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_buffer_length(buffer: *mut u8) -> f64 {
    // SAFETY: guaranteed by the caller.
    unsafe { (*buffer.cast::<Buffer>()).length as f64 }
}

/// # Safety
/// `array` is a live Uint8Array view in this task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_bytes_get(array: *mut u8, index: f64) -> f64 {
    // SAFETY: the view's range is valid; the element is read only after its bounds check.
    unsafe {
        let array = &*array.cast::<View>();
        if index >= 0.0 && index < array.length as f64 && libm::trunc(index) == index {
            *(*array.buffer)
                .data
                .add((array.offset + index as u32) as usize) as f64
        } else {
            f64::from_bits(UNDEFINED_NUMBER)
        }
    }
}

/// # Safety
/// `array` is a live Uint8Array view in this task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_bytes_put(array: *mut u8, index: f64, value: f64) -> f64 {
    // SAFETY: the view's range is valid; the element is written only after its bounds check.
    unsafe {
        let array = &*array.cast::<View>();
        if index >= 0.0 && index < array.length as f64 && libm::trunc(index) == index {
            (*array.buffer)
                .data
                .add((array.offset + index as u32) as usize)
                .write(byte(value));
        }
    }
    value
}

/// # Safety
/// `array` is a live Uint8Array view in this task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_bytes_subarray(array: *mut u8, begin: f64, end: f64) -> *mut u8 {
    // SAFETY: the view is live and relative indices clamp to its bounds.
    unsafe {
        let array = &*array.cast::<View>();
        let begin = relative(begin, array.length);
        let end = if end.to_bits() == UNDEFINED_NUMBER {
            array.length
        } else {
            relative(end, array.length)
        };
        view(
            array.buffer,
            array.offset + begin,
            end.saturating_sub(begin),
        )
    }
}

/// # Safety
/// `array` is a live Uint8Array view in this task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_bytes_copy(array: *mut u8) -> *mut u8 {
    // SAFETY: source and fresh destination have equal lengths and disjoint storage.
    unsafe {
        let source = &*array.cast::<View>();
        let result = perry_bytes_new(source.length as f64);
        let target = &*result.cast::<View>();
        ptr::copy_nonoverlapping(
            (*source.buffer).data.add(source.offset as usize),
            (*target.buffer).data,
            source.length as usize,
        );
        result
    }
}

/// # Safety
/// `array` is a live Uint8Array view in this task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_bytes_slice(array: *mut u8, begin: f64, end: f64) -> *mut u8 {
    // SAFETY: the temporary view and copied result belong to this task.
    unsafe { perry_bytes_copy(perry_bytes_subarray(array, begin, end)) }
}

/// # Safety
/// Both arguments are live Uint8Array views in this task; overlap is supported.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_bytes_set(target: *mut u8, source: *mut u8, offset: f64) -> i32 {
    // SAFETY: ranges are checked before writing; ptr::copy handles overlapping views.
    unsafe {
        let (target, source) = (&*target.cast::<View>(), &*source.cast::<View>());
        let Some(offset) = index(offset)
            .filter(|offset| *offset <= target.length && source.length <= target.length - offset)
        else {
            return range_error();
        };
        ptr::copy(
            (*source.buffer).data.add(source.offset as usize),
            (*target.buffer).data.add((target.offset + offset) as usize),
            source.length as usize,
        );
    }
    0
}

/// # Safety
/// `source` is a live dense array with f64 slots, and `target` a live Uint8Array view.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_bytes_set_array(
    target: *mut u8,
    source: *mut u8,
    offset: f64,
) -> i32 {
    // SAFETY: slots have the promised type; bounds are checked before writing.
    unsafe {
        let target = &*target.cast::<View>();
        let length = crate::json::view::perry_view_length(source);
        let Some(offset) = index(offset)
            .filter(|offset| *offset <= target.length && length <= target.length - offset)
        else {
            return range_error();
        };
        for position in 0..length {
            let bits = if crate::json::view::source(source).is_some() {
                let bits = crate::json::view::perry_view_get(source, position);
                let status = crate::exception::perry_exception_status();
                if status != 0 {
                    return status as i32;
                }
                bits
            } else {
                crate::array::slots(source)[position as usize]
            };
            (*target.buffer)
                .data
                .add((target.offset + offset) as usize + position as usize)
                .write(byte(f64::from_bits(bits)));
        }
    }
    0
}

/// # Safety
/// `array` is a live dense array with f64 slots in this task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_bytes_from_array(array: *mut u8) -> *mut u8 {
    // SAFETY: the destination has room for every source element.
    unsafe {
        let result = perry_bytes_new(crate::json::view::perry_view_length(array) as f64);
        perry_bytes_set_array(result, array, 0.0);
        result
    }
}
