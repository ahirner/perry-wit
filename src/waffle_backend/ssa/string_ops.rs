//! Typed string method calls and argument defaults.

use anyhow::{Result, bail, ensure};
use perry_hir::{
    ir::{CompareOp, Expr},
    types::Type as HirType,
};
use waffle::{BlockTarget, Operator, Terminator, Type, Value};

use super::FunctionLowerer;
use super::types::StringKind;

impl FunctionLowerer<'_> {
    /// Rejects unsupported coercions before a primitive can become a descriptor address.
    pub(super) fn string_operand(&mut self, expr: &Expr) -> Result<Value> {
        ensure!(
            self.infer_expr_type(expr) == HirType::String,
            "String coercion is unsupported for {:?}",
            self.infer_expr_type(expr)
        );
        self.expression(expr)
    }

    /// String-only operations trap on undefined instead of reading address zero as a descriptor.
    pub(super) fn string_receiver(&mut self, expr: &Expr) -> Result<Value> {
        ensure!(
            self.is_string(expr),
            "Expected a string, got {:?}",
            self.infer_expr_type(expr)
        );
        let value = self.expression(expr)?;
        let present = self.body.add_block();
        let missing = self.body.add_block();
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: value,
                if_true: BlockTarget {
                    block: present,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: missing,
                    args: vec![],
                },
            },
        );
        self.body.set_terminator(missing, Terminator::Unreachable);
        self.block = present;
        Ok(value)
    }

    pub(super) fn string_truthiness(&mut self, desc: Value) -> Value {
        let present = self.body.add_block();
        let join = self.body.add_block();
        let result = self.body.add_blockparam(join, Type::I32);
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: desc,
                if_true: BlockTarget {
                    block: present,
                    args: vec![],
                },
                if_false: BlockTarget {
                    block: join,
                    args: vec![desc],
                },
            },
        );
        self.block = present;
        let length = self.string_length(desc);
        self.branch(join, vec![length]);
        self.block = join;
        result
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
        let left_kind = StringKind::of(&left_type);
        let right_kind = StringKind::of(&right_type);
        let left_val = self.expression(left)?;
        let right_val = self.expression(right)?;
        if left_kind.is_none() || right_kind.is_none() {
            ensure!(
                matches!(op, CompareOp::Eq | CompareOp::Ne)
                    && (left_kind.is_some()
                        || matches!(left_type, HirType::Number | HirType::Boolean))
                    && (right_kind.is_some()
                        || matches!(right_type, HirType::Number | HirType::Boolean)),
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
        ensure!(
            matches!(
                op,
                CompareOp::Eq | CompareOp::Ne | CompareOp::LooseEq | CompareOp::LooseNe
            ) || (left_kind == Some(StringKind::Present)
                && right_kind == Some(StringKind::Present)),
            "Ordering string-or-undefined values requires an unsupported coercion"
        );
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
            "charAt" | "codePointAt" => 0..=1,
            "indexOf" => 1..=2,
            "toLowerCase" | "toUpperCase" => 0..=0,
            "split" => 1..=1,
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
        let desc = self.string_receiver(receiver)?;
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
            "codePointAt" => {
                let position = self.position_argument(args.first(), 0.0)?;
                (
                    helpers
                        .str_code_point_at
                        .expect("codePointAt helper available"),
                    vec![desc, position],
                    Type::F64,
                )
            }
            "toLowerCase" => {
                let zero = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
                (
                    helpers
                        .str_case_convert
                        .expect("case_convert helper available"),
                    vec![desc, zero],
                    Type::I32,
                )
            }
            "toUpperCase" => {
                let one = self.op(Operator::I32Const { value: 1 }, &[], &[Type::I32]);
                (
                    helpers
                        .str_case_convert
                        .expect("case_convert helper available"),
                    vec![desc, one],
                    Type::I32,
                )
            }
            "split" => {
                ensure!(
                    self.is_string(&args[0]),
                    "split requires a string separator operand"
                );
                let sep = self.string_receiver(&args[0])?;
                (
                    helpers.str_split.expect("split helper available"),
                    vec![desc, sep],
                    Type::I32,
                )
            }
            "indexOf" => {
                ensure!(
                    self.is_string(&args[0]),
                    "indexOf requires a string search operand"
                );
                let search = self.string_receiver(&args[0])?;
                let position = self.position_argument(args.get(1), 0.0)?;
                (
                    helpers.str_index_of.expect("indexOf helper available"),
                    vec![desc, search, position],
                    Type::F64,
                )
            }
            _ => unreachable!("Method checked above"),
        };
        Ok(self.op(Operator::Call { function_index }, &values, &[result_type]))
    }

    /// Lowers Array.prototype.join for array of string descriptors.
    pub(super) fn array_join(&mut self, receiver: &Expr, args: &[Expr]) -> Result<Value> {
        let helpers = self
            .registry
            .string_helpers
            .expect("String runtime is registered");
        let arr_ptr = self.expression(receiver)?;
        let sep_desc = if let Some(arg) = args.first() {
            ensure!(
                self.is_string(arg),
                "join requires a string separator operand"
            );
            self.string_receiver(arg)?
        } else {
            let comma_offset = self
                .string_pool
                .get(",")
                .expect("Comma string literal is pre-interned");
            self.op(
                Operator::I32Const {
                    value: comma_offset,
                },
                &[],
                &[Type::I32],
            )
        };
        Ok(self.op(
            Operator::Call {
                function_index: helpers.str_join.expect("join helper available"),
            },
            &[arr_ptr, sep_desc],
            &[Type::I32],
        ))
    }

    /// Supplies omitted/undefined defaults without silently coercing other argument types.
    pub(super) fn position_argument(&mut self, arg: Option<&Expr>, default: f64) -> Result<Value> {
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
