use crate::bindings::wasi::filesystem::preopens;
use crate::bindings::wasi::filesystem::types::{
    Descriptor, DescriptorFlags, DescriptorType, ErrorCode, OpenFlags, PathFlags,
};
use crate::bindings::wasi::io::streams::StreamError;
use crate::nanbox::TAG_UNDEFINED;
use crate::state::{get_state, JsHandle};

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
        format!(
            "Error: EACCES: permission denied, path '{path}' is not within any preopened directory"
        )
    })
}

fn format_error_code(code: ErrorCode, op: &str, path: &str) -> String {
    match code {
        ErrorCode::NoEntry => format!("Error: ENOENT: no such file or directory, {op} '{path}'"),
        ErrorCode::Access => format!("Error: EACCES: permission denied, {op} '{path}'"),
        ErrorCode::Exist => format!("Error: EEXIST: file already exists, {op} '{path}'"),
        ErrorCode::IsDirectory => {
            format!("Error: EISDIR: illegal operation on a directory, {op} '{path}'")
        }
        ErrorCode::NotDirectory => format!("Error: ENOTDIR: not a directory, {op} '{path}'"),
        ErrorCode::NotPermitted => format!("Error: EPERM: operation not permitted, {op} '{path}'"),
        ErrorCode::ReadOnly => format!("Error: EROFS: read-only file system, {op} '{path}'"),
        _ => format!("Error: EIO: filesystem error {code:?}, {op} '{path}'"),
    }
}

pub(crate) fn fs_read_file_sync(path_val: i64, options_val: i64) -> i64 {
    let state = get_state();
    let as_utf8 = match supported_read_options(&state.to_js_value(options_val)) {
        Ok(utf8) => utf8,
        Err(err) => {
            state.current_exception = Some(err.into());
            return TAG_UNDEFINED as i64;
        }
    };
    fs_read_file_impl(path_val, as_utf8)
}

pub(crate) fn fs_read_file_binary(path_val: i64) -> i64 {
    fs_read_file_impl(path_val, false)
}

fn supported_read_options(options: &serde_json::Value) -> Result<bool, &'static str> {
    let is_utf8 = |s: &str| s.eq_ignore_ascii_case("utf8") || s.eq_ignore_ascii_case("utf-8");
    let is_binary = |s: &str| s.eq_ignore_ascii_case("binary");

    match options {
        serde_json::Value::Null => Ok(false),
        serde_json::Value::String(s) => {
            if is_utf8(s) {
                Ok(true)
            } else if is_binary(s) {
                Ok(false)
            } else {
                Err("TypeError: Unsupported readFileSync options; only UTF-8 and binary encodings are supported")
            }
        }
        serde_json::Value::Object(fields) => {
            let mut as_utf8 = false;
            for (key, value) in fields {
                match key.as_str() {
                    "encoding" => {
                        if value.is_null() {
                            as_utf8 = false;
                        } else if let Some(s) = value.as_str() {
                            if is_utf8(s) {
                                as_utf8 = true;
                            } else if is_binary(s) {
                                as_utf8 = false;
                            } else {
                                return Err("TypeError: Unsupported readFileSync options; only UTF-8 and binary encodings are supported");
                            }
                        } else {
                            return Err("TypeError: Unsupported readFileSync options; only UTF-8 and binary encodings are supported");
                        }
                    }
                    "flag" => {
                        if value.as_str() != Some("r") {
                            return Err("TypeError: Unsupported readFileSync options; only flag 'r' is supported");
                        }
                    }
                    _ => return Err("TypeError: Unsupported readFileSync options"),
                }
            }
            Ok(as_utf8)
        }
        _ => Err("TypeError: Unsupported readFileSync options"),
    }
}

fn fs_read_file_impl(path_val: i64, as_utf8: bool) -> i64 {
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

    if as_utf8 {
        match core::str::from_utf8(&bytes) {
            Ok(text) => get_state().alloc_string(text),
            Err(_) => {
                get_state().current_exception =
                    Some(format!("Error: file '{path}' is not valid UTF-8"));
                0
            }
        }
    } else {
        let view = crate::buffer::Uint8ArrayView::from_bytes(bytes);
        let id = get_state().alloc_handle(crate::state::JsHandle::Uint8Array(view));
        crate::nanbox::nanbox_pointer(id)
    }
}

pub(crate) fn fs_write_file_sync(path_val: i64, content_val: i64, options: i64) -> i64 {
    let state = get_state();
    let byte_view = match state.get_handle(content_val) {
        Some(JsHandle::Uint8Array(view)) => Some(view),
        _ => None,
    };
    if !supported_write_options(&state.to_js_value(options), byte_view.is_some()) {
        state.current_exception = Some("TypeError: Unsupported writeFileSync options; only UTF-8 encoding and flag 'w' are supported".into());
        return TAG_UNDEFINED as i64;
    }
    let path = state.get_string(path_val);
    let bytes = if let Some(view) = byte_view {
        view.to_vec()
    } else {
        state.get_string(content_val).into_bytes()
    };

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
        get_state().current_exception =
            Some(format!("Error: EIO: i/o error while writing '{path}'"));
        return 0;
    }

    TAG_UNDEFINED as i64
}

/// The write implementation must reject options that change its semantics.
fn supported_write_options(options: &serde_json::Value, is_byte_view: bool) -> bool {
    let valid_encoding = |encoding: &serde_json::Value| {
        encoding.as_str().is_some_and(|e| {
            e.eq_ignore_ascii_case("utf8")
                || e.eq_ignore_ascii_case("utf-8")
                || (is_byte_view && e.eq_ignore_ascii_case("binary"))
        })
    };
    match options {
        serde_json::Value::Null => true,
        serde_json::Value::String(_) => valid_encoding(options),
        serde_json::Value::Object(fields) => fields.iter().all(|(key, value)| match key.as_str() {
            "encoding" => valid_encoding(value) || value.is_null(),
            "flag" => value.as_str() == Some("w"),
            _ => false,
        }),
        _ => false,
    }
}

pub(crate) fn fs_exists_sync(path_val: i64) -> i64 {
    let state = get_state();
    let path = state.get_string(path_val);

    let (dir, rel_path) = match locate_preopen(&path) {
        Ok(loc) => loc,
        Err(_) => return crate::nanbox::TAG_FALSE as i64,
    };

    let target_path = if rel_path.is_empty() {
        ".".to_string()
    } else {
        rel_path
    };

    let exists = dir.stat_at(PathFlags::SYMLINK_FOLLOW, &target_path).is_ok();
    drop(dir);
    if exists {
        crate::nanbox::TAG_TRUE as i64
    } else {
        crate::nanbox::TAG_FALSE as i64
    }
}

pub(crate) fn fs_unlink_sync(path_val: i64, options: i64) -> i64 {
    let state = get_state();
    if options as u64 != TAG_UNDEFINED {
        state.current_exception = Some("TypeError: unlinkSync does not accept options".into());
        return TAG_UNDEFINED as i64;
    }
    let path = state.get_string(path_val);

    let (dir, rel_path) = match locate_preopen(&path) {
        Ok(loc) => loc,
        Err(err) => {
            get_state().current_exception = Some(err);
            return 0;
        }
    };

    if rel_path.is_empty() {
        drop(dir);
        get_state().current_exception =
            Some(format_error_code(ErrorCode::NotPermitted, "unlink", &path));
        return 0;
    }

    match dir.unlink_file_at(&rel_path) {
        Ok(()) => {
            drop(dir);
            TAG_UNDEFINED as i64
        }
        Err(err) => {
            drop(dir);
            get_state().current_exception = Some(format_error_code(err, "unlink", &path));
            0
        }
    }
}

pub(crate) fn fs_mkdir_sync(path_val: i64, options: i64) -> i64 {
    let state = get_state();
    if options as u64 != TAG_UNDEFINED {
        state.current_exception = Some("TypeError: mkdirSync options are not supported".into());
        return TAG_UNDEFINED as i64;
    }
    let path = state.get_string(path_val);

    let (dir, rel_path) = match locate_preopen(&path) {
        Ok(loc) => loc,
        Err(err) => {
            get_state().current_exception = Some(err);
            return 0;
        }
    };

    if rel_path.is_empty() {
        drop(dir);
        get_state().current_exception = Some(format_error_code(ErrorCode::Exist, "mkdir", &path));
        return 0;
    }

    match dir.create_directory_at(&rel_path) {
        Ok(()) => {
            drop(dir);
            TAG_UNDEFINED as i64
        }
        Err(err) => {
            drop(dir);
            get_state().current_exception = Some(format_error_code(err, "mkdir", &path));
            0
        }
    }
}

pub(crate) fn fs_rmdir_sync(path_val: i64, options: i64) -> i64 {
    let state = get_state();
    if options as u64 != TAG_UNDEFINED {
        state.current_exception = Some("TypeError: rmdirSync options are not supported".into());
        return TAG_UNDEFINED as i64;
    }
    let path = state.get_string(path_val);

    let (dir, rel_path) = match locate_preopen(&path) {
        Ok(loc) => loc,
        Err(err) => {
            get_state().current_exception = Some(err);
            return 0;
        }
    };

    if rel_path.is_empty() {
        drop(dir);
        get_state().current_exception =
            Some(format_error_code(ErrorCode::NotPermitted, "rmdir", &path));
        return 0;
    }

    match dir.remove_directory_at(&rel_path) {
        Ok(()) => {
            drop(dir);
            TAG_UNDEFINED as i64
        }
        Err(err) => {
            drop(dir);
            get_state().current_exception = Some(format_error_code(err, "rmdir", &path));
            0
        }
    }
}

pub(crate) fn fs_readdir_sync(path_val: i64, options: i64) -> i64 {
    let state = get_state();
    let options_js = state.to_js_value(options);
    let supported = match &options_js {
        serde_json::Value::Null => true,
        serde_json::Value::String(s) => {
            s.eq_ignore_ascii_case("utf8") || s.eq_ignore_ascii_case("utf-8")
        }
        serde_json::Value::Object(map) => map.iter().all(|(k, v)| match k.as_str() {
            "encoding" => v
                .as_str()
                .map(|s| s.eq_ignore_ascii_case("utf8") || s.eq_ignore_ascii_case("utf-8"))
                .unwrap_or(v.is_null()),
            "recursive" => v.as_bool() == Some(false),
            "withFileTypes" => v.as_bool() == Some(false),
            _ => false,
        }),
        _ => false,
    };
    if !supported {
        state.current_exception = Some("TypeError: readdirSync options are not supported".into());
        return TAG_UNDEFINED as i64;
    }
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

    let dir_desc = match dir.open_at(
        PathFlags::SYMLINK_FOLLOW,
        &target_path,
        OpenFlags::DIRECTORY,
        DescriptorFlags::READ,
    ) {
        Ok(d) => d,
        Err(err) => {
            drop(dir);
            get_state().current_exception = Some(format_error_code(err, "readdir", &path));
            return 0;
        }
    };
    drop(dir);

    let stream = match dir_desc.read_directory() {
        Ok(s) => s,
        Err(err) => {
            drop(dir_desc);
            get_state().current_exception = Some(format_error_code(err, "readdir", &path));
            return 0;
        }
    };

    let mut entries = Vec::new();
    let mut read_err = None;
    loop {
        match stream.read_directory_entry() {
            Ok(Some(entry)) => {
                if entry.name != "." && entry.name != ".." {
                    let s_id = get_state().alloc_string(&entry.name);
                    entries.push(s_id);
                }
            }
            Ok(None) => break,
            Err(err) => {
                read_err = Some(format_error_code(err, "readdir", &path));
                break;
            }
        }
    }
    drop(stream);
    drop(dir_desc);

    if let Some(err_msg) = read_err {
        get_state().current_exception = Some(err_msg);
        return 0;
    }

    let arr_id = get_state().alloc_handle(crate::state::JsHandle::Array(entries));
    crate::nanbox::nanbox_pointer(arr_id)
}

pub(crate) fn fs_stat_sync(path_val: i64, options: i64) -> i64 {
    let state = get_state();
    let options_js = state.to_js_value(options);
    let supported = match &options_js {
        serde_json::Value::Null => true,
        serde_json::Value::Object(map) => map.iter().all(|(k, v)| match k.as_str() {
            "throwIfNoEntry" => v.as_bool() == Some(true),
            "bigint" => v.as_bool() == Some(false),
            _ => false,
        }),
        _ => false,
    };
    if !supported {
        state.current_exception = Some("TypeError: statSync options are not supported".into());
        return TAG_UNDEFINED as i64;
    }
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

    let stat = match dir.stat_at(PathFlags::SYMLINK_FOLLOW, &target_path) {
        Ok(s) => s,
        Err(err) => {
            drop(dir);
            get_state().current_exception = Some(format_error_code(err, "stat", &path));
            return 0;
        }
    };
    drop(dir);

    let is_file = matches!(stat.type_, DescriptorType::RegularFile);
    let is_dir = matches!(stat.type_, DescriptorType::Directory);
    let size = stat.size as f64;
    let mtime_ms = if let Some(dt) = stat.data_modification_timestamp {
        (dt.seconds as f64 * 1000.0) + (dt.nanoseconds as f64 / 1_000_000.0)
    } else {
        0.0
    };

    let mut map = serde_json::Map::new();
    map.insert("size".to_string(), serde_json::Value::from(size));
    map.insert("mtimeMs".to_string(), serde_json::Value::from(mtime_ms));
    map.insert("isFile".to_string(), serde_json::Value::Bool(is_file));
    map.insert("isDirectory".to_string(), serde_json::Value::Bool(is_dir));

    let obj_id =
        get_state().alloc_handle(crate::state::JsHandle::Json(serde_json::Value::Object(map)));
    crate::nanbox::nanbox_pointer(obj_id)
}
