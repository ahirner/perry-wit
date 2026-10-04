//! Preserve filesystem argument effects before validating options or starting I/O.

use anyhow::{Result, bail, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};
use waffle::{MemoryArg, Operator, Type, Value};

use super::{FunctionLowerer, options::literal_properties};
use crate::waffle_backend::{
    abi, bytes::is_byte_view, capabilities::FilesystemOperation, text_or_bytes::is_text_or_bytes,
};

impl FunctionLowerer<'_> {
    pub(super) fn filesystem_operation(
        &mut self,
        operation: FilesystemOperation,
        arguments: &[Expr],
    ) -> Result<Option<Value>> {
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
                let (encoding, flag, valid) = self.filesystem_options(arguments.get(2))?;
                let checked = self.op(
                    Operator::Call {
                        function_index: self.registry.filesystem_helpers.unwrap().write_options,
                    },
                    &[encoding, flag, binary],
                    &[Type::I32],
                );
                let valid = self.op(Operator::I32And, &[checked, valid], &[Type::I32]);
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
                let (encoding, flag, valid) = self.filesystem_options(arguments.get(1))?;
                let helpers = self.registry.filesystem_helpers.unwrap();
                let mode = self.op(
                    Operator::Call {
                        function_index: helpers.read_options,
                    },
                    &[encoding, flag, valid],
                    &[Type::I32],
                );
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
        let Some(properties) = literal_properties(self.contract, expression)? else {
            if operation == FilesystemOperation::ReadDirectory {
                let encoding = self.filesystem_option_string(expression, true)?;
                return Ok(self.op(
                    Operator::Call {
                        function_index: self.registry.filesystem_helpers.unwrap().directory_options,
                    },
                    &[encoding],
                    &[Type::I32],
                ));
            }
            self.expression(expression)?;
            return Ok(zero);
        };
        let mut valid = if supports_options { one } else { zero };
        let mut fields = std::collections::BTreeMap::new();
        for (name, expression) in properties {
            if operation == FilesystemOperation::ReadDirectory && name == "encoding" {
                let encoding = self.filesystem_option_string(expression, true)?;
                fields.insert(
                    name,
                    self.op(
                        Operator::Call {
                            function_index: self
                                .registry
                                .filesystem_helpers
                                .unwrap()
                                .directory_options,
                        },
                        &[encoding],
                        &[Type::I32],
                    ),
                );
                continue;
            }
            let expected = match (operation, name.as_str()) {
                (FilesystemOperation::Stat, "bigint")
                | (FilesystemOperation::ReadDirectory, "recursive" | "withFileTypes") => {
                    Some(false)
                }
                (FilesystemOperation::Stat, "throwIfNoEntry") => Some(true),
                _ => None,
            };
            let value = if matches!(expression, Expr::Null) {
                None
            } else {
                Some(self.expression(expression)?)
            };
            if let Some(expected) = expected {
                let checked = if self.infer_expr_type(expression) == HirType::Boolean {
                    if expected {
                        value.unwrap()
                    } else {
                        self.op(Operator::I32Eqz, &[value.unwrap()], &[Type::I32])
                    }
                } else {
                    zero
                };
                fields.insert(name, checked);
            } else {
                valid = zero;
            }
        }
        for checked in fields.values() {
            valid = self.op(Operator::I32And, &[valid, *checked], &[Type::I32]);
        }
        Ok(valid)
    }

    fn filesystem_options(&mut self, expression: Option<&Expr>) -> Result<(Value, Value, Value)> {
        let zero = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let mut encoding = zero;
        let mut flag = zero;
        let mut valid = self.op(Operator::I32Const { value: 1 }, &[], &[Type::I32]);
        if let Some(expression) = expression {
            if let Some(properties) = literal_properties(self.contract, expression)? {
                for (name, value) in properties {
                    match name.as_str() {
                        "encoding" => encoding = self.filesystem_option_string(value, true)?,
                        "flag" => flag = self.filesystem_option_string(value, false)?,
                        _ => {
                            if !matches!(value, Expr::Null) {
                                self.expression(value)?;
                            }
                            valid = zero;
                        }
                    }
                }
            } else {
                encoding = self.filesystem_option_string(expression, true)?;
            }
        }
        Ok((encoding, flag, valid))
    }

    fn filesystem_option_string(&mut self, expression: &Expr, default: bool) -> Result<Value> {
        if self.infer_expr_type(expression) == HirType::String {
            return self.string_receiver(expression);
        }
        if !matches!(expression, Expr::Null) {
            self.expression(expression)?;
        }
        let omitted =
            matches!(expression, Expr::Null) || self.infer_expr_type(expression) == HirType::Void;
        Ok(self.op(
            Operator::I32Const {
                value: if default && omitted { 0 } else { u32::MAX },
            },
            &[],
            &[Type::I32],
        ))
    }
}
