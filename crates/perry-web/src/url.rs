//! The declared HTTP(S) URL subset. A linked search-params value retains its URL owner.
use crate::{Buffer, TYPE_ERROR, alloc, bytes, from_static, helper};
use perry_abi::{adopt, raise};

#[repr(C)]
struct Url {
    href: *mut u8,
}

fn url(value: *mut u8) -> &'static mut Url {
    // SAFETY: generated code passes live URL owners, without an intervening collection.
    unsafe { &mut *value.cast::<Url>() }
}

fn text(value: *mut u8) -> &'static [u8] {
    // SAFETY: generated code and URL fields hold Perry strings.
    unsafe { bytes(value) }
}

fn view(bytes: &[u8]) -> *mut u8 {
    // SAFETY: slices refer to validated, immutable URL strings or owned UTF-8 buffers.
    unsafe { adopt(bytes.as_ptr().cast_mut(), bytes.len() as u32) }
}

fn normalize(input: &[u8]) -> Option<*mut u8> {
    let capacity = u32::try_from(input.len())
        .ok()?
        .checked_mul(4)?
        .checked_add(64)?;
    let output = alloc(capacity);
    // SAFETY: disjoint, initialized input and writable output; the helper never allocates.
    let length =
        unsafe { helper::fetch_url_value(input.as_ptr(), input.len() as u32, output, capacity) };
    (length != u32::MAX).then(|| unsafe { adopt(output, length) })
}

struct Parts {
    scheme: usize,
    path: usize,
    query: usize,
    hash: usize,
}

fn parts(href: &[u8]) -> Parts {
    let scheme = href
        .iter()
        .position(|b| *b == b':')
        .expect("normalized scheme");
    let path = scheme
        + 3
        + href[scheme + 3..]
            .iter()
            .position(|b| *b == b'/')
            .expect("normalized path");
    let hash = href.iter().position(|b| *b == b'#').unwrap_or(href.len());
    let query = href[..hash].iter().position(|b| *b == b'?').unwrap_or(hash);
    Parts {
        scheme,
        path,
        query,
        hash,
    }
}

fn resolve(input: &[u8], base: &[u8]) -> Option<*mut u8> {
    let base = normalize(base)?;
    let base = text(base);
    let p = parts(base);
    let input = input.trim_ascii();
    let colon = input.iter().position(|b| *b == b':');
    let delimiter = input
        .iter()
        .position(|b| b"/\\?#".contains(b))
        .unwrap_or(input.len());
    if colon.is_some_and(|at| at < delimiter) {
        return normalize(input);
    }
    let prefix = if input.starts_with(b"//") || input.starts_with(b"\\\\") {
        p.scheme + 1
    } else if input.starts_with(b"/") || input.starts_with(b"\\") {
        p.path
    } else if input.starts_with(b"?") {
        p.query
    } else if input.is_empty() || input.starts_with(b"#") {
        p.hash
    } else {
        base[..p.query].iter().rposition(|b| *b == b'/')? + 1
    };
    let mut joined = Buffer::new();
    joined.extend_from_slice(&base[..prefix]);
    if input.starts_with(b"\\\\") {
        joined.extend_from_slice(b"//");
        joined.extend_from_slice(&input[2..]);
    } else {
        joined.extend_from_slice(input);
    }
    normalize(joined.as_slice())
}

/// Construct a URL with an optional base string (null means no base).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_url_new(input: *mut u8, base: *mut u8) -> *mut u8 {
    let href = if base.is_null() {
        normalize(text(input))
    } else {
        resolve(text(input), text(base))
    };
    let Some(href) = href else {
        raise(TYPE_ERROR, "Invalid HTTP(S) URL");
        return core::ptr::null_mut();
    };
    let object = perry_abi::object!(Url; href);
    // SAFETY: fresh owner with one traced string field.
    unsafe {
        object.write(Url { href });
    }
    object.cast()
}

/// Read a declared URL field: href, protocol, pathname, username, password, hash,
/// search, host, hostname, port or origin.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_url_get(value: *mut u8, field: f64) -> *mut u8 {
    let href = url(value).href;
    if field == 0.0 {
        return href;
    }
    let href = text(href);
    let p = parts(href);
    let authority = &href[p.scheme + 3..p.path];
    let (credentials, host) = authority
        .iter()
        .rposition(|b| *b == b'@')
        .map_or((&b""[..], authority), |at| {
            (&authority[..at], &authority[at + 1..])
        });
    let user_end = credentials
        .iter()
        .position(|b| *b == b':')
        .unwrap_or(credentials.len());
    let port = if host.starts_with(b"[") {
        host.iter().position(|b| *b == b']').map(|at| at + 1)
    } else {
        host.iter().position(|b| *b == b':')
    }
    .unwrap_or(host.len());
    view(match field as u32 {
        1 => &href[..p.scheme + 1],
        2 => &href[p.path..p.query],
        3 => &credentials[..user_end],
        4 => credentials.get(user_end + 1..).unwrap_or_default(),
        5 => {
            if p.hash + 1 < href.len() {
                &href[p.hash..]
            } else {
                b""
            }
        }
        6 => {
            if p.query + 1 < p.hash {
                &href[p.query..p.hash]
            } else {
                b""
            }
        }
        7 => host,
        8 => &host[..port],
        9 => host.get(port + 1..).unwrap_or_default(),
        10 => {
            let mut origin = Buffer::new();
            origin.extend_from_slice(&href[..p.scheme + 3]);
            origin.extend_from_slice(host);
            return view(origin.as_slice());
        }
        _ => return from_static(""),
    })
}

/// Replace the pathname, preserving credentials, query and fragment.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_url_set_pathname(value: *mut u8, pathname: *mut u8) {
    let object = url(value);
    let href = text(object.href);
    let p = parts(href);
    let path = text(pathname);
    let mut joined = Buffer::new();
    joined.extend_from_slice(&href[..p.path]);
    if !path.starts_with(b"/") && !path.starts_with(b"\\") {
        joined.push(b'/');
    }
    for &byte in path {
        match byte {
            b'?' => joined.extend_from_slice(b"%3F"),
            b'#' => joined.extend_from_slice(b"%23"),
            _ => joined.push(byte),
        }
    }
    joined.extend_from_slice(&href[p.query..]);
    if let Some(href) = normalize(joined.as_slice()) {
        object.href = href;
    }
}

fn encode(out: &mut Buffer, input: &[u8], form: bool) {
    for &byte in input {
        if byte.is_ascii_alphanumeric()
            || b"*-._".contains(&byte)
            || (!form && b"!~'()".contains(&byte))
        {
            out.push(byte);
        } else if form && byte == b' ' {
            out.push(b'+');
        } else {
            out.push(b'%');
            out.push(b"0123456789ABCDEF"[(byte >> 4) as usize]);
            out.push(b"0123456789ABCDEF"[(byte & 15) as usize]);
        }
    }
}

fn decode_form(input: &[u8], output: &mut Buffer) {
    let hex = |b: u8| (b as char).to_digit(16).map(|v| v as u8);
    let mut decoded = Buffer::new();
    let mut at = 0;
    while at < input.len() {
        if input[at] == b'%'
            && let Some(pair) = input.get(at + 1..at + 3)
            && let (Some(high), Some(low)) = (hex(pair[0]), hex(pair[1]))
        {
            decoded.push(high * 16 + low);
            at += 3;
        } else {
            decoded.push(if input[at] == b'+' { b' ' } else { input[at] });
            at += 1;
        }
    }
    for chunk in decoded.as_slice().utf8_chunks() {
        encode(output, chunk.valid().as_bytes(), true);
        if !chunk.invalid().is_empty() {
            encode(output, "�".as_bytes(), true);
        }
    }
}

/// Append through a linked URLSearchParams view, applying form serialization to the query.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_url_append(value: *mut u8, name: *mut u8, item: *mut u8) {
    let object = url(value);
    let href = text(object.href);
    let p = parts(href);
    let mut joined = Buffer::new();
    joined.extend_from_slice(&href[..p.query]);
    joined.push(b'?');
    if p.query < p.hash {
        for pair in href[p.query + 1..p.hash]
            .split(|b| *b == b'&')
            .filter(|pair| !pair.is_empty())
        {
            let end = pair.iter().position(|b| *b == b'=').unwrap_or(pair.len());
            decode_form(&pair[..end], &mut joined);
            joined.push(b'=');
            decode_form(pair.get(end + 1..).unwrap_or_default(), &mut joined);
            joined.push(b'&');
        }
    }
    encode(&mut joined, text(name), true);
    joined.push(b'=');
    encode(&mut joined, text(item), true);
    joined.extend_from_slice(&href[p.hash..]);
    object.href = view(joined.as_slice());
}

/// Percent-encode Unicode scalar text with the encodeURIComponent character set.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_encode_uri_component(value: *mut u8) -> *mut u8 {
    let mut output = Buffer::new();
    encode(&mut output, text(value), false);
    view(output.as_slice())
}
