#![cfg_attr(target_arch = "wasm32", no_std)]

/// Normalize the supported absolute HTTP URL shape into caller-owned storage.
/// Returns (secure, authority start/end, path start, total length).
fn normalize(input: &[u8], output: &mut [u8]) -> Result<[u32; 5], ()> {
    let input = input.split(|byte| *byte == b'#').next().ok_or(())?;
    let scheme_end = input.iter().position(|byte| *byte == b':').ok_or(())?;
    let secure = if input[..scheme_end].eq_ignore_ascii_case(b"https") {
        true
    } else if input[..scheme_end].eq_ignore_ascii_case(b"http") {
        false
    } else {
        return Err(());
    };
    if input.get(scheme_end..scheme_end + 3) != Some(b"://") {
        return Err(());
    }
    let rest = &input[scheme_end + 3..];
    let end = rest
        .iter()
        .position(|byte| matches!(byte, b'/' | b'?'))
        .unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty()
        || authority.iter().any(|byte| {
            !byte.is_ascii()
                || byte.is_ascii_control()
                || matches!(byte, b' ' | b'@' | b'\\' | b'%' | b'#')
        })
    {
        return Err(());
    }
    let port_start = if authority.starts_with(b"[") {
        let close = authority.iter().position(|byte| *byte == b']').ok_or(())?;
        core::str::from_utf8(&authority[1..close])
            .map_err(|_| ())?
            .parse::<core::net::Ipv6Addr>()
            .map_err(|_| ())?;
        if authority.len() > close + 1 && authority[close + 1] != b':' {
            return Err(());
        }
        (authority.len() > close + 1).then_some(close + 1)
    } else {
        if authority.iter().any(|byte| matches!(byte, b'[' | b']')) {
            return Err(());
        }
        authority.iter().position(|byte| *byte == b':')
    };
    let host_end = port_start.unwrap_or(authority.len());
    if host_end == 0 {
        return Err(());
    }
    let mut authority_end = authority.len();
    if let Some(port) = port_start {
        let digits = &authority[port + 1..];
        if !digits.iter().all(u8::is_ascii_digit) {
            return Err(());
        }
        let number = digits
            .iter()
            .try_fold(0u32, |value, digit| {
                value.checked_mul(10)?.checked_add(u32::from(digit - b'0'))
            })
            .ok_or(())?;
        if number > 65535 {
            return Err(());
        }
        if digits.is_empty() || number == if secure { 443 } else { 80 } {
            authority_end = port;
        }
    }
    let prefix: &[u8] = if secure { b"https://" } else { b"http://" };
    let required = input
        .len()
        .checked_mul(3)
        .and_then(|size| size.checked_add(1))
        .ok_or(())?;
    if output.len() < required {
        return Err(());
    }
    output[..prefix.len()].copy_from_slice(prefix);
    let mut cursor = prefix.len();
    for byte in &authority[..authority_end] {
        output[cursor] = byte.to_ascii_lowercase();
        cursor += 1;
    }
    let path_start = cursor;
    let path = &rest[end..];
    if !path.starts_with(b"/") {
        output[cursor] = b'/';
        cursor += 1;
    }
    for byte in path {
        if *byte == b'\\' || byte.is_ascii_control() {
            return Err(());
        }
        if *byte >= 127 || matches!(byte, b' ' | b'"' | b'<' | b'>' | b'`' | b'{' | b'}') {
            output[cursor] = b'%';
            output[cursor + 1] = b"0123456789ABCDEF"[(byte >> 4) as usize];
            output[cursor + 2] = b"0123456789ABCDEF"[(byte & 15) as usize];
            cursor += 3;
        } else {
            output[cursor] = *byte;
            cursor += 1;
        }
    }
    Ok([
        u32::from(secure),
        prefix.len() as u32,
        path_start as u32,
        path_start as u32,
        cursor as u32,
    ])
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
        let metadata = normalize(input, &mut output[32..])?;
        for (index, value) in metadata.iter().enumerate() {
            output[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        Ok(0)
    })
}

#[cfg(target_arch = "wasm32")]
#[unsafe(no_mangle)]
pub extern "C" fn fetch_decode(input: u32, length: u32, output: u32, capacity: u32) -> u32 {
    borrow(input, length, output, capacity, |input, output| {
        decode(input, output).map(|length| length as u32)
    })
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

#[cfg(test)]
mod tests {
    use super::*;
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
