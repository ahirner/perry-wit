//! Preserve filesystem argument effects before validating options or starting I/O.

use anyhow::{Result, bail, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};
use waffle::{MemoryArg, Operator, Type, Value};

use super::FunctionLowerer;
use crate::waffle_backend::{
    abi, bytes::is_byte_view, capabilities::FilesystemOperation, objects::is_object,
    text_or_bytes::is_text_or_bytes,
};

impl FunctionLowerer<'_> {
    pub(super) fn filesystem_operation(
        &mut self,
        name: &str,
        operation: FilesystemOperation,
        arguments: &[Expr],
    ) -> Result<Option<Value>> {
        let zero = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let mut signal = zero;
        match operation {
            FilesystemOperation::WriteFile => {
                ensure!(
                    (2..=3).contains(&arguments.len()),
                    "writeFileSync accepts a path, data, and optional encoding/options"
                );
                let path = self.string_receiver(&arguments[0])?;
                let data_type = self.infer_expr_type(&arguments[1]);
                let (data, binary) = if is_text_or_bytes(&data_type) {
                    let value = self.expression(&arguments[1])?;
                    self.text_or_bytes_parts(value)
                } else {
                    let binary = is_byte_view(&data_type);
                    ensure!(
                        binary || data_type == HirType::String,
                        "writeFileSync data must be a string or Uint8Array"
                    );
                    let data = self.expression(&arguments[1])?;
                    let binary = self.op(
                        Operator::I32Const {
                            value: u32::from(binary),
                        },
                        &[],
                        &[Type::I32],
                    );
                    (data, binary)
                };
                let helpers = self.registry.filesystem_helpers.unwrap();
                let valid = if let Some(options) = arguments
                    .get(2)
                    .filter(|options| is_object(&self.infer_expr_type(options)))
                {
                    let object = self.expression(options)?;
                    signal = self.filesystem_signal(name, options, object)?;
                    self.op(
                        Operator::Call {
                            function_index: helpers.write_object_options,
                        },
                        &[object, binary],
                        &[Type::I32],
                    )
                } else {
                    let encoding = self.filesystem_encoding(arguments.get(2))?;
                    let flag = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
                    self.op(
                        Operator::Call {
                            function_index: helpers.write_options,
                        },
                        &[encoding, flag, binary],
                        &[Type::I32],
                    )
                };
                if let Some(record) = self.start_task(
                    &crate::waffle_backend::promises::TaskTarget::Intrinsic(name.into()),
                    &[path, data, valid, signal],
                    None,
                )? {
                    return Ok(Some(record));
                }
                self.call_completion(
                    self.registry.filesystem_helpers.unwrap().write,
                    &[path, data, valid],
                );
                Ok(None)
            }
            FilesystemOperation::ReadBytes
            | FilesystemOperation::ReadText
            | FilesystemOperation::ReadValue => {
                ensure!(
                    (1..=2).contains(&arguments.len()),
                    "readFileSync accepts a path and optional encoding/options"
                );
                let path = self.string_receiver(&arguments[0])?;
                let helpers = self.registry.filesystem_helpers.unwrap();
                let mode = if let Some(options) = arguments
                    .get(1)
                    .filter(|options| is_object(&self.infer_expr_type(options)))
                {
                    let object = self.expression(options)?;
                    signal = self.filesystem_signal(name, options, object)?;
                    self.op(
                        Operator::Call {
                            function_index: helpers.read_object_options,
                        },
                        &[object],
                        &[Type::I32],
                    )
                } else {
                    let encoding = self.filesystem_encoding(arguments.get(1))?;
                    let flag = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
                    let valid = self.op(Operator::I32Const { value: 1 }, &[], &[Type::I32]);
                    self.op(
                        Operator::Call {
                            function_index: helpers.read_options,
                        },
                        &[encoding, flag, valid],
                        &[Type::I32],
                    )
                };
                if let Some(record) = self.start_task(
                    &crate::waffle_backend::promises::TaskTarget::Intrinsic(name.into()),
                    &[path, mode, signal],
                    None,
                )? {
                    return Ok(Some(record));
                }
                let payload = self.call_completion(helpers.read, &[path, mode]);
                let descriptor = abi::decode_payload(&mut self.body, self.block, payload, true);
                let value = if operation == FilesystemOperation::ReadValue {
                    let binary = self.op(Operator::I32Eqz, &[mode], &[Type::I32]);
                    self.op(Operator::I32Or, &[descriptor, binary], &[Type::I32])
                } else {
                    descriptor
                };
                Ok(Some(value))
            }
            operation => {
                let max_arguments = if operation == FilesystemOperation::Exists {
                    1
                } else {
                    2
                };
                ensure!(
                    (1..=max_arguments).contains(&arguments.len()),
                    "{} accepts a path{}",
                    operation.name(),
                    if max_arguments == 2 {
                        " and optional options"
                    } else {
                        ""
                    }
                );
                let path = self.string_receiver(&arguments[0])?;
                let valid = self.filesystem_metadata_options(operation, arguments.get(1))?;
                if let Some(record) = self.start_task(
                    &crate::waffle_backend::promises::TaskTarget::Intrinsic(name.into()),
                    &[path, valid, zero],
                    None,
                )? {
                    return Ok(Some(record));
                }
                let helpers = self.registry.filesystem_helpers.unwrap();
                let payload = if operation == FilesystemOperation::ReadDirectory {
                    self.call_completion(helpers.read_directory, &[path, valid])
                } else {
                    let code = match operation {
                        FilesystemOperation::Stat => 0,
                        FilesystemOperation::Exists => 1,
                        FilesystemOperation::MakeDirectory => 2,
                        FilesystemOperation::Unlink => 3,
                        FilesystemOperation::RemoveDirectory => 4,
                        _ => unreachable!(),
                    };
                    let code = self.op(Operator::I32Const { value: code }, &[], &[Type::I32]);
                    self.call_completion(helpers.metadata, &[path, code, valid])
                };
                Ok(matches!(
                    operation,
                    FilesystemOperation::Stat
                        | FilesystemOperation::Exists
                        | FilesystemOperation::ReadDirectory
                )
                .then(|| abi::decode_payload(&mut self.body, self.block, payload, true)))
            }
        }
    }

    fn filesystem_signal(&mut self, name: &str, expression: &Expr, object: Value) -> Result<Value> {
        let zero = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let HirType::Object(shape) = self.infer_expr_type(expression) else {
            return Ok(zero);
        };
        let Some(field) = shape.properties.get("signal") else {
            return Ok(zero);
        };
        ensure!(
            matches!(
                self.contract.intrinsics.get(name),
                Some(crate::waffle_backend::resolve::TypedIntrinsic::Capability(
                    crate::waffle_backend::capabilities::CapabilityOperation::FilesystemPromise(_)
                ))
            ),
            "AbortSignal is supported by promise readFile/writeFile; synchronous filesystem calls do not accept signals"
        );
        self.option_signal(object, &field.ty, field.optional, "Filesystem")
    }

    pub(super) fn stats_property(&mut self, receiver: &Expr, property: &str) -> Result<Value> {
        let offset = match property {
            "size" => 0,
            "mtimeMs" => 8,
            _ => bail!("Unsupported Stats property '{property}'"),
        };
        let stats = self.expression(receiver)?;
        Ok(self.op(
            Operator::F64Load {
                memory: MemoryArg {
                    align: 3,
                    offset,
                    memory: self.registry.memory,
                },
            },
            &[stats],
            &[Type::F64],
        ))
    }

    pub(super) fn stats_method(
        &mut self,
        receiver: &Expr,
        property: &str,
        arguments: &[Expr],
    ) -> Result<Value> {
        ensure!(
            arguments.is_empty(),
            "Stats methods do not accept arguments"
        );
        let tag = match property {
            "isFile" => 5,
            "isDirectory" => 2,
            _ => bail!("Unsupported Stats method '{property}'"),
        };
        let stats = self.expression(receiver)?;
        let kind = self.op(
            Operator::I32Load {
                memory: MemoryArg {
                    align: 2,
                    offset: 16,
                    memory: self.registry.memory,
                },
            },
            &[stats],
            &[Type::I32],
        );
        let tag = self.op(Operator::I32Const { value: tag }, &[], &[Type::I32]);
        Ok(self.op(Operator::I32Eq, &[kind, tag], &[Type::I32]))
    }

    fn filesystem_metadata_options(
        &mut self,
        operation: FilesystemOperation,
        expression: Option<&Expr>,
    ) -> Result<Value> {
        let one = self.op(Operator::I32Const { value: 1 }, &[], &[Type::I32]);
        let zero = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let Some(expression) = expression else {
            return Ok(one);
        };
        if self.infer_expr_type(expression) == HirType::Void {
            self.expression(expression)?;
            return Ok(one);
        }
        let supports_options = matches!(
            operation,
            FilesystemOperation::Stat | FilesystemOperation::ReadDirectory
        );
        if matches!(expression, Expr::Null) {
            return Ok(if supports_options { one } else { zero });
        }
        if is_object(&self.infer_expr_type(expression)) {
            let object = self.expression(expression)?;
            let operation = self.op(
                Operator::I32Const {
                    value: match operation {
                        FilesystemOperation::Stat => 0,
                        FilesystemOperation::ReadDirectory => 1,
                        _ => 2,
                    },
                },
                &[],
                &[Type::I32],
            );
            return Ok(self.op(
                Operator::Call {
                    function_index: self
                        .registry
                        .filesystem_helpers
                        .unwrap()
                        .metadata_object_options,
                },
                &[object, operation],
                &[Type::I32],
            ));
        }
        if operation == FilesystemOperation::ReadDirectory {
            let encoding = self.filesystem_encoding(Some(expression))?;
            return Ok(self.op(
                Operator::Call {
                    function_index: self.registry.filesystem_helpers.unwrap().directory_options,
                },
                &[encoding],
                &[Type::I32],
            ));
        }
        self.expression(expression)?;
        Ok(zero)
    }

    fn filesystem_encoding(&mut self, expression: Option<&Expr>) -> Result<Value> {
        let Some(expression) = expression else {
            return Ok(self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]));
        };
        if matches!(
            super::types::StringKind::of(&self.infer_expr_type(expression)),
            Some(super::types::StringKind::Present | super::types::StringKind::Optional)
        ) {
            return self.expression(expression);
        }
        if !matches!(expression, Expr::Null) {
            self.expression(expression)?;
        }
        let omitted =
            matches!(expression, Expr::Null) || self.infer_expr_type(expression) == HirType::Void;
        Ok(self.op(
            Operator::I32Const {
                value: if omitted { 0 } else { u32::MAX },
            },
            &[],
            &[Type::I32],
        ))
    }
}
