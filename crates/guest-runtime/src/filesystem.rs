use crate::bindings::wasi::filesystem::preopens;
use crate::bindings::wasi::filesystem::types::{
    Descriptor, DescriptorFlags, ErrorCode, OpenFlags, PathFlags,
};
use crate::bindings::wasi::io::streams::StreamError;
use crate::nanbox::TAG_UNDEFINED;
use crate::state::get_state;

/// Normalizes a path string, resolving '.' and '..' components and checking confinement.
/// Returns (normalized_path, is_absolute).
pub(crate) fn normalize_path(path: &str) -> Result<(String, bool), &'static str> {
    if path.contains('\0') {
        return Err("path contains NUL bytes");
    }
    let is_absolute = path.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();

    for seg in path.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            if segments.is_empty() {
                if is_absolute {
                    // In absolute paths, '/..' remains '/'
                    continue;
                } else {
                    return Err("path escapes preopen root");
                }
            }
            segments.pop();
        } else {
            segments.push(seg);
        }
    }

    let mut result = String::new();
    if is_absolute {
        result.push('/');
    }
    for (i, seg) in segments.iter().enumerate() {
        if i > 0 || is_absolute {
            if !result.ends_with('/') {
                result.push('/');
            }
        }
        result.push_str(seg);
    }
    if result.is_empty() {
        result.push(if is_absolute { '/' } else { '.' });
    }
    Ok((result, is_absolute))
}

/// Discovers matching preopen directory descriptor and relative path within that preopen.
pub(crate) fn locate_preopen(path: &str) -> Result<(Descriptor, String), String> {
    let (normalized, is_absolute) = normalize_path(path)
        .map_err(|e| format!("EACCES: permission denied, invalid path: {e}"))?;

    let directories = preopens::get_directories();
    if directories.is_empty() {
        return Err("EACCES: no preopened directories available".to_string());
    }

    let mut best_match: Option<(Descriptor, String)> = None;
    let mut best_score: isize = -1;

    for (descriptor, mount) in directories {
        let (norm_mount, mount_is_abs) = match normalize_path(&mount) {
            Ok(res) => res,
            Err(_) => continue,
        };

        // If path is absolute, prefer absolute preopens; if relative, prefer relative preopens or '/' root.
        let (matched, rel_path) = if is_absolute {
            if !mount_is_abs && norm_mount != "." {
                (false, String::new())
            } else if norm_mount == "/" {
                // Root preopen matches any absolute path
                let rel = normalized.strip_prefix('/').unwrap_or(&normalized);
                (true, rel.to_string())
            } else if normalized == norm_mount {
                (true, ".".to_string())
            } else {
                let prefix = format!("{}/", norm_mount.trim_end_matches('/'));
                if normalized.starts_with(&prefix) {
                    let rel = &normalized[prefix.len()..];
                    (true, rel.to_string())
                } else {
                    (false, String::new())
                }
            }
        } else {
            // Relative input path
            if norm_mount == "." || norm_mount == "" || norm_mount == "/" {
                (true, normalized.clone())
            } else if normalized.starts_with(&format!("{}/", norm_mount)) {
                let rel = &normalized[norm_mount.len() + 1..];
                (true, rel.to_string())
            } else {
                (false, String::new())
            }
        };

        if matched {
            let score = norm_mount.len() as isize;
            if score > best_score {
                best_score = score;
                best_match = Some((descriptor, rel_path));
            }
        }
    }

    best_match.ok_or_else(|| {
        format!("Error: EACCES: permission denied, path '{path}' is not within any preopened directory")
    })
}

fn format_error_code(code: ErrorCode, op: &str, path: &str) -> String {
    match code {
        ErrorCode::NoEntry => format!("Error: ENOENT: no such file or directory, {op} '{path}'"),
        ErrorCode::Access => format!("Error: EACCES: permission denied, {op} '{path}'"),
        ErrorCode::Exist => format!("Error: EEXIST: file already exists, {op} '{path}'"),
        ErrorCode::IsDirectory => format!("Error: EISDIR: illegal operation on a directory, {op} '{path}'"),
        ErrorCode::NotDirectory => format!("Error: ENOTDIR: not a directory, {op} '{path}'"),
        ErrorCode::NotPermitted => format!("Error: EPERM: operation not permitted, {op} '{path}'"),
        ErrorCode::ReadOnly => format!("Error: EROFS: read-only file system, {op} '{path}'"),
        _ => format!("Error: EIO: filesystem error {code:?}, {op} '{path}'"),
    }
}

pub(crate) fn fs_read_file_sync(path_val: i64) -> i64 {
    let state = get_state();
    let path = state.get_string(path_val);

    let (dir, rel_path) = match locate_preopen(&path) {
        Ok(loc) => loc,
        Err(err) => {
            get_state().current_exception = Some(err);
            return 0;
        }
    };

    let target_path = if rel_path.is_empty() {
        ".".to_string()
    } else {
        rel_path
    };

    let file_desc = match dir.open_at(
        PathFlags::SYMLINK_FOLLOW,
        &target_path,
        OpenFlags::empty(),
        DescriptorFlags::READ,
    ) {
        Ok(fd) => fd,
        Err(err) => {
            get_state().current_exception = Some(format_error_code(err, "open", &path));
            return 0;
        }
    };

    let stream = match file_desc.read_via_stream(0) {
        Ok(s) => s,
        Err(err) => {
            drop(file_desc);
            get_state().current_exception = Some(format_error_code(err, "read", &path));
            return 0;
        }
    };

    let mut bytes = Vec::new();
    loop {
        match stream.blocking_read(65536) {
            Ok(chunk) => {
                if chunk.is_empty() {
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            Err(StreamError::Closed) => break,
            Err(StreamError::LastOperationFailed(_)) => {
                drop(stream);
                drop(file_desc);
                get_state().current_exception =
                    Some(format!("EIO: i/o error while reading '{path}'"));
                return 0;
            }
        }
    }

    drop(stream);
    drop(file_desc);

    match core::str::from_utf8(&bytes) {
        Ok(text) => get_state().alloc_string(text),
        Err(_) => {
            get_state().current_exception =
                Some(format!("Error: file '{path}' is not valid UTF-8"));
            0
        }
    }
}

pub(crate) fn fs_write_file_sync(path_val: i64, content_val: i64) -> i64 {
    let state = get_state();
    let path = state.get_string(path_val);
    let content = state.get_string(content_val);

    let (dir, rel_path) = match locate_preopen(&path) {
        Ok(loc) => loc,
        Err(err) => {
            get_state().current_exception = Some(err);
            return 0;
        }
    };

    let target_path = if rel_path.is_empty() {
        ".".to_string()
    } else {
        rel_path
    };

    let open_flags = OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let desc_flags = DescriptorFlags::WRITE | DescriptorFlags::MUTATE_DIRECTORY;

    let file_desc = match dir.open_at(
        PathFlags::SYMLINK_FOLLOW,
        &target_path,
        open_flags,
        desc_flags,
    ) {
        Ok(fd) => fd,
        Err(err) => {
            get_state().current_exception = Some(format_error_code(err, "open", &path));
            return 0;
        }
    };

    let stream = match file_desc.write_via_stream(0) {
        Ok(s) => s,
        Err(err) => {
            drop(file_desc);
            get_state().current_exception = Some(format_error_code(err, "write", &path));
            return 0;
        }
    };

    let bytes = content.as_bytes();
    let mut write_err = false;
    if !bytes.is_empty() {
        for chunk in bytes.chunks(4096) {
            if stream.blocking_write_and_flush(chunk).is_err() {
                write_err = true;
                break;
            }
        }
    } else if stream.flush().is_err() {
        write_err = true;
    }

    drop(stream);
    drop(file_desc);

    if write_err {
        get_state().current_exception = Some(format!("Error: EIO: i/o error while writing '{path}'"));
        return 0;
    }

    TAG_UNDEFINED as i64
}
