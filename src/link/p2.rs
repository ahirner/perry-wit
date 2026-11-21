//! Bind Perry's internal asynchronous runtime protocol to native P2 helpers.
//! The resulting core module imports only the P2 ABI; no P3 component is nested or adapted.
use perry_contract::target::Target;
use perry_wit_diagnostics::{Result, codes, ensure_diag};

fn helper(module: &str, name: &str) -> Option<&'static str> {
    if module == "$root" {
        return Some(match name {
            "[context-get-0]" => "p2_context_get",
            "[context-set-0]" => "p2_context_set",
            "[waitable-set-new]" => "p2_set_new",
            "[waitable-set-drop]" => "p2_set_drop",
            "[waitable-join]" => "p2_join",
            "[subtask-drop]" => "p2_drop",
            "[subtask-cancel]" => "p2_subtask_cancel",
            _ => return None,
        });
    }
    if module == "[export]$root" {
        if name == "[task-cancel]" {
            return Some("p2_task_cancel");
        }
        if name.ends_with("perry-internal-promise") {
            return future(name);
        }
    }
    if module == "wasi:clocks/monotonic-clock@0.3.0" && name == "[async-lower]wait-for" {
        return Some("p2_wait_for");
    }
    if matches!(module, "wasi:cli/stdout@0.3.0" | "wasi:cli/stderr@0.3.0") {
        return Some(match name {
            "[stream-new-0]write-via-stream" => "p2_io_new",
            "write-via-stream" if module.contains("stdout") => "p2_io_stdout",
            "write-via-stream" => "p2_io_stderr",
            "[async-lower][stream-write-0]write-via-stream" => "p2_io_write",
            "[stream-drop-writable-0]write-via-stream" => "p2_io_drop_writer",
            "[stream-cancel-write-0]write-via-stream" => "p2_io_cancel_write",
            _ => return future(name),
        });
    }
    if module == "wasi:http/client@0.3.0" && name == "[async-lower]send" {
        return Some("p2_http_send");
    }
    if module == "perry:canon" {
        return Some(match name {
            "[async-lower][stream-read]u8" => "p2_http_read",
            "[stream-drop-readable]u8" => "p2_http_drop_readable",
            "[stream-cancel-read]u8" => "p2_cancel",
            _ => return None,
        });
    }
    if module == "wasi:http/types@0.3.0" {
        return Some(match name {
            "[constructor]fields" => "p2_http_fields_new",
            "[method]fields.append" => "p2_http_fields_append",
            "[method]fields.copy-all" => "p2_http_fields_entries",
            "[resource-drop]fields" => "p2_http_fields_drop",
            "[static]request.new" => "p2_http_request_new",
            "[method]request.set-method" => "p2_http_set_method",
            "[method]request.set-scheme" => "p2_http_set_scheme",
            "[method]request.set-authority" => "p2_http_set_authority",
            "[method]request.set-path-with-query" => "p2_http_set_path",
            "[method]response.get-status-code" => "p2_http_status",
            "[method]response.get-headers" => "p2_http_headers",
            "[static]response.consume-body" => "p2_http_consume",
            "[stream-new-0][static]request.new" => "p2_http_contents_new",
            "[async-lower][stream-write-0][static]request.new" => "p2_http_contents_write",
            "[stream-drop-writable-0][static]request.new" => "p2_http_contents_drop",
            "[stream-cancel-write-0][static]request.new" => "p2_cancel",
            name if name.ends_with("request.new") || name.ends_with("response.consume-body") => {
                return future(name);
            }
            _ => return None,
        });
    }
    if module == "wasi:filesystem/types@0.3.0" {
        return Some(match name {
            "[async-lower][method]descriptor.open-at" => "p2_fs_open",
            "[async-lower][method]descriptor.stat-at" => "p2_fs_stat",
            "[async-lower][method]descriptor.create-directory-at" => "p2_fs_mkdir",
            "[async-lower][method]descriptor.remove-directory-at" => "p2_fs_rmdir",
            "[async-lower][method]descriptor.unlink-file-at" => "p2_fs_unlink",
            "[method]descriptor.read-via-stream" => "p2_fs_read_stream",
            "[method]descriptor.write-via-stream" => "p2_fs_write_stream",
            "[method]descriptor.read-directory" => "p2_fs_directory",
            "[stream-new-0][method]descriptor.write-via-stream" => "p2_fs_write_new",
            "[async-lower][stream-write-0][method]descriptor.write-via-stream" => "p2_fs_write",
            "[async-lower][stream-read-0][method]descriptor.read-via-stream" => "p2_fs_read",
            "[async-lower][stream-read-0][method]descriptor.read-directory" => "p2_fs_entries",
            name if name.starts_with("[stream-drop-") => "p2_fs_drop_stream",
            name if name.starts_with("[stream-cancel-") => "p2_cancel",
            _ => return future(name),
        });
    }
    None
}

fn future(name: &str) -> Option<&'static str> {
    Some(if name.starts_with("[future-new-") {
        "p2_future_new"
    } else if name.starts_with("[async-lower][future-read-") {
        "p2_future_read"
    } else if name.starts_with("[async-lower][future-write-") {
        "p2_future_write"
    } else if name.starts_with("[future-drop-") {
        "p2_drop"
    } else if name.starts_with("[future-cancel-") {
        "p2_cancel"
    } else {
        return None;
    })
}

fn external(module: &str) -> &str {
    match module {
        "wasi:cli/environment@0.3.0" => "wasi:cli/environment@0.2.12",
        "wasi:clocks/monotonic-clock@0.3.0" => "wasi:clocks/monotonic-clock@0.2.12",
        "wasi:clocks/system-clock@0.3.0" => "wasi:clocks/wall-clock@0.2.12",
        "wasi:random/random@0.3.0" => "wasi:random/random@0.2.12",
        "wasi:random/insecure@0.3.0" => "wasi:random/insecure@0.2.12",
        "wasi:filesystem/preopens@0.3.0" => "wasi:filesystem/preopens@0.2.12",
        "wasi:filesystem/types@0.3.0" => "wasi:filesystem/types@0.2.12",
        other => other,
    }
}

/// Resolve Perry's runtime imports against the interfaces of the selected WIT world.
pub(crate) struct RuntimeImports<'a> {
    pub(crate) target: Target,
    pub(crate) interfaces: &'a [String],
}

impl RuntimeImports<'_> {
    pub(crate) const RAW: Self = Self {
        target: Target::P3,
        interfaces: &[],
    };

    pub(super) fn helper<'a>(&self, module: &str, name: &'a str) -> Option<&'a str> {
        if module == perry_codegen::HELPER_MODULE {
            Some(name)
        } else if self.target == Target::P2 {
            helper(module, name)
        } else {
            None
        }
    }

    pub(super) fn module<'a>(&'a self, module: &'a str, name: &str) -> Result<&'a str> {
        let pinned = external(module);
        let fallback = match self.target {
            Target::P2 => {
                ensure_diag!(
                    pinned != "wasi:io/poll@0.2.12"
                        || self
                            .interfaces
                            .iter()
                            .any(|name| name.starts_with("wasi:io/poll@0.2.")),
                    codes::CONTRACT_MISMATCH,
                    "async TypeScript in a P2 world requires wasi:io/poll; import wasi:io/poll@0.2.12 or include wasi:cli/imports@0.2.12"
                );
                ensure_diag!(
                    !matches!(pinned, "$root" | "[export]$root" | "perry:canon")
                        && !(pinned.starts_with("wasi:") && pinned.contains("@0.3.")),
                    codes::UNSUPPORTED_CONSTRUCT,
                    "P2 backend does not support runtime import {module}#{name}; use a P3 world for this capability"
                );
                pinned
            }
            Target::P3 if pinned == module || self.interfaces.iter().any(|name| name == module) => {
                return Ok(module);
            }
            Target::P3 => module,
        };
        let Some(base) = pinned.strip_suffix("@0.2.12") else {
            return Ok(fallback);
        };
        Ok(self
            .interfaces
            .iter()
            .find(|interface| {
                interface
                    .strip_prefix(base)
                    .is_some_and(|suffix| suffix.starts_with("@0.2."))
            })
            .map_or(fallback, String::as_str))
    }
}
