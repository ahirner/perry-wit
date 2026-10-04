//! Tagged descriptor operations and flow-sensitive narrowing of union locals.

use anyhow::{Result, bail, ensure};
use perry_hir::{
    ir::{CompareOp, Expr, Stmt, UnaryOp},
    types::{LocalId, Type as HirType},
};
use waffle::{BlockTarget, MemoryArg, Operator, Terminator, Type, Value};

use super::FunctionLowerer;
use crate::waffle_backend::{bytes::is_byte_view, text_or_bytes::is_text_or_bytes, visit};

impl FunctionLowerer<'_> {
    pub(super) fn text_or_bytes_operand(&mut self, expression: &Expr) -> Result<Value> {
        let ty = self.infer_expr_type(expression);
        ensure!(
            is_text_or_bytes(&ty) || ty == HirType::String || is_byte_view(&ty),
            "Expected a string or Uint8Array, got {ty:?}"
        );
        let value = self.expression(expression)?;
        Ok(self.tag_text_or_bytes(value, &ty))
    }

    pub(super) fn tag_text_or_bytes(&mut self, value: Value, ty: &HirType) -> Value {
        if is_byte_view(ty) {
            let one = self.op(Operator::I32Const { value: 1 }, &[], &[Type::I32]);
            self.op(Operator::I32Or, &[value, one], &[Type::I32])
        } else {
            value
        }
    }

    pub(super) fn text_or_bytes_parts(&mut self, value: Value) -> (Value, Value) {
        let one = self.op(Operator::I32Const { value: 1 }, &[], &[Type::I32]);
        let mask = self.op(Operator::I32Const { value: !1 }, &[], &[Type::I32]);
        let binary = self.op(Operator::I32And, &[value, one], &[Type::I32]);
        let descriptor = self.op(Operator::I32And, &[value, mask], &[Type::I32]);
        (descriptor, binary)
    }

    pub(super) fn text_or_bytes_length(&mut self, expression: &Expr) -> Result<Value> {
        let value = self.expression(expression)?;
        let (descriptor, binary) = self.text_or_bytes_parts(value);
        let bytes = self.op(Operator::I32Const { value: 4 }, &[], &[Type::I32]);
        let scalars = self.op(Operator::I32Const { value: 8 }, &[], &[Type::I32]);
        let offset = self.op(Operator::Select, &[bytes, scalars, binary], &[Type::I32]);
        let address = self.op(Operator::I32Add, &[descriptor, offset], &[Type::I32]);
        let length = self.op(
            Operator::I32Load {
                memory: MemoryArg {
                    align: 2,
                    offset: 0,
                    memory: self.registry.memory,
                },
            },
            &[address],
            &[Type::I32],
        );
        Ok(self.op(Operator::F64ConvertI32U, &[length], &[Type::F64]))
    }

    pub(super) fn text_or_bytes_truthiness(&mut self, value: Value) -> Value {
        let (descriptor, binary) = self.text_or_bytes_parts(value);
        // Both descriptors have a byte count at offset four. Empty byte views are objects.
        let length = self.op(
            Operator::I32Load {
                memory: MemoryArg {
                    align: 2,
                    offset: 4,
                    memory: self.registry.memory,
                },
            },
            &[descriptor],
            &[Type::I32],
        );
        self.op(Operator::I32Or, &[length, binary], &[Type::I32])
    }

    pub(super) fn type_of(&mut self, expression: &Expr) -> Result<Value> {
        let ty = self.infer_expr_type(expression);
        let value = self.expression(expression)?;
        if crate::waffle_backend::values::is_dynamic(&ty) {
            return self.value_typeof(value);
        }
        if is_text_or_bytes(&ty) {
            let (_, binary) = self.text_or_bytes_parts(value);
            let text = self.expression(&Expr::String("string".into()))?;
            let object = self.expression(&Expr::String("object".into()))?;
            return Ok(self.op(Operator::Select, &[object, text, binary], &[Type::I32]));
        }
        let label = match &ty {
            HirType::String => "string",
            HirType::Number => "number",
            HirType::Boolean => "boolean",
            HirType::Void => "undefined",
            ty if super::types::identity_kind(ty).is_some() => "object",
            _ => bail!("Unsupported typeof operand: {ty:?}"),
        };
        self.expression(&Expr::String(label.into()))
    }

    pub(super) fn text_or_bytes_comparison(
        &mut self,
        op: CompareOp,
        left: &Expr,
        right: &Expr,
    ) -> Result<Value> {
        ensure!(
            matches!(op, CompareOp::Eq | CompareOp::Ne),
            "String-or-byte values support strict equality only; narrow before coercive comparisons"
        );
        let left_ty = self.infer_expr_type(left);
        let left_val = self.expression(left)?;
        let right_ty = self.infer_expr_type(right);
        let right_val = self.expression(right)?;
        let comparable = |ty: &HirType| {
            is_text_or_bytes(ty) || is_byte_view(ty) || super::types::StringKind::of(ty).is_some()
        };
        if !comparable(&left_ty) || !comparable(&right_ty) {
            return Ok(self.op(
                Operator::I32Const {
                    value: u32::from(op == CompareOp::Ne),
                },
                &[],
                &[Type::I32],
            ));
        }
        let left_val = self.tag_text_or_bytes(left_val, &left_ty);
        let right_val = self.tag_text_or_bytes(right_val, &right_ty);
        let (left_desc, left_binary) = self.text_or_bytes_parts(left_val);
        let (right_desc, right_binary) = self.text_or_bytes_parts(right_val);
        let any_binary = self.op(Operator::I32Or, &[left_binary, right_binary], &[Type::I32]);
        let compare_text = self.body.add_block();
        let join = self.body.add_block();
        let result = self.body.add_blockparam(join, Type::I32);
        let identity = self.op(
            if op == CompareOp::Eq {
                Operator::I32Eq
            } else {
                Operator::I32Ne
            },
            &[left_val, right_val],
            &[Type::I32],
        );
        self.body.set_terminator(
            self.block,
            Terminator::CondBr {
                cond: any_binary,
                if_true: BlockTarget {
                    block: join,
                    args: vec![identity],
                },
                if_false: BlockTarget {
                    block: compare_text,
                    args: vec![],
                },
            },
        );
        self.block = compare_text;
        let comparison = self.op(
            Operator::Call {
                function_index: self.registry.string_helpers.unwrap().str_compare,
            },
            &[left_desc, right_desc],
            &[Type::I32],
        );
        let zero = self.op(Operator::I32Const { value: 0 }, &[], &[Type::I32]);
        let equal = self.op(
            if op == CompareOp::Eq {
                Operator::I32Eq
            } else {
                Operator::I32Ne
            },
            &[comparison, zero],
            &[Type::I32],
        );
        self.branch(join, vec![equal]);
        self.block = join;
        Ok(result)
    }

    pub(super) fn narrow_type_guard(&mut self, expression: &Expr, truth: bool) {
        if let Some((id, ty)) = type_guard(expression, truth)
            && self.local_types.get(&id).is_some_and(is_text_or_bytes)
        {
            self.narrowings.insert(id, ty);
        }
    }

    /// Loop backedges and exceptional joins may arrive after any nested assignment.
    pub(super) fn invalidate_narrowings(&mut self, statements: &[Stmt]) {
        visit::visit_statements(statements, &mut |expression| {
            if let Expr::LocalSet(id, _) | Expr::Update { id, .. } = expression {
                self.narrowings.remove(id);
            }
        });
    }
}

fn type_guard(expression: &Expr, truth: bool) -> Option<(LocalId, HirType)> {
    if let Expr::Unary {
        op: UnaryOp::Not,
        operand,
    } = expression
    {
        return type_guard(operand, !truth);
    }
    let Expr::Compare { op, left, right } = expression else {
        return None;
    };
    let equal = match op {
        CompareOp::Eq | CompareOp::LooseEq => truth,
        CompareOp::Ne | CompareOp::LooseNe => !truth,
        _ => return None,
    };
    let (operand, label) = match (left.as_ref(), right.as_ref()) {
        (Expr::TypeOf(operand), Expr::String(label))
        | (Expr::String(label), Expr::TypeOf(operand)) => (operand, label),
        _ => return None,
    };
    let Expr::LocalGet(id) = operand.as_ref() else {
        return None;
    };
    let text = match label.as_str() {
        "string" => equal,
        "object" => !equal,
        _ => return None,
    };
    Some((
        *id,
        if text {
            HirType::String
        } else {
            HirType::Named("Uint8Array".into())
        },
    ))
}
