#![no_std]
#![cfg(any(target_arch = "wasm32", test))]

#[cfg(not(target_arch = "wasm32"))]
extern crate std;

use perry_rt_sys as _;

mod url;
#[cfg(test)]
use url::normalize_request;

fn is_token(input: &[u8]) -> bool {
    !input.is_empty()
        && input
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(b))
}

fn header(input: &[u8]) -> Result<u32, ()> {
    if !is_token(input) {
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

fn status_text(input: &[u8]) -> Result<u32, ()> {
    let text = core::str::from_utf8(input).map_err(|_| ())?;
    if text
        .chars()
        .all(|ch| ch == '\t' || (' '..='~').contains(&ch) || ('\u{80}'..='\u{ff}').contains(&ch))
    {
        Ok(0)
    } else {
        Err(())
    }
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub(crate) extern "C" fn fetch_status_text(input: u32, length: u32) -> u32 {
    borrow(input, length, 0, 0, |input, _| status_text(input))
}

fn method(input: &[u8]) -> Result<u32, ()> {
    if !is_token(input)
        || [b"CONNECT".as_slice(), b"TRACE", b"TRACK"]
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

#[cfg(target_arch = "wasm32")]
use perry_rt_sys::guest_memory;

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
        let input = GuestRange::new(input, length).ok_or(())?;
        let output = GuestRange::new(output, capacity).ok_or(())?;
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
pub(crate) extern "C" fn fetch_url(input: u32, length: u32, output: u32, capacity: u32) -> u32 {
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
pub(crate) extern "C" fn fetch_url_value(
    input: u32,
    length: u32,
    output: u32,
    capacity: u32,
) -> u32 {
    borrow(input, length, output, capacity, |input, output| {
        Ok(url::normalize_url(input, output)?[5])
    })
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub(crate) extern "C" fn fetch_method(input: u32, length: u32) -> u32 {
    let result = (|| {
        let input = guest_memory::GuestRange::new(input, length).ok_or(())?;
        // SAFETY: validated initialized guest input, read without allocation or suspension.
        method(unsafe { input.bytes() })
    })();
    result.unwrap_or(u32::MAX)
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub(crate) extern "C" fn fetch_header_value(
    input: u32,
    length: u32,
    output: u32,
    capacity: u32,
) -> u32 {
    borrow(input, length, output, capacity, |input, output| {
        header_value(input, output).map(|length| length as u32)
    })
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub(crate) extern "C" fn fetch_header_name(
    input: u32,
    length: u32,
    output: u32,
    capacity: u32,
) -> u32 {
    borrow(input, length, output, capacity, |input, output| {
        header(input)?;
        let target = output.get_mut(..input.len()).ok_or(())?;
        for (source, target) in input.iter().zip(target) {
            *target = source.to_ascii_lowercase();
        }
        Ok(input.len() as u32)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn methods_and_header_bytes_follow_fetch_validation() {
        for valid in ["", "OK", "Créé\tOK", "\u{80}"] {
            assert_eq!(status_text(valid.as_bytes()), Ok(0));
        }
        for invalid in ["bad\rtext", "bad\ntext", "\0", "\u{7f}", "Ā"] {
            assert!(status_text(invalid.as_bytes()).is_err());
        }
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
        for invalid in ["🙂", "a\0b", "a\nb", "a\rb"] {
            assert!(header_value(invalid.as_bytes(), &mut output).is_err());
        }
    }
    #[test]
    fn request_url_coordinates_match_normalization() {
        for (input, expected) in [
            (
                "HTTP://Example.COM:80?q=hello world#ignored",
                "http://example.com/?q=hello%20world",
            ),
            ("https://[::1]:8443/é", "https://[::1]:8443/%C3%A9"),
            ("http://[0:0:0:0:0:0:0:1]:80/a", "http://[::1]/a"),
            ("http://[::ffff:192.0.2.1]/", "http://[::ffff:c000:201]/"),
            ("http://[1:0:0:2:0:0:3:4]/", "http://[1::2:0:0:3:4]/"),
            ("http://127.1/", "http://127.0.0.1/"),
            ("http://0x7f000001/", "http://127.0.0.1/"),
            ("http://0177.0.0.1/", "http://127.0.0.1/"),
            (
                "http://example.com/a/./b/../%2e/%2e%2E/c\\..\\d?k=\\v",
                "http://example.com/d?k=\\v",
            ),
            ("http://example.com/../../x", "http://example.com/x"),
            (
                "  http://exa\tmple.com/a\r/b\n?q=a b  ",
                "http://example.com/a/b?q=a%20b",
            ),
        ] {
            let mut output = [0; 512];
            let metadata = normalize_request(input.as_bytes(), &mut output).unwrap();
            assert_eq!(&output[..metadata[4] as usize], expected.as_bytes());
        }
        for input in [
            "file:///a",
            "http://",
            "http://host:99999",
            "http://host:x",
            "http://[wrong]/",
            "http://[::1",
            "http://[::1]tail",
            "http://[1::2::3]/",
            "http://user@host/",
            "http://host|name/",
            "http://a%2fb/",
            "http://a%00b/",
            "http://a%zzb/",
            "http://1.2.3.256/",
            "http://test.42/",
            "http://1.2.3.4.5/",
            "http://09/",
        ] {
            assert!(normalize_request(input.as_bytes(), &mut [0; 512]).is_err());
        }
        let mut output = [0; 512];
        let input = "HTTP://alice:p@ss@EXAMPLE.com:80/a^b?q=x#é\ny";
        let metadata = url::normalize_url(input.as_bytes(), &mut output).unwrap();
        assert_eq!(
            &output[..metadata[5] as usize],
            b"http://alice:p%40ss@example.com/a%5Eb?q=x#%C3%A9y"
        );
        assert!(normalize_request(input.as_bytes(), &mut output).is_err());
        let metadata = normalize_request(b"http://example.com/a#x\ny", &mut output).unwrap();
        assert_eq!(&output[..metadata[4] as usize], b"http://example.com/a");
        assert_eq!(&output[..metadata[5] as usize], b"http://example.com/a#xy");
    }
}
