//! Incremental UTF-8 decoding with bounded pending state and independent output strings.
use crate::runtime;
use core::ptr;

#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct Decoder {
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
        let mut rest = input;
        loop {
            // Between sequences, a valid run is emitted as one part; the byte loop below
            // handles pending sequences, invalid bytes and a split final sequence.
            if self.length == 0 {
                let valid = match core::str::from_utf8(rest) {
                    Ok(_) => rest.len(),
                    Err(error) => error.valid_up_to(),
                };
                if valid != 0 {
                    let mut run = &rest[..valid];
                    if !self.bom_seen {
                        self.bom_seen = true;
                        if self.ignore_bom == 0 && run.starts_with(&[0xef, 0xbb, 0xbf]) {
                            run = &run[3..];
                        }
                    }
                    if !run.is_empty() {
                        output(run);
                    }
                    rest = &rest[valid..];
                }
            }
            let Some((&byte, tail)) = rest.split_first() else {
                break;
            };
            rest = tail;
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
pub(crate) extern "C" fn std_decoder_new(fatal: u32, ignore_bom: u32) -> *mut Decoder {
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
pub(crate) unsafe extern "C" fn std_decoder_decode(
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
    fn bulk_runs_preserve_replacement_bom_and_chunk_boundaries() {
        for input in [
            &b"\xef\xbb\xbfabc\xc3\xa9\xf0\x9f\x99\x82xyz"[..],
            &b"abc\xffdef\xe0\x80\x80ghi\xf0\x9f"[..],
            &b"\xef\xbb\xbf\xef\xbb\xbf"[..],
            &b"abc\xed\xa0\x80def\xc3z"[..],
            &b""[..],
        ] {
            for ignore_bom in [0, 1] {
                for split in 0..=input.len() {
                    let mut decoder = Decoder {
                        fatal: 0,
                        ignore_bom,
                        streaming: false,
                        bom_seen: false,
                        pending: [0; 4],
                        length: 0,
                    };
                    let mut output = Vec::new();
                    decoder
                        .transcode(&input[..split], true, |part| output.extend_from_slice(part))
                        .unwrap();
                    decoder
                        .transcode(&input[split..], false, |part| {
                            output.extend_from_slice(part)
                        })
                        .unwrap();
                    let expected = String::from_utf8_lossy(input);
                    let expected = if ignore_bom == 0 {
                        expected.strip_prefix('\u{feff}').unwrap_or(&expected)
                    } else {
                        &expected
                    };
                    assert_eq!(
                        output,
                        expected.as_bytes(),
                        "split={split}, ignore_bom={ignore_bom}"
                    );
                }
            }
        }
    }
}
