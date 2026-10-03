//! Typed string method calls and argument defaults.

use anyhow::{Result, bail, ensure};
use perry_hir::{
    ir::{CompareOp, Expr},
    types::Type as HirType,
};
use waffle::{Operator, Type, Value};

use super::FunctionLowerer;

impl FunctionLowerer<'_> {
    /// Rejects unsupported coercions before a primitive can become a descriptor address.
    pub(super) fn string_operand(&mut self, expr: &Expr) -> Result<Value> {
        ensure!(
            self.is_string(expr),
            "String coercion is unsupported for {:?}",
            self.infer_expr_type(expr)
        );
        self.expression(expr)
    }

    /// Reads scalar length for properties and truthiness of a known string.
    pub(super) fn string_length(&mut self, desc: Value) -> Value {
        self.op(
            Operator::I32Load {
                memory: waffle::MemoryArg {
                    align: 2,
                    offset: 8,
                    memory: self.registry.memory,
                },
            },
            &[desc],
            &[Type::I32],
        )
    }

    /// Strict equality distinguishes types; coercive comparisons require two strings.
    pub(super) fn string_comparison(
        &mut self,
        op: CompareOp,
        left: &Expr,
        right: &Expr,
    ) -> Result<Value> {
        let left_type = self.infer_expr_type(left);
        let right_type = self.infer_expr_type(right);
        let left_val = self.expression(left)?;
        let right_val = self.expression(right)?;
        if left_type != HirType::String || right_type != HirType::String {
            ensure!(
                matches!(op, CompareOp::Eq | CompareOp::Ne)
                    && matches!(
                        left_type,
                        HirType::String | HirType::Number | HirType::Boolean
                    )
                    && matches!(
                        right_type,
                        HirType::String | HirType::Number | HirType::Boolean
                    ),
                "Unsupported mixed string comparison: {left_type:?} {op:?} {right_type:?}"
            );
            return Ok(self.op(
                Operator::I32Const {
                    value: u32::from(op == CompareOp::Ne),
                },
                &[],
                &[Type::I32],
            ));
        }
        let helpers = self
            .registry
            .string_helpers
            .expect("String runtime is registered");
        let comparison = self.op(
            Operator::Call {
                function_index: helpers.str_compare,
            },
            &[left_val, right_val],
            &[Type::I32],
        );
        let zero = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let operator = match op {
            CompareOp::Eq | CompareOp::LooseEq => Operator::I32Eq,
            CompareOp::Ne | CompareOp::LooseNe => Operator::I32Ne,
            CompareOp::Lt => Operator::I32LtS,
            CompareOp::Le => Operator::I32LeS,
            CompareOp::Gt => Operator::I32GtS,
            CompareOp::Ge => Operator::I32GeS,
        };
        Ok(self.op(operator, &[comparison, zero], &[Type::I32]))
    }

    /// Lowers supported methods after checking receiver types and accepted arities.
    pub(super) fn string_method(
        &mut self,
        receiver: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<Value> {
        ensure!(
            self.is_string(receiver),
            "String method '{method}' requires a string receiver"
        );
        let arity = match method {
            "slice" => 0..=2,
            "charAt" => 0..=1,
            "indexOf" => 1..=2,
            _ => bail!("Unsupported string method: {method}"),
        };
        ensure!(
            arity.contains(&args.len()),
            "Unsupported argument count for string method '{method}': {}",
            args.len()
        );
        let helpers = self
            .registry
            .string_helpers
            .expect("String runtime is registered");
        let desc = self.expression(receiver)?;
        let (function_index, values, result_type) = match method {
            "slice" => {
                let start = self.position_argument(args.first(), 0.0)?;
                let end = self.position_argument(args.get(1), f64::INFINITY)?;
                (helpers.str_slice, vec![desc, start, end], Type::I32)
            }
            "charAt" => {
                let position = self.position_argument(args.first(), 0.0)?;
                (helpers.str_char_at, vec![desc, position], Type::I32)
            }
            "indexOf" => {
                ensure!(
                    self.is_string(&args[0]),
                    "indexOf requires a string search operand"
                );
                let search = self.expression(&args[0])?;
                let position = self.position_argument(args.get(1), 0.0)?;
                (
                    helpers.str_index_of,
                    vec![desc, search, position],
                    Type::F64,
                )
            }
            _ => unreachable!("Method checked above"),
        };
        Ok(self.op(Operator::Call { function_index }, &values, &[result_type]))
    }

    /// Supplies omitted/undefined defaults without silently coercing other argument types.
    fn position_argument(&mut self, arg: Option<&Expr>, default: f64) -> Result<Value> {
        let value = match arg {
            None | Some(Expr::Undefined) => self.op(
                Operator::F64Const {
                    value: default.to_bits(),
                },
                &[],
                &[Type::F64],
            ),
            Some(expr) => self.expression(expr)?,
        };
        ensure!(
            self.body.values[value].ty(&self.body.type_pool) == Some(Type::F64),
            "String position arguments must be numeric"
        );
        Ok(value)
    }
}
