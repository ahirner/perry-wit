//! Number-or-undefined values use NaN only when their logical type is optional.

use anyhow::{Result, ensure};
use perry_hir::{
    ir::{CompareOp, Expr},
    types::Type as HirType,
};
use waffle::{Operator, Type, Value};

use super::{FunctionLowerer, types::StringKind};

impl FunctionLowerer<'_> {
    pub(super) fn optional_number_comparison(
        &mut self,
        op: CompareOp,
        left: &Expr,
        right: &Expr,
    ) -> Result<Value> {
        let left_number =
            self.is_optional_number(left) || self.infer_expr_type(left) == HirType::Number;
        let right_number =
            self.is_optional_number(right) || self.infer_expr_type(right) == HirType::Number;
        let left_value = self.expression(left)?;
        let right_value = self.expression(right)?;
        let equality = matches!(
            op,
            CompareOp::Eq | CompareOp::Ne | CompareOp::LooseEq | CompareOp::LooseNe
        );
        ensure!(
            (left_number && right_number)
                || matches!(op, CompareOp::Eq | CompareOp::Ne)
                || (equality
                    && (self.infer_expr_type(left) == HirType::Void
                        || self.infer_expr_type(right) == HirType::Void)),
            "Unsupported mixed number-or-undefined comparison"
        );
        if !equality {
            let operator = match op {
                CompareOp::Lt => Operator::F64Lt,
                CompareOp::Le => Operator::F64Le,
                CompareOp::Gt => Operator::F64Gt,
                CompareOp::Ge => Operator::F64Ge,
                _ => unreachable!(),
            };
            return Ok(self.op(operator, &[left_value, right_value], &[Type::I32]));
        }
        let left_missing = self.is_undefined_value(left, left_value)?;
        let right_missing = self.is_undefined_value(right, right_value)?;
        let mut equal = self.op(
            Operator::I32And,
            &[left_missing, right_missing],
            &[Type::I32],
        );
        if left_number && right_number {
            let numeric_equal = self.op(Operator::F64Eq, &[left_value, right_value], &[Type::I32]);
            equal = self.op(Operator::I32Or, &[equal, numeric_equal], &[Type::I32]);
        }
        if matches!(op, CompareOp::Ne | CompareOp::LooseNe) {
            equal = self.op(Operator::I32Eqz, &[equal], &[Type::I32]);
        }
        Ok(equal)
    }

    fn is_undefined_value(&mut self, expr: &Expr, value: Value) -> Result<Value> {
        if self.is_optional_number(expr) {
            return Ok(self.op(Operator::F64Ne, &[value, value], &[Type::I32]));
        }
        let ty = self.infer_expr_type(expr);
        if StringKind::of(&ty) == Some(StringKind::Optional) {
            return Ok(self.op(Operator::I32Eqz, &[value], &[Type::I32]));
        }
        ensure!(
            matches!(
                ty,
                HirType::Number | HirType::Boolean | HirType::String | HirType::Void
            ),
            "Unsupported optional comparison operand: {ty:?}"
        );
        Ok(self.op(
            Operator::I32Const {
                value: u32::from(ty == HirType::Void),
            },
            &[],
            &[Type::I32],
        ))
    }
}
