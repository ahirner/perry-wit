//! Strict incremental UTF-8 codecs over task-owned strings and mutable byte views.
use crate::{bytes, exception, region::perry_alloc, string};
use core::ptr;

struct Decoder {
    ignore_bom: bool,
    continued: bool,
    bom_seen: bool,
    pending: *const u8,
    pending_len: usize,
}

#[unsafe(no_mangle)]
pub extern "C" fn perry_encoder_new() -> *mut u8 {
    perry_alloc(1)
}

/// # Safety
/// `source` is undefined or a live string. The result has independent mutable storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_text_encode(source: *mut u8) -> *mut u8 {
    // SAFETY: the source is immutable and the result is a fresh, disjoint allocation.
    unsafe {
        let text = if source.is_null() {
            ""
        } else {
            string::text(source)
        };
        let data = perry_alloc(text.len().max(1) as u32);
        ptr::copy_nonoverlapping(text.as_ptr(), data, text.len());
        bytes::perry_bytes_adopt(data, text.len() as u32)
    }
}

/// # Safety
/// `label` is undefined or a live string; flags are booleans.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_decoder_new(label: *mut u8, fatal: u32, ignore_bom: u32) -> *mut u8 {
    // SAFETY: the optional label borrows live immutable task storage.
    let label = if label.is_null() {
        "utf-8"
    } else {
        unsafe { string::text(label) }
    };
    let label = label.trim_matches(['\t', '\n', '\x0c', '\r', ' ']);
    if ![
        "utf-8",
        "utf8",
        "unicode-1-1-utf-8",
        "unicode11utf8",
        "unicode20utf8",
        "x-unicode20utf8",
    ]
    .iter()
    .any(|accepted| label.eq_ignore_ascii_case(accepted))
    {
        exception::raise_error(
            exception::RANGE_ERROR,
            "TextDecoder supports UTF-8 labels only",
        );
        return ptr::null_mut();
    }
    if fatal == 0 {
        exception::raise_error(
            exception::TYPE_ERROR,
            "replacement decoding is outside the UTF-8 scalar subset",
        );
        return ptr::null_mut();
    }
    let decoder = perry_alloc(size_of::<Decoder>() as u32).cast::<Decoder>();
    // SAFETY: fresh task allocation, aligned and large enough for Decoder.
    unsafe {
        decoder.write(Decoder {
            ignore_bom: ignore_bom != 0,
            continued: false,
            bom_seen: false,
            pending: ptr::null(),
            pending_len: 0,
        });
    }
    decoder.cast()
}

/// # Safety
/// `decoder` is live in the current task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_decoder_ignore_bom(decoder: *mut u8) -> u32 {
    // SAFETY: guaranteed by the caller.
    unsafe { u32::from((*decoder.cast::<Decoder>()).ignore_bom) }
}

/// # Safety
/// `decoder` is live; `input` is undefined or a live byte view; stream is a boolean.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_text_decode(
    decoder: *mut u8,
    input: *mut u8,
    stream: u32,
) -> *mut u8 {
    // SAFETY: reads only valid view ranges and copies input before retaining it or exposing text.
    unsafe {
        let decoder = &mut *decoder.cast::<Decoder>();
        if !decoder.continued {
            decoder.bom_seen = false;
            decoder.pending_len = 0;
        }
        decoder.continued = stream != 0;
        let length = if input.is_null() {
            0
        } else {
            bytes::perry_bytes_length(input) as usize
        };
        let Some(total) = decoder
            .pending_len
            .checked_add(length)
            .filter(|&size| size <= i32::MAX as usize)
        else {
            exception::raise_error(exception::RANGE_ERROR, "decoded text is too large");
            return ptr::null_mut();
        };
        let data = perry_alloc(total.max(1) as u32);
        if decoder.pending_len != 0 {
            ptr::copy_nonoverlapping(decoder.pending, data, decoder.pending_len);
        }
        if length != 0 {
            ptr::copy_nonoverlapping(
                bytes::perry_bytes_data(input),
                data.add(decoder.pending_len),
                length,
            );
        }
        let input = core::slice::from_raw_parts(data, total);
        let prefix = match core::str::from_utf8(input) {
            Ok(_) => total,
            Err(error) => {
                let prefix = error.valid_up_to();
                if let Some(consumed) = error.error_len() {
                    let remaining = prefix + consumed;
                    decoder.pending = data.add(remaining);
                    decoder.pending_len = total - remaining;
                    exception::raise_error(exception::TYPE_ERROR, "invalid UTF-8 input");
                    return ptr::null_mut();
                }
                if !decoder.continued {
                    decoder.pending_len = 0;
                    exception::raise_error(exception::TYPE_ERROR, "unfinished UTF-8 input");
                    return ptr::null_mut();
                }
                prefix
            }
        };
        decoder.pending = data.add(prefix);
        decoder.pending_len = total - prefix;
        let mut skip = 0;
        if prefix != 0 && !decoder.ignore_bom && !decoder.bom_seen {
            decoder.bom_seen = true;
            if input[..prefix].starts_with(&[0xef, 0xbb, 0xbf]) {
                skip = 3;
            }
        }
        string::perry_string_adopt(data.add(skip), (prefix - skip) as u32)
    }
}
