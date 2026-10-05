//! Timer values keep their static type and identity while native time elapses.

use super::FunctionLowerer;
use crate::waffle_backend::{
    abi,
    capabilities::{CapabilityOperation, ClockOperation},
    promises::{TaskTarget, is_task_outcome},
    resolve::TypedIntrinsic,
};
use anyhow::{Result, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};
use waffle::{Operator, Type, Value};

impl FunctionLowerer<'_> {
    pub(super) fn timer_value<'b>(&self, callee: &'b Expr) -> Option<&'b str> {
        let Expr::ExternFuncRef { name, .. } = callee else {
            return None;
        };
        matches!(
            self.contract.intrinsics.get(name),
            Some(TypedIntrinsic::Capability(CapabilityOperation::Clock(
                ClockOperation::TimeoutValue
            )))
        )
        .then_some(name.as_str())
    }

    pub(super) fn timer_value_type(&self, args: &[Expr]) -> Result<HirType> {
        ensure!(
            args.len() == 2,
            "setTimeout expects a delay and a typed result value"
        );
        ensure!(
            self.infer_expr_type(&args[0]) == HirType::Number,
            "Timer delay must be a number"
        );
        let ty = if let Expr::Array(items) = &args[1] {
            ensure!(
                !items.is_empty(),
                "An empty timer result array requires a declared element type"
            );
            let ty = self.infer_expr_type(&items[0]);
            ensure!(
                items
                    .iter()
                    .all(|item| crate::waffle_backend::wit::same_type(
                        &ty,
                        &self.infer_expr_type(item)
                    )),
                "Timer result array literals must be homogeneous; declare a tuple for distinct element types"
            );
            HirType::Array(Box::new(ty))
        } else {
            self.infer_expr_type(&args[1])
        };
        ensure!(
            is_task_outcome(&ty),
            "Timer results require supported static values; nested Promise results are unsupported"
        );
        Ok(ty)
    }

    pub(super) fn start_timer_value(&mut self, name: &str, args: &[Expr]) -> Result<Value> {
        let ty = self.timer_value_type(args)?;
        let delay = self.expression(&args[0])?;
        let value = self.typed_operand(&args[1], &ty)?;
        let reference = super::types::is_reference(&ty);
        let owner = if reference {
            self.reference_values.insert(value);
            value
        } else {
            self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32])
        };
        let payload = abi::encode_payload(&mut self.body, self.block, Some(value));
        let args = [delay, payload, owner];
        if let Some(record) =
            self.start_task(&TaskTarget::Intrinsic(name.into()), &args, Some(&ty))?
        {
            return Ok(record);
        }
        let payload = self.op(
            Operator::Call {
                function_index: self.registry.intrinsics[name],
            },
            &args,
            &[Type::F64],
        );
        Ok(abi::decode_payload(
            &mut self.body,
            self.block,
            payload,
            ty == HirType::Void
                || crate::waffle_backend::registry::map_type_to_waffle(&ty)? == Type::I32,
        ))
    }
}
