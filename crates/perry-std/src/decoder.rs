//! Incremental UTF-8 decoding with bounded pending state and independent output strings.
use crate::runtime;
use core::ptr;

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Decoder {
    fatal: u32,
    ignore_bom: u32,
    streaming: bool,
    bom_seen: bool,
    pending: [u8; 4],
    length: u8,
}

impl Decoder {
    fn emit(&mut self, bytes: &[u8], output: &mut impl FnMut(&[u8])) {
        let first = !self.bom_seen;
        self.bom_seen = true;
        if !first || self.ignore_bom != 0 || bytes != [0xef, 0xbb, 0xbf] {
            output(bytes);
        }
    }

    fn replacement(&mut self, output: &mut impl FnMut(&[u8])) -> Result<(), ()> {
        self.length = 0;
        if self.fatal != 0 {
            return Err(());
        }
        self.emit(&[0xef, 0xbf, 0xbd], output);
        Ok(())
    }

    fn transcode(
        &mut self,
        input: &[u8],
        stream: bool,
        mut output: impl FnMut(&[u8]),
    ) -> Result<(), ()> {
        if !self.streaming {
            self.length = 0;
            self.bom_seen = false;
        }
        self.streaming = stream;
        for &byte in input {
            if self.length != 0 {
                let range = match (self.length, self.pending[0]) {
                    (1, 0xe0) => 0xa0..=0xbf,
                    (1, 0xed) => 0x80..=0x9f,
                    (1, 0xf0) => 0x90..=0xbf,
                    (1, 0xf4) => 0x80..=0x8f,
                    _ => 0x80..=0xbf,
                };
                if range.contains(&byte) {
                    self.pending[self.length as usize] = byte;
                    self.length += 1;
                    let expected = if self.pending[0] < 0xe0 {
                        2
                    } else if self.pending[0] < 0xf0 {
                        3
                    } else {
                        4
                    };
                    if self.length == expected {
                        let sequence = self.pending;
                        self.length = 0;
                        self.emit(&sequence[..expected as usize], &mut output);
                    }
                    continue;
                }
                self.replacement(&mut output)?;
            }
            match byte {
                0..=0x7f => self.emit(&[byte], &mut output),
                0xc2..=0xf4 => {
                    self.pending[0] = byte;
                    self.length = 1;
                }
                _ => self.replacement(&mut output)?,
            }
        }
        if !stream && self.length != 0 {
            self.replacement(&mut output)?;
        }
        Ok(())
    }
}

/// Allocate only the decoder's bounded scalar state; no input buffer is retained.
#[unsafe(no_mangle)]
pub extern "C" fn std_decoder_new(fatal: u32, ignore_bom: u32) -> *mut Decoder {
    let decoder = runtime::alloc(core::mem::size_of::<Decoder>() as u32).cast::<Decoder>();
    // SAFETY: fresh, aligned storage with no pointer fields.
    unsafe {
        decoder.write(Decoder {
            fatal,
            ignore_bom,
            streaming: false,
            bom_seen: false,
            pending: [0; 4],
            length: 0,
        });
    }
    decoder
}

/// # Safety
/// `decoder` is a live, exclusively accessed decoder; `bytes` is a live byte view.
/// No collection occurs while this helper holds temporary output storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn std_decoder_decode(
    decoder: *mut Decoder,
    bytes: *const runtime::Bytes,
    stream: u32,
) -> *mut u8 {
    let initial = unsafe { *decoder };
    let bytes = unsafe { &*bytes };
    let input = if bytes.len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(bytes.data, bytes.len as usize) }
    };
    let mut measured = initial;
    let mut length = 0usize;
    let result = measured.transcode(input, stream != 0, |part| length += part.len());
    unsafe {
        *decoder = measured;
    }
    if result.is_err() {
        runtime::raise(
            runtime::TYPE_ERROR,
            "The encoded data was not valid for encoding utf-8",
        );
        return runtime::from_static("");
    }
    let Ok(length) = u32::try_from(length) else {
        runtime::raise(runtime::RANGE_ERROR, "Decoded string is too large");
        return runtime::from_static("");
    };
    let data = runtime::alloc(length);
    let mut position = 0;
    let mut writing = initial;
    let _ = writing.transcode(input, stream != 0, |part| {
        // SAFETY: the identical measuring pass established this output capacity.
        unsafe {
            ptr::copy_nonoverlapping(part.as_ptr(), data.add(position), part.len());
        }
        position += part.len();
    });
    // SAFETY: the transducer emits complete valid Unicode scalars only.
    unsafe { runtime::adopt(data, length) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_split_preserves_bom_scalar_and_invalid_subpart_state() {
        for input in [
            b"\xef\xbb\xbfhello\xf0\x9f\x99\x82".as_slice(),
            b"\xe0\x80x\xed\xa0\x80\xf4\x90\x80\x80\xe2\x82",
        ] {
            let expected =
                String::from_utf8_lossy(input.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(input));
            for split in 0..=input.len() {
                let mut decoder = Decoder {
                    fatal: 0,
                    ignore_bom: 0,
                    streaming: false,
                    bom_seen: false,
                    pending: [0; 4],
                    length: 0,
                };
                let mut output = Vec::new();
                decoder
                    .transcode(&input[..split], true, |b| output.extend_from_slice(b))
                    .unwrap();
                decoder
                    .transcode(&input[split..], false, |b| output.extend_from_slice(b))
                    .unwrap();
                assert_eq!(
                    String::from_utf8(output).unwrap(),
                    expected,
                    "split={split}"
                );
            }
        }
    }
}
