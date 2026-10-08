//! Typed Temporal operations over immutable caller-owned codec storage.

use super::{FunctionLowerer, options::literal_properties};
use crate::waffle_backend::time::{TimeConstructor, TimeKind};
use anyhow::{Result, bail, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};
use waffle::{BlockTarget, MemoryArg, Operator, Terminator, Type, Value};

impl FunctionLowerer<'_> {
    pub(super) fn new_time(
        &mut self,
        operation: TimeConstructor,
        arguments: &[Expr],
    ) -> Result<Value> {
        ensure!(
            arguments.len() == 1,
            "{} requires one argument",
            operation.name()
        );
        let argument = &arguments[0];
        ensure!(
            self.infer_expr_type(argument) == operation.argument_type(),
            "{} requires a statically known {:?}; coercion and object overloads are unsupported",
            operation.name(),
            operation.argument_type()
        );
        let value = self.expression(argument)?;
        let output = self.time_storage(16);
        let (name, args) = if operation == TimeConstructor::InstantFromMs {
            ("time_instant_from_ms", vec![value, output])
        } else {
            let fields = [0, 4].map(|offset| {
                self.op(
                    Operator::I32Load {
                        memory: MemoryArg {
                            memory: self.registry.memory,
                            align: 2,
                            offset,
                        },
                    },
                    &[value],
                    &[Type::I32],
                )
            });
            let name = match operation {
                TimeConstructor::InstantFrom => "time_instant_parse",
                TimeConstructor::PlainFrom => "time_plain_parse",
                TimeConstructor::PlainDateFrom => "time_plain_date_parse",
                TimeConstructor::InstantFromMs => unreachable!(),
            };
            (name, vec![fields[0], fields[1], output])
        };
        self.time_codec(name, &args);
        Ok(output)
    }

    pub(super) fn time_method(
        &mut self,
        kind: TimeKind,
        receiver: &Expr,
        method: &str,
        arguments: &[Expr],
    ) -> Result<Value> {
        match method {
            "toString" => {
                ensure!(
                    arguments.is_empty(),
                    "Temporal.toString options are unsupported"
                );
                let value = self.expression(receiver)?;
                let capacity_val = match kind {
                    TimeKind::PlainDate => 16,
                    _ => 33,
                };
                let output = self.time_storage(capacity_val);
                let capacity = self.op(
                    Operator::I32Const {
                        value: capacity_val,
                    },
                    &[],
                    &[Type::I32],
                );
                let name = match kind {
                    TimeKind::Instant => "time_instant_format",
                    TimeKind::PlainDateTime => "time_plain_format",
                    TimeKind::PlainDate => "time_plain_date_format",
                };
                let length = self.time_codec(name, &[value, output, capacity]);
                Ok(self.op(
                    Operator::Call {
                        function_index: self.registry.string_helpers.unwrap().lift_canonical,
                    },
                    &[output, length],
                    &[Type::I32],
                ))
            }
            "add" if kind == TimeKind::PlainDateTime || kind == TimeKind::PlainDate => {
                ensure!(
                    arguments.len() == 1,
                    "Temporal.add requires one {{days: number}} record"
                );
                let value = self.expression(receiver)?;
                let days = self.time_days(&arguments[0])?;
                let output = self.time_storage(16);
                self.time_codec("time_plain_add_days", &[value, days, output]);
                Ok(output)
            }
            _ => bail!("Unsupported Temporal method '{method}'"),
        }
    }

    pub(super) fn time_property(&mut self, receiver: &Expr, property: &str) -> Result<Value> {
        let kind = TimeKind::of(&self.infer_expr_type(receiver)).expect("Temporal receiver");
        let value = self.expression(receiver)?;
        let (name, args) = match kind {
            TimeKind::Instant => {
                ensure!(
                    property == "epochMilliseconds",
                    "Unsupported Temporal.Instant property '{property}'"
                );
                ("time_instant_ms", vec![value])
            }
            TimeKind::PlainDateTime => {
                let part = match property {
                    "year" => 0,
                    "month" => 1,
                    "day" => 2,
                    "hour" => 3,
                    "minute" => 4,
                    "second" => 5,
                    "millisecond" => 6,
                    "microsecond" => 7,
                    "nanosecond" => 8,
                    "dayOfWeek" => 9,
                    _ => bail!("Unsupported Temporal.PlainDateTime property '{property}'"),
                };
                let part = self.op(Operator::I32Const { value: part }, &[], &[Type::I32]);
                ("time_plain_part", vec![value, part])
            }
            TimeKind::PlainDate => {
                let part = match property {
                    "year" => 0,
                    "month" => 1,
                    "day" => 2,
                    "dayOfWeek" => 9,
                    _ => bail!("Unsupported Temporal.PlainDate property '{property}'"),
                };
                let part = self.op(Operator::I32Const { value: part }, &[], &[Type::I32]);
                ("time_plain_part", vec![value, part])
            }
        };
        Ok(self.op(
            Operator::Call {
                function_index: self.registry.time_helpers[name],
            },
            &args,
            &[Type::F64],
        ))
    }

    fn time_days(&mut self, expression: &Expr) -> Result<Value> {
        if let Some(mut fields) = literal_properties(self.contract, expression)? {
            if let (Some(("days", days)), None) = (fields.next(), fields.next())
                && self.infer_expr_type(days) == HirType::Number
            {
                return self.expression(days);
            }
            bail!("Temporal.add supports only {{days: number}}");
        }
        let HirType::Object(fields) = self.infer_expr_type(expression) else {
            bail!("Temporal.PlainDateTime.add requires a statically typed {{days: number}} record");
        };
        ensure!(
            fields.properties.len() == 1
                && fields.index_signature.is_none()
                && fields
                    .properties
                    .get("days")
                    .is_some_and(|field| field.ty == HirType::Number && !field.optional),
            "Temporal.PlainDateTime.add supports only {{days: number}}"
        );
        self.object_get(expression, &Expr::String("days".into()))
    }

    fn time_storage(&mut self, bytes: u32) -> Value {
        let zero = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let alignment = self.op(Operator::I32Const { value: 8 }, &[], &[Type::I32]);
        let size = self.op(Operator::I32Const { value: bytes }, &[], &[Type::I32]);
        let pointer = self.op(
            Operator::Call {
                function_index: self.registry.allocator.unwrap().realloc,
            },
            &[zero, zero, alignment, size],
            &[Type::I32],
        );
        self.reference_values.insert(pointer);
        pointer
    }

    /// Decode codec status into the ordinary catch/finally path before exposing output.
    fn time_codec(&mut self, name: &str, args: &[Value]) -> Value {
        let result = self.op(
            Operator::Call {
                function_index: self.registry.time_helpers[name],
            },
            args,
            &[Type::I64],
        );
        let shift = self.op(Operator::I64Const { value: 32 }, &[], &[Type::I64]);
        let status = self.op(Operator::I64ShrU, &[result, shift], &[Type::I64]);
        let status = self.op(Operator::I32WrapI64, &[status], &[Type::I32]);
        let ok = self.body.add_block();
        let error = self.body.add_block();
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: status,
                if_true: BlockTarget {
                    block: error,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: ok,
                    args: vec![],
                },
            },
        );
        self.block = error;
        let payload = self.op(Operator::F64ConvertI32U, &[status], &[Type::F64]);
        self.emit_native_throw(payload);
        self.block = ok;
        self.op(Operator::I32WrapI64, &[result], &[Type::I32])
    }
}
