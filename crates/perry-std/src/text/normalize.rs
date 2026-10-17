//! Unicode normalization (`NFC`, `NFD`, `NFKC`, `NFKD`) for `String.prototype.normalize`.
use super::{from_static, view};

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "__perry_helper")]
unsafe extern "C" {
    fn str_normalize(src_ptr: u32, src_byte_len: u32, out_ptr: u32, mode: u32) -> u64;
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn str_normalize(_src_ptr: u32, _src_byte_len: u32, _out_ptr: u32, _mode: u32) -> u64 {
    0
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_normalize(string: *mut u8, form: *mut u8) -> *mut u8 {
    let mode: u32 = if form.is_null() {
        1
    } else {
        match unsafe { crate::runtime::bytes(form) } {
            [b'N', b'F', b'C'] => 1,
            [b'N', b'F', b'D'] => 0,
            [b'N', b'F', b'K', b'C'] => 3,
            [b'N', b'F', b'K', b'D'] => 2,
            _ => {
                crate::runtime::raise(crate::runtime::RANGE_ERROR, "");
                return from_static("");
            }
        }
    };
    let v = view(string);
    if v.ascii {
        return string;
    }
    let src_ptr = v.text.as_ptr() as usize as u32;
    let src_len = v.text.len() as u32;
    let measured = unsafe { str_normalize(src_ptr, src_len, 0, mode) };
    if measured == 0 {
        return string;
    }
    if measured == u64::MAX {
        return from_static("");
    }
    let byte_len = measured as u32;
    let scalar_len = (measured >> 32) as u32;
    let data = crate::runtime::alloc(byte_len);
    unsafe {
        str_normalize(src_ptr, src_len, data as usize as u32, mode);
        crate::runtime::adopt_counted(data, byte_len, scalar_len)
    }
}
