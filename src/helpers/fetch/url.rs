//! Allocation-free HTTP URL normalization into the caller's guest-heap buffer.
use core::fmt::{self, Write};

struct Output<'a> {
    bytes: &'a mut [u8],
    length: usize,
}
impl Output<'_> {
    fn byte(&mut self, byte: u8) -> Result<(), ()> {
        *self.bytes.get_mut(self.length).ok_or(())? = byte;
        self.length += 1;
        Ok(())
    }
    fn encoded(&mut self, byte: u8, query: bool) -> Result<(), ()> {
        if byte <= 32
            || byte >= 127
            || matches!(byte, b'"' | b'<' | b'>')
            || (query && byte == b'\'')
            || (!query && matches!(byte, b'?' | b'^' | b'`' | b'{' | b'}'))
        {
            self.byte(b'%')?;
            self.byte(b"0123456789ABCDEF"[(byte >> 4) as usize])?;
            self.byte(b"0123456789ABCDEF"[(byte & 15) as usize])
        } else {
            self.byte(byte)
        }
    }
    fn ipv6(&mut self, pieces: [u16; 8]) -> Result<(), ()> {
        let mut longest = (0, 0);
        let mut at = 0;
        while at < 8 {
            let start = at;
            while at < 8 && pieces[at] == 0 {
                at += 1;
            }
            if at - start > longest.1 {
                longest = (start, at - start);
            }
            at += 1;
        }
        self.byte(b'[')?;
        let mut at = 0;
        while at < 8 {
            if longest.1 > 1 && at == longest.0 {
                self.write_str("::").map_err(|_| ())?;
                at += longest.1;
            } else {
                if at > 0 && !(longest.1 > 1 && at == longest.0 + longest.1) {
                    self.byte(b':')?;
                }
                write!(self, "{:x}", pieces[at]).map_err(|_| ())?;
                at += 1;
            }
        }
        self.byte(b']')
    }
}
impl Write for Output<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.length.checked_add(text.len()).ok_or(fmt::Error)?;
        self.bytes
            .get_mut(self.length..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}

fn hex(byte: u8) -> Result<u8, ()> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(()),
    }
}
fn ipv4_number(input: &[u8]) -> Result<u64, ()> {
    let (digits, radix) = if input.len() >= 2 && input[..2].eq_ignore_ascii_case(b"0x") {
        (&input[2..], 16)
    } else if input.len() > 1 && input[0] == b'0' {
        (&input[1..], 8)
    } else {
        (input, 10)
    };
    digits.iter().try_fold(0u64, |value, byte| {
        let digit = u64::from(hex(*byte)?);
        if digit >= radix {
            return Err(());
        }
        value
            .checked_mul(radix)
            .and_then(|value| value.checked_add(digit))
            .ok_or(())
    })
}
fn ipv4(input: &[u8]) -> Result<Option<u32>, ()> {
    let input = input.strip_suffix(b".").unwrap_or(input);
    let last = input.rsplit(|byte| *byte == b'.').next().ok_or(())?;
    if last.is_empty() || (!last.iter().all(u8::is_ascii_digit) && ipv4_number(last).is_err()) {
        return Ok(None);
    }
    let mut parts = [0u64; 4];
    let mut count = 0;
    for part in input.split(|byte| *byte == b'.') {
        if part.is_empty() || count == 4 {
            return Err(());
        }
        parts[count] = ipv4_number(part)?;
        count += 1;
    }
    if parts[..count - 1].iter().any(|part| *part > 255)
        || parts[count - 1] >= (1u64 << (8 * (5 - count)))
    {
        return Err(());
    }
    let mut result = parts[count - 1];
    for (index, part) in parts[..count - 1].iter().enumerate() {
        result += part << (8 * (3 - index));
    }
    Ok(Some(result as u32))
}

fn dot(segment: &[u8]) -> usize {
    if segment == b"." || segment.eq_ignore_ascii_case(b"%2e") {
        1
    } else if segment == b".."
        || segment.eq_ignore_ascii_case(b".%2e")
        || segment.eq_ignore_ascii_case(b"%2e.")
        || segment.eq_ignore_ascii_case(b"%2e%2e")
    {
        2
    } else {
        0
    }
}

/// Returns secure, authority start/end, path start, and total byte length.
/// Input storage is immutable and disjoint from output; no allocation occurs.
pub(super) fn normalize(input: &[u8], output: &mut [u8]) -> Result<[u32; 5], ()> {
    let start = input.iter().position(|byte| *byte > 32).ok_or(())?;
    let end = input.iter().rposition(|byte| *byte > 32).ok_or(())? + 1;
    let input = &input[start..end];
    if input
        .iter()
        .any(|byte| matches!(byte, b'\t' | b'\r' | b'\n'))
    {
        let reserve = input
            .len()
            .checked_mul(3)
            .and_then(|length| length.checked_add(1))
            .ok_or(())?;
        if reserve > output.len() {
            return Err(());
        }
        let (target, scratch) = output.split_at_mut(reserve);
        let mut length = 0;
        for byte in input
            .iter()
            .filter(|byte| !matches!(byte, b'\t' | b'\r' | b'\n'))
        {
            *scratch.get_mut(length).ok_or(())? = *byte;
            length += 1;
        }
        canonicalize(&scratch[..length], target)
    } else {
        canonicalize(input, output)
    }
}

fn canonicalize(input: &[u8], output: &mut [u8]) -> Result<[u32; 5], ()> {
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
        .position(|byte| matches!(byte, b'/' | b'?' | b'\\'))
        .unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() {
        return Err(());
    }
    let (host, port) = if authority.starts_with(b"[") {
        let close = authority.iter().position(|byte| *byte == b']').ok_or(())?;
        let tail = &authority[close + 1..];
        (
            &authority[..close + 1],
            if tail.is_empty() {
                None
            } else {
                Some(tail.strip_prefix(b":").ok_or(())?)
            },
        )
    } else if let Some(at) = authority.iter().position(|byte| *byte == b':') {
        (&authority[..at], Some(&authority[at + 1..]))
    } else {
        (authority, None)
    };
    if host.is_empty() {
        return Err(());
    }
    let mut out = Output {
        bytes: output,
        length: 0,
    };
    out.write_str(if secure { "https://" } else { "http://" })
        .map_err(|_| ())?;
    let authority_start = out.length;
    if host.starts_with(b"[") {
        let address = core::str::from_utf8(&host[1..host.len() - 1])
            .map_err(|_| ())?
            .parse::<core::net::Ipv6Addr>()
            .map_err(|_| ())?;
        out.ipv6(address.segments())?;
    } else {
        let mut at = 0;
        while at < host.len() {
            let byte = if host[at] == b'%' {
                let pair = host.get(at + 1..at + 3).ok_or(())?;
                at += 2;
                hex(pair[0])? * 16 + hex(pair[1])?
            } else {
                host[at]
            };
            if !byte.is_ascii()
                || byte <= 32
                || byte == 127
                || b"#/:<>?@[\\]^|".contains(&byte)
                || byte == b'%'
            {
                return Err(());
            }
            out.byte(byte.to_ascii_lowercase())?;
            at += 1;
        }
        if let Some(address) = ipv4(&out.bytes[authority_start..out.length])? {
            out.length = authority_start;
            write!(
                out,
                "{}.{}.{}.{}",
                address >> 24,
                (address >> 16) & 255,
                (address >> 8) & 255,
                address & 255
            )
            .map_err(|_| ())?;
        }
    }
    if let Some(digits) = port
        && !digits.is_empty()
    {
        let number = digits
            .iter()
            .try_fold(0u32, |value, byte| {
                if !byte.is_ascii_digit() {
                    return None;
                }
                value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))
            })
            .ok_or(())?;
        if number > 65535 {
            return Err(());
        }
        if number != if secure { 443 } else { 80 } {
            write!(out, ":{number}").map_err(|_| ())?;
        }
    }
    let path_start = out.length;
    let path_query = &rest[end..];
    let query = path_query.iter().position(|byte| *byte == b'?');
    let path = &path_query[..query.unwrap_or(path_query.len())];
    out.byte(b'/')?;
    let path = if path.is_empty() { path } else { &path[1..] };
    let mut segments = path.split(|byte| matches!(byte, b'/' | b'\\')).peekable();
    while let Some(segment) = segments.next() {
        match dot(segment) {
            1 => {}
            2 => {
                let tail = &out.bytes[path_start..out.length.saturating_sub(1).max(path_start)];
                out.length = path_start
                    + tail
                        .iter()
                        .rposition(|byte| *byte == b'/')
                        .map_or(1, |index| index + 1);
            }
            _ => {
                for byte in segment {
                    out.encoded(*byte, false)?;
                }
                if segments.peek().is_some() {
                    out.byte(b'/')?;
                }
            }
        }
    }
    if let Some(query) = query {
        out.byte(b'?')?;
        for byte in &path_query[query + 1..] {
            out.encoded(*byte, true)?;
        }
    }
    Ok([
        u32::from(secure),
        authority_start as u32,
        path_start as u32,
        path_start as u32,
        out.length as u32,
    ])
}
