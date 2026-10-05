#![cfg_attr(target_arch = "wasm32", no_std)]

#[path = "fetch/url.rs"]
mod url;
use url::normalize;

fn header(input: &[u8]) -> Result<u32, ()> {
    if input.is_empty()
        || !input
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(b))
    {
        return Err(());
    }
    Ok(u32::from(input.eq_ignore_ascii_case(b"content-type")))
}

fn header_value(input: &[u8], output: &mut [u8]) -> Result<usize, ()> {
    let text = core::str::from_utf8(input)
        .map_err(|_| ())?
        .trim_matches(['\t', ' ', '\r', '\n']);
    let mut length = 0;
    for ch in text.chars() {
        if u32::from(ch) > 255 || matches!(ch, '\0' | '\r' | '\n') {
            return Err(());
        }
        *output.get_mut(length).ok_or(())? = ch as u8;
        length += 1;
    }
    Ok(length)
}

fn method(input: &[u8]) -> Result<u32, ()> {
    if input.is_empty()
        || !input
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(b))
    {
        return Err(());
    }
    if [b"CONNECT".as_slice(), b"TRACE", b"TRACK"]
        .iter()
        .any(|name| input.eq_ignore_ascii_case(name))
    {
        return Err(());
    }
    for (name, tag) in [
        (b"GET".as_slice(), 0),
        (b"HEAD", 1),
        (b"POST", 2),
        (b"PUT", 3),
        (b"DELETE", 4),
        (b"OPTIONS", 6),
    ] {
        if input.eq_ignore_ascii_case(name) {
            return Ok(tag);
        }
    }
    Ok(if input == b"PATCH" { 8 } else { 9 })
}

fn latin1(input: &[u8], output: &mut [u8]) -> Result<usize, ()> {
    let mut length = 0;
    for byte in input {
        if *byte < 128 {
            *output.get_mut(length).ok_or(())? = *byte;
            length += 1;
        } else {
            let target = output.get_mut(length..length + 2).ok_or(())?;
            target[0] = 0xc0 | (byte >> 6);
            target[1] = 0x80 | (byte & 63);
            length += 2;
        }
    }
    Ok(length)
}

fn decode(input: &[u8], output: &mut [u8]) -> Result<usize, ()> {
    let input = input.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(input);
    let mut length = 0;
    for chunk in input.utf8_chunks() {
        for part in [
            chunk.valid().as_bytes(),
            if chunk.invalid().is_empty() {
                b""
            } else {
                b"\xef\xbf\xbd"
            },
        ] {
            let end = length + part.len();
            output.get_mut(length..end).ok_or(())?.copy_from_slice(part);
            length = end;
        }
    }
    Ok(length)
}

#[cfg(target_arch = "wasm32")]
#[path = "guest_memory.rs"]
mod guest_memory;

#[cfg(target_arch = "wasm32")]
fn borrow(
    input: u32,
    length: u32,
    output: u32,
    capacity: u32,
    operation: impl FnOnce(&[u8], &mut [u8]) -> Result<u32, ()>,
) -> u32 {
    use guest_memory::GuestRange;
    let result = (|| {
        let input = GuestRange::new(input, length)?;
        let output = GuestRange::new(output, capacity)?;
        if input.overlaps(output) {
            return Err(());
        }
        // SAFETY: the compiler supplies initialized input and exclusive output;
        // checked ranges are disjoint and neither call allocates or suspends.
        operation(unsafe { input.bytes() }, unsafe { output.bytes_mut() })
    })();
    result.unwrap_or(u32::MAX)
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn fetch_url(input: u32, length: u32, output: u32, capacity: u32) -> u32 {
    borrow(input, length, output, capacity, |input, output| {
        if output.len() < 32 {
            return Err(());
        }
        let metadata = url::normalize_request(input, &mut output[32..])?;
        for (index, value) in metadata.iter().enumerate() {
            output[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        Ok(0)
    })
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn fetch_redirect(
    base: u32,
    base_len: u32,
    location: u32,
    location_len: u32,
    output: u32,
    capacity: u32,
) -> u32 {
    use guest_memory::GuestRange;
    let result = (|| {
        let base = GuestRange::new(base, base_len)?;
        let location = GuestRange::new(location, location_len)?;
        let output = GuestRange::new(output, capacity)?;
        if base.overlaps(output) || location.overlaps(output) {
            return Err(());
        }
        // SAFETY: validated immutable inputs are disjoint from caller-owned output;
        // the codec neither allocates nor suspends while these ranges are borrowed.
        let (base, location, output) =
            unsafe { (base.bytes(), location.bytes(), output.bytes_mut()) };
        if output.len() < 32 {
            return Err(());
        }
        let metadata = url::resolve(base, location, &mut output[32..])?;
        let origin_end = base
            .get(8..)
            .ok_or(())?
            .iter()
            .position(|byte| *byte == b'/')
            .ok_or(())?
            + 8;
        let same_origin = base.get(..origin_end) == output.get(32..32 + metadata[2] as usize);
        for (index, value) in metadata
            .into_iter()
            .chain([u32::from(same_origin)])
            .enumerate()
        {
            output[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        Ok(0)
    })();
    result.unwrap_or(u32::MAX)
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn fetch_decode(input: u32, length: u32, output: u32, capacity: u32) -> u32 {
    borrow(input, length, output, capacity, |input, output| {
        decode(input, output).map(|length| length as u32)
    })
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn fetch_method(input: u32, length: u32) -> u32 {
    let result = (|| {
        let input = guest_memory::GuestRange::new(input, length)?;
        // SAFETY: validated initialized guest input, read without allocation or suspension.
        method(unsafe { input.bytes() })
    })();
    result.unwrap_or(u32::MAX)
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn fetch_header(input: u32, length: u32) -> u32 {
    let result = (|| {
        let input = guest_memory::GuestRange::new(input, length)?;
        // SAFETY: validated initialized guest input, read without allocation or suspension.
        header(unsafe { input.bytes() })
    })();
    result.unwrap_or(u32::MAX)
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn fetch_header_value(input: u32, length: u32, output: u32, capacity: u32) -> u32 {
    borrow(input, length, output, capacity, |input, output| {
        header_value(input, output).map(|length| length as u32)
    })
}

fn joined_header_size<'a>(
    fields: impl Iterator<Item = (&'a [u8], &'a [u8])>,
    name: &[u8],
) -> Result<Option<usize>, ()> {
    header(name)?;
    let mut size: Option<usize> = None;
    for (_, value) in fields.filter(|(key, _)| key.eq_ignore_ascii_case(name)) {
        let length = value
            .iter()
            .try_fold(0usize, |length, byte| {
                length.checked_add(if *byte < 128 { 1 } else { 2 })
            })
            .ok_or(())?;
        size = Some(match size {
            None => length,
            Some(size) => size
                .checked_add(2)
                .and_then(|size| size.checked_add(length))
                .ok_or(())?,
        });
    }
    Ok(size)
}

fn join_header<'a>(
    fields: impl Iterator<Item = (&'a [u8], &'a [u8])>,
    name: &[u8],
    output: &mut [u8],
) -> Result<usize, ()> {
    let mut length = 0;
    let mut first = true;
    for (_, value) in fields.filter(|(key, _)| key.eq_ignore_ascii_case(name)) {
        if !first {
            output
                .get_mut(length..length + 2)
                .ok_or(())?
                .copy_from_slice(b", ");
            length += 2;
        }
        length += latin1(value, output.get_mut(length..).ok_or(())?)?;
        first = false;
    }
    Ok(length)
}

#[cfg(target_arch = "wasm32")]
struct HeaderInput {
    fields: guest_memory::GuestRange,
    name: guest_memory::GuestRange,
}

#[cfg(target_arch = "wasm32")]
impl HeaderInput {
    fn new(fields: u32, count: u32, name: u32, length: u32) -> Result<Self, ()> {
        let input = Self {
            fields: guest_memory::GuestRange::new(fields, count.checked_mul(16).ok_or(())?)?,
            name: guest_memory::GuestRange::new(name, length)?,
        };
        for ranges in input.ranges() {
            ranges?;
        }
        Ok(input)
    }

    fn ranges(&self) -> impl Iterator<Item = Result<[guest_memory::GuestRange; 2], ()>> {
        // SAFETY: the canonical field list is initialized, immutable, and validated by construction.
        unsafe { self.fields.bytes() }
            .chunks_exact(16)
            .map(|record| {
                let word = |at| u32::from_le_bytes(record[at..at + 4].try_into().unwrap());
                Ok([
                    guest_memory::GuestRange::new(word(0), word(4))?,
                    guest_memory::GuestRange::new(word(8), word(12))?,
                ])
            })
    }

    fn fields(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.ranges().map(|ranges| {
            let [name, value] = ranges.unwrap();
            // SAFETY: construction validated every initialized input; neither traversal allocates nor suspends.
            unsafe { (name.bytes(), value.bytes()) }
        })
    }

    fn name(&self) -> &[u8] {
        // SAFETY: construction validates this initialized immutable input.
        unsafe { self.name.bytes() }
    }

    fn validate_output(&self, output: guest_memory::GuestRange) -> Result<(), ()> {
        if output.overlaps(self.fields) || output.overlaps(self.name) {
            return Err(());
        }
        for pair in self.ranges() {
            if pair?.iter().any(|range| output.overlaps(*range)) {
                return Err(());
            }
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn fetch_header_size(fields: u32, count: u32, name: u32, length: u32) -> u32 {
    let result = (|| {
        let input = HeaderInput::new(fields, count, name, length)?;
        match joined_header_size(input.fields(), input.name())? {
            None => Ok(u32::MAX),
            Some(size) if size < (u32::MAX - 1) as usize => Ok(size as u32),
            _ => Err(()),
        }
    })();
    result.unwrap_or(u32::MAX - 1)
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn fetch_header_get(
    fields: u32,
    count: u32,
    name: u32,
    length: u32,
    output: u32,
    capacity: u32,
) -> u32 {
    let result = (|| {
        let input = HeaderInput::new(fields, count, name, length)?;
        let output = guest_memory::GuestRange::new(output, capacity)?;
        input.validate_output(output)?;
        // SAFETY: output is checked and disjoint from every input range, with exclusive caller ownership.
        join_header(input.fields(), input.name(), unsafe { output.bytes_mut() })
            .map(|size| size as u32)
    })();
    result.unwrap_or(u32::MAX)
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

fn edit_headers<'a>(
    fields: impl Iterator<Item = ([u32; 4], &'a [u8])>,
    name: &[u8],
    replacement: [u32; 4],
    action: u32,
    output: &mut [u8],
) -> Result<u32, ()> {
    header(name)?;
    if action > 2 {
        return Err(());
    }
    let mut count = 0u32;
    let mut replaced = false;
    let mut write = |field: [u32; 4]| -> Result<(), ()> {
        let offset = (count as usize).checked_mul(16).ok_or(())?;
        let target = output
            .get_mut(offset..offset.checked_add(16).ok_or(())?)
            .ok_or(())?;
        for (word, slot) in field
            .into_iter()
            .zip(target.as_chunks_mut::<4>().0.iter_mut())
        {
            slot.copy_from_slice(&word.to_le_bytes());
        }
        count = count.checked_add(1).ok_or(())?;
        Ok(())
    };
    for (field, key) in fields {
        if action != 0 && key.eq_ignore_ascii_case(name) {
            if action == 1 && !replaced {
                write(replacement)?;
                replaced = true;
            }
        } else {
            write(field)?;
        }
    }
    if action == 0 || (action == 1 && !replaced) {
        write(replacement)?;
    }
    Ok(count)
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn fetch_header_name(input: u32, length: u32, output: u32, capacity: u32) -> u32 {
    borrow(input, length, output, capacity, |input, output| {
        header(input)?;
        let target = output.get_mut(..input.len()).ok_or(())?;
        for (source, target) in input.iter().zip(target) {
            *target = source.to_ascii_lowercase();
        }
        Ok(input.len() as u32)
    })
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn fetch_header_edit(
    fields: u32,
    count: u32,
    name: u32,
    name_length: u32,
    value: u32,
    value_length: u32,
    action: u32,
    output: u32,
    capacity: u32,
) -> u32 {
    use guest_memory::GuestRange;
    (|| {
        let input = HeaderInput::new(fields, count, name, name_length)?;
        let value_range = GuestRange::new(value, value_length)?;
        let target = GuestRange::new(output, capacity)?;
        input.validate_output(target)?;
        if value_range.overlaps(target) {
            return Err(());
        }
        // SAFETY: the canonical list is initialized and immutable; target is disjoint.
        let fields = unsafe { input.fields.bytes() }
            .chunks_exact(16)
            .zip(input.fields())
            .map(|(record, (key, _))| {
                let word = |at| u32::from_le_bytes(record[at..at + 4].try_into().unwrap());
                ([word(0), word(4), word(8), word(12)], key)
            });
        // SAFETY: all input ranges were validated and output is exclusively borrowed and disjoint.
        edit_headers(
            fields,
            input.name(),
            [name, name_length, value, value_length],
            action,
            unsafe { target.bytes_mut() },
        )
    })()
    .unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redirect_codec_rejects_short_storage_and_non_http_targets() {
        let mut request = [0; 256];
        let parts =
            url::normalize_request(b"https://example.com/a#fragment", &mut request).unwrap();
        assert_eq!(&request[..parts[4] as usize], b"https://example.com/a");
        assert_eq!(
            &request[..parts[5] as usize],
            b"https://example.com/a#fragment"
        );
        let base = b"https://example.com/a/b";
        let mut output = [0; 512];
        assert!(url::resolve(base, b"file:///secret", &mut output).is_err());
        assert!(url::resolve(base, b"../c", &mut output[..8]).is_err());
        let metadata = url::resolve(base, b"../c", &mut output).unwrap();
        assert_eq!(&output[..metadata[4] as usize], b"https://example.com/c");
    }
    #[test]
    fn methods_and_header_bytes_follow_fetch_validation() {
        let fields = [
            ([1, 1, 2, 1], b"X".as_slice()),
            ([3, 1, 4, 1], b"x".as_slice()),
            ([5, 1, 6, 1], b"y".as_slice()),
        ];
        let mut output = [0u8; 64];
        assert_eq!(
            edit_headers(fields.into_iter(), b"x", [7, 1, 8, 1], 0, &mut output),
            Ok(4)
        );
        assert_eq!(
            edit_headers(fields.into_iter(), b"x", [7, 1, 8, 1], 1, &mut output),
            Ok(2)
        );
        assert_eq!(u32::from_le_bytes(output[..4].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(output[16..20].try_into().unwrap()), 5);
        assert_eq!(
            edit_headers(fields.into_iter(), b"X", [0; 4], 2, &mut output),
            Ok(1)
        );
        assert_eq!(u32::from_le_bytes(output[..4].try_into().unwrap()), 5);
        assert!(edit_headers(fields.into_iter(), b"X", [0; 4], 0, &mut output[..1]).is_err());
        assert_eq!(method(b"post"), Ok(2));
        assert_eq!(method(b"patch"), Ok(9));
        assert_eq!(method(b"PATCH"), Ok(8));
        for invalid in [
            b"trace".as_slice(),
            b"TRACK",
            b"connect",
            b"bad method",
            b"",
        ] {
            assert!(method(invalid).is_err());
        }
        assert_eq!(header(b"Content-Type"), Ok(1));
        assert!(header(b"bad header").is_err());
        let mut output = [0; 32];
        let length = header_value(" \tété\r\n".as_bytes(), &mut output).unwrap();
        assert_eq!(&output[..length], b"\xe9t\xe9");
        let length = latin1(b"\xe9t\xe9", &mut output).unwrap();
        assert_eq!(&output[..length], "été".as_bytes());
        let fields = [
            (b"X".as_slice(), b"\xe9".as_slice()),
            (b"x", b""),
            (b"empty", b""),
        ];
        assert_eq!(joined_header_size(fields.into_iter(), b"x"), Ok(Some(4)));
        assert_eq!(join_header(fields.into_iter(), b"X", &mut output), Ok(4));
        assert_eq!(&output[..4], "é, ".as_bytes());
        assert_eq!(
            joined_header_size(fields.into_iter(), b"empty"),
            Ok(Some(0))
        );
        assert_eq!(joined_header_size(fields.into_iter(), b"absent"), Ok(None));
        for invalid in ["🙂", "a\0b", "a\nb", "a\rb"] {
            assert!(header_value(invalid.as_bytes(), &mut output).is_err());
        }
    }
    #[test]
    fn url_and_utf8_use_one_runtime_path() {
        for (input, expected) in [
            (
                "HTTP://Example.COM:80?q=hello world#ignored",
                "http://example.com/?q=hello%20world",
            ),
            ("https://[::1]:8443/é", "https://[::1]:8443/%C3%A9"),
        ] {
            let mut output = [0; 512];
            let metadata = normalize(input.as_bytes(), &mut output).unwrap();
            assert_eq!(&output[..metadata[4] as usize], expected.as_bytes());
        }
        for input in [
            "file:///a",
            "http://",
            "http://host:99999",
            "http://[wrong]/",
            "http://user@host/",
        ] {
            assert!(normalize(input.as_bytes(), &mut [0; 512]).is_err());
        }
        let mut output = [0; 512];
        let length = decode(b"\xef\xbb\xbfhi\xf0\x9f\x99\x82\xe2\x82X\xff", &mut output).unwrap();
        assert_eq!(core::str::from_utf8(&output[..length]).unwrap(), "hi🙂�X�");
    }
}
