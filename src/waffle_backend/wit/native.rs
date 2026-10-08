//! Use the component encoder's canonical import convention for native capabilities.

use super::WitWorld;
use anyhow::{Context, Result, bail, ensure};
use waffle::Module;
use wit_parser::{Type, TypeDefKind, WorldItem};

const HTTP: &str = "wasi:http/types@0.3.0";
const FILESYSTEM: &str = "wasi:filesystem/types@0.3.0";

#[derive(Clone, Copy)]
enum Payload {
    Stream,
    Completion,
    Trailers,
}

impl WitWorld {
    pub(in crate::waffle_backend) fn bind_native_imports(
        &self,
        module: &mut Module<'_>,
    ) -> Result<()> {
        for import in &mut module.imports {
            let binding = match import.module.as_str() {
                "host" => {
                    let (key, _) = self
                        .imports
                        .iter()
                        .find(|(_, binding)| {
                            binding.module == "$root"
                                && crate::sdk::codegen::to_camel_case(&binding.function.name)
                                    == import.name
                        })
                        .with_context(|| {
                            format!(
                                "WIT world has no import for core function '{}'",
                                import.name
                            )
                        })?;
                    ("$root".into(), key.clone())
                }
                "streams" => {
                    let export = self
                        .functions
                        .values()
                        .find(|export| export.function.params.iter().any(|param| matches!(param.ty, Type::Id(id) if matches!(self.resolve.types[id].kind, TypeDefKind::Stream(Some(Type::U8))))))
                        .context("Byte stream bindings require a direct WIT stream<u8> parameter")?;
                    let payloads = export.function.find_futures_and_streams(&self.resolve);
                    let index = payloads
                        .iter()
                        .position(|id| {
                            matches!(self.resolve.types[*id].kind, TypeDefKind::Stream(_))
                        })
                        .context("WIT entry requires a byte stream parameter")?;
                    let operation = match import.name.as_str() {
                        "read" => "stream-read",
                        "async-read" => "[async-lower]stream-read",
                        "cancel-read" => "[async-lower]stream-cancel-read",
                        "drop" => "stream-drop-readable",
                        other => bail!("Unsupported stream intrinsic '{other}'"),
                    };
                    {
                        let (prefix, operation) = operation
                            .strip_prefix("[async-lower]")
                            .map_or(("", operation), |op| ("[async-lower]", op));
                        (
                            "[export]$root".into(),
                            format!("{prefix}[{operation}-{index}]{}", export.core_name),
                        )
                    }
                }
                "http" => self.http_binding(&import.name)?,
                "http-server" => self.http_handler_binding(&import.name)?,
                "context" => ("wasi:cli/environment@0.3.0".into(), import.name.clone()),
                "random" => ("wasi:random/random@0.3.0".into(), "get-random-bytes".into()),
                "filesystem" => self.filesystem_binding(&import.name)?,
                "output" => self.output_binding(&import.name)?,
                module if module.starts_with("wasi:") => (module.into(), import.name.clone()),
                _ => continue,
            };
            if binding.0 != "$root" && !binding.0.starts_with("[export]") {
                ensure!(
                    self.resolve.worlds[self.world]
                        .imports
                        .iter()
                        .any(|(key, item)| matches!(item, WorldItem::Interface { .. })
                            && self.resolve.name_world_key(key) == binding.0),
                    "WIT world must import '{}' for this capability",
                    binding.0
                );
            }
            (import.module, import.name) = binding;
        }
        Ok(())
    }

    fn http_handler_binding(&self, name: &str) -> Result<(String, String)> {
        let function = match name {
            "method" => "[method]request.get-method",
            "scheme" => "[method]request.get-scheme",
            "authority" => "[method]request.get-authority",
            "path" => "[method]request.get-path-with-query",
            "headers" => "[method]request.get-headers",
            "consume" => "[static]request.consume-body",
            "response" => "[static]response.new",
            "status" => "[method]response.set-status-code",
            "new-stream" | "write" | "drop-writer" => {
                return self.payload_binding(
                    HTTP,
                    "[static]request.consume-body",
                    Payload::Stream,
                    match name {
                        "new-stream" => "stream-new",
                        "write" => "stream-write",
                        _ => "stream-drop-writable",
                    },
                );
            }
            "return" => {
                return Ok((
                    "[export]wasi:http/handler@0.3.0".into(),
                    "[task-return]handle".into(),
                ));
            }
            "backpressure-inc" => return Ok(("$root".into(), "[backpressure-inc]".into())),
            "backpressure-dec" => return Ok(("$root".into(), "[backpressure-dec]".into())),
            _ => return self.http_binding(name),
        };
        Ok((HTTP.into(), function.into()))
    }

    fn payload_binding(
        &self,
        interface: &str,
        function: &str,
        payload: Payload,
        operation: &str,
    ) -> Result<(String, String)> {
        let function = &self
            .imports
            .get(&format!("{interface}#{function}"))
            .with_context(|| format!("WIT world must import '{interface}' with '{function}'"))?
            .function;
        let payloads = function.find_futures_and_streams(&self.resolve);
        let index = payloads
            .iter()
            .position(|id| match (&self.resolve.types[*id].kind, payload) {
                (TypeDefKind::Stream(_), Payload::Stream) => true,
                (
                    TypeDefKind::Future(Some(Type::Id(id))),
                    Payload::Completion | Payload::Trailers,
                ) => {
                    matches!(&self.resolve.types[*id].kind, TypeDefKind::Result(result)
                        if result.ok.is_some() == matches!(payload, Payload::Trailers))
                }
                _ => false,
            })
            .context("Native capability has an incompatible stream/future payload")?;
        let (prefix, operation) = operation
            .strip_prefix("async-")
            .map_or(("", operation), |operation| ("[async-lower]", operation));
        Ok((
            interface.into(),
            format!("{prefix}[{operation}-{index}]{}", function.name),
        ))
    }

    fn http_binding(&self, name: &str) -> Result<(String, String)> {
        if name == "async-send" {
            return Ok(("wasi:http/client@0.3.0".into(), "[async-lower]send".into()));
        }
        if let Some(operation) = name
            .strip_prefix("async-")
            .or_else(|| name.strip_prefix("cancel-"))
        {
            let (operation, payload, family) =
                if let Some(operation) = operation.strip_suffix("-trailers") {
                    (operation, Payload::Trailers, "future")
                } else if let Some(operation) = operation.strip_suffix("-completion") {
                    (operation, Payload::Completion, "future")
                } else {
                    (operation, Payload::Stream, "stream")
                };
            let cancel = if name.starts_with("cancel-") {
                "cancel-"
            } else {
                ""
            };
            return self.payload_binding(
                HTTP,
                "[static]request.consume-body",
                payload,
                &format!("async-{family}-{cancel}{operation}"),
            );
        }
        let function = match name {
            "fields" => "[static]fields.from-list",
            "copy-fields" => "[method]fields.copy-all",
            "request" => "[static]request.new",
            "set-method" => "[method]request.set-method",
            "scheme" => "[method]request.set-scheme",
            "authority" => "[method]request.set-authority",
            "path" => "[method]request.set-path-with-query",
            "status" => "[method]response.get-status-code",
            "headers" => "[method]response.get-headers",
            "consume" => "[static]response.consume-body",
            "drop-fields" => "[resource-drop]fields",
            "drop-request" => "[resource-drop]request",
            "send" => return Ok(("wasi:http/client@0.3.0".into(), "send".into())),
            "read" | "drop-reader" | "new-stream" | "write" | "drop-writer" => {
                return self.payload_binding(
                    HTTP,
                    "[static]request.consume-body",
                    Payload::Stream,
                    match name {
                        "read" => "stream-read",
                        "drop-reader" => "stream-drop-readable",
                        "new-stream" => "stream-new",
                        "write" => "stream-write",
                        _ => "stream-drop-writable",
                    },
                );
            }
            "new-set" => return Ok(("$root".into(), "[waitable-set-new]".into())),
            "join" => return Ok(("$root".into(), "[waitable-join]".into())),
            "wait" => return Ok(("$root".into(), "[waitable-set-wait]".into())),
            "drop-set" => return Ok(("$root".into(), "[waitable-set-drop]".into())),
            name => {
                let (operation, payload) = if let Some(operation) = name.strip_suffix("-trailers") {
                    (operation, Payload::Trailers)
                } else if let Some(operation) = name.strip_suffix("-completion") {
                    (operation, Payload::Completion)
                } else if let Some(operation) = name.strip_prefix("drop-trailers-") {
                    (
                        if operation == "reader" {
                            "drop-readable"
                        } else {
                            "drop-writable"
                        },
                        Payload::Trailers,
                    )
                } else if let Some(operation) = name.strip_prefix("drop-completion-") {
                    (
                        if operation == "reader" {
                            "drop-readable"
                        } else {
                            "drop-writable"
                        },
                        Payload::Completion,
                    )
                } else {
                    bail!("Unknown HTTP canonical import '{name}'")
                };
                let operation = if operation == "write" {
                    "async-future-write".into()
                } else {
                    format!("future-{operation}")
                };
                return self.payload_binding(
                    HTTP,
                    "[static]request.consume-body",
                    payload,
                    &operation,
                );
            }
        };
        Ok((HTTP.into(), function.into()))
    }

    fn filesystem_binding(&self, name: &str) -> Result<(String, String)> {
        if let Some(name) = name.strip_prefix("async-") {
            let (interface, function) = self.filesystem_binding(name)?;
            return Ok((interface, format!("[async-lower]{function}")));
        }
        let function = match name {
            "directories" => {
                return Ok((
                    "wasi:filesystem/preopens@0.3.0".into(),
                    "get-directories".into(),
                ));
            }
            "open" => "[method]descriptor.open-at",
            "start-write" => "[method]descriptor.write-via-stream",
            "start-read" => "[method]descriptor.read-via-stream",
            "stat" => "[method]descriptor.stat-at",
            "mkdir" => "[method]descriptor.create-directory-at",
            "unlink" => "[method]descriptor.unlink-file-at",
            "rmdir" => "[method]descriptor.remove-directory-at",
            "start-directory" => "[method]descriptor.read-directory",
            "drop-descriptor" => "[resource-drop]descriptor",
            "read-entry" | "cancel-read-entry" | "drop-entries" => {
                return self.payload_binding(
                    FILESYSTEM,
                    "[method]descriptor.read-directory",
                    Payload::Stream,
                    match name {
                        "read-entry" => "stream-read",
                        "cancel-read-entry" => "async-stream-cancel-read",
                        _ => "stream-drop-readable",
                    },
                );
            }
            "await" | "drop-future" => {
                return self.payload_binding(
                    FILESYSTEM,
                    "[method]descriptor.write-via-stream",
                    Payload::Completion,
                    if name == "await" {
                        "future-read"
                    } else {
                        "future-drop-readable"
                    },
                );
            }
            "new" | "write" | "read" | "cancel-read" | "cancel-write" | "drop-reader"
            | "drop-writer" => {
                return self.payload_binding(
                    FILESYSTEM,
                    "[method]descriptor.write-via-stream",
                    Payload::Stream,
                    &format!(
                        "{}stream-{}",
                        if name.starts_with("cancel-") {
                            "async-"
                        } else {
                            ""
                        },
                        match name {
                            "drop-reader" => "drop-readable",
                            "drop-writer" => "drop-writable",
                            name => name,
                        }
                    ),
                );
            }
            _ => bail!("Unknown filesystem canonical import '{name}'"),
        };
        Ok((FILESYSTEM.into(), function.into()))
    }

    fn output_binding(&self, name: &str) -> Result<(String, String)> {
        if let Some(name) = name.strip_prefix("async-") {
            let (interface, function) = self.output_binding(name)?;
            return Ok((interface, format!("[async-lower]{function}")));
        }
        if matches!(name, "stdout" | "stderr") {
            return Ok((format!("wasi:cli/{name}@0.3.0"), "write-via-stream".into()));
        }
        let channel = ["stdout", "stderr"]
            .into_iter()
            .find(|channel| {
                self.imports
                    .contains_key(&format!("wasi:cli/{channel}@0.3.0#write-via-stream"))
            })
            .context("WIT world must import wasi:cli/stdout@0.3.0 or wasi:cli/stderr@0.3.0")?;
        let (payload, operation) = match name {
            "new" => (Payload::Stream, "stream-new"),
            "write" => (Payload::Stream, "stream-write"),
            "cancel-write" => (Payload::Stream, "async-stream-cancel-write"),
            "drop-writer" => (Payload::Stream, "stream-drop-writable"),
            "await" => (Payload::Completion, "future-read"),
            "drop-future" => (Payload::Completion, "future-drop-readable"),
            _ => bail!("Unknown output canonical import '{name}'"),
        };
        self.payload_binding(
            &format!("wasi:cli/{channel}@0.3.0"),
            "write-via-stream",
            payload,
            operation,
        )
    }
}
