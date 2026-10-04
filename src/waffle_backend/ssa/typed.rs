//! Structural checks for values used by resolved WIT implementations.

use super::{FunctionLowerer, options::literal_properties};
use anyhow::{Result, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};

impl FunctionLowerer<'_> {
    pub(super) fn typed_operand(
        &mut self,
        expression: &Expr,
        expected: &HirType,
    ) -> Result<waffle::Value> {
        self.check_typed_value(expression, expected)?;
        if crate::waffle_backend::values::is_boxed_union(expected) {
            return self.value_operand(expression);
        }
        if let Some(inner) = crate::waffle_backend::nullable::inner(expected) {
            let actual = self.infer_expr_type(expression);
            if crate::waffle_backend::nullable::inner(&actual).is_some() {
                return self.expression(expression);
            }
            if matches!(actual, HirType::Null | HirType::Void) {
                return self.value_operand(expression);
            }
            let value = self.typed_operand(expression, inner)?;
            return self.box_typed_value(value, inner);
        }
        if let HirType::Union(variants) = expected {
            if let Ok(Some(fields)) = literal_properties(self.contract, expression)
                && let Some((_, Expr::Bool(ok))) = fields.iter().find(|(name, _)| name == "ok")
                && let Some(variant) = result_variant(variants, *ok)
            {
                return self.typed_operand(expression, variant);
            }
            if let Some(variant) = variants
                .iter()
                .find(|ty| self.matches_typed_value(expression, ty))
            {
                return self.typed_operand(expression, variant);
            }
        }
        match (expression, expected) {
            (Expr::Array(items), HirType::Array(inner)) if **inner == HirType::String => {
                self.new_string_array(items)
            }
            (Expr::Array(items), HirType::Array(inner)) => {
                self.new_value_array(items, Some(&vec![(**inner).clone(); items.len()]))
            }
            (Expr::Array(items), HirType::Tuple(types)) => self.new_value_array(items, Some(types)),
            (_, HirType::Object(record))
                if literal_properties(self.contract, expression)?.is_some() =>
            {
                self.new_object(expression, Some(record))
            }
            _ => self.expression(expression),
        }
    }

    pub(super) fn check_typed_value(&self, expression: &Expr, expected: &HirType) -> Result<()> {
        ensure!(
            self.matches_typed_value(expression, expected),
            "Value does not match its declared static type {expected:?}; found {:?}",
            self.infer_expr_type(expression)
        );
        Ok(())
    }

    fn matches_typed_value(&self, expression: &Expr, expected: &HirType) -> bool {
        let actual = self.infer_expr_type(expression);
        if crate::waffle_backend::wit::same_type(&actual, expected) {
            return true;
        }
        if let HirType::Union(variants) = expected {
            if let Ok(Some(fields)) = literal_properties(self.contract, expression)
                && let Some((_, Expr::Bool(ok))) = fields.iter().find(|(name, _)| name == "ok")
                && let Some(variant) = result_variant(variants, *ok)
            {
                return self.matches_typed_value(expression, variant);
            }
            return variants
                .iter()
                .any(|ty| self.matches_typed_value(expression, ty));
        }
        match (expression, expected) {
            (Expr::Array(items), HirType::Array(inner)) => items
                .iter()
                .all(|item| self.matches_typed_value(item, inner)),
            (Expr::String(value), HirType::StringLiteral(expected)) => value == expected,
            (Expr::Array(items), HirType::Tuple(types)) => {
                items.len() == types.len()
                    && items
                        .iter()
                        .zip(types)
                        .all(|(item, ty)| self.matches_typed_value(item, ty))
            }
            (_, HirType::Object(expected)) => {
                if let Ok(Some(fields)) = literal_properties(self.contract, expression) {
                    return expected.properties.iter().all(|(name, field)| {
                        fields
                            .iter()
                            .find(|(key, _)| key == name)
                            .map_or(field.optional, |(_, value)| {
                                self.matches_typed_value(value, &field.ty)
                            })
                    }) && fields
                        .iter()
                        .all(|(name, _)| expected.properties.contains_key(name));
                }
                if let HirType::Object(actual) = actual {
                    return expected.properties.iter().all(|(name, field)| {
                        actual.properties.get(name).is_some_and(|found| {
                            crate::waffle_backend::wit::same_type(&found.ty, &field.ty)
                                && (!found.optional || field.optional)
                        })
                    });
                }
                false
            }
            (_, HirType::String) => crate::waffle_backend::values::is_string_type(&actual),
            _ => false,
        }
    }
}

impl FunctionLowerer<'_> {
    pub(super) fn narrow_declared_union(&mut self, expression: &Expr, truth: bool) {
        use perry_hir::ir::{CompareOp, LogicalOp, UnaryOp};
        if let Expr::Unary {
            op: UnaryOp::Not,
            operand,
        } = expression
        {
            self.narrow_declared_union(operand, !truth);
            return;
        }
        if let Some(guard) = null_guard(expression)
            && let Some(ty @ HirType::Union(variants)) = self
                .narrowings
                .get(&guard.local)
                .or_else(|| self.local_types.get(&guard.local))
            && variants.len() == 2
            && crate::waffle_backend::values::is_boxed_union(ty)
        {
            let absent = if guard.kind == AbsenceKind::Null {
                HirType::Null
            } else {
                HirType::Void
            };
            if variants.contains(&absent) {
                let selected = variants
                    .iter()
                    .find(|ty| (**ty == absent) == (guard.equal == truth))
                    .unwrap();
                self.narrowings.insert(guard.local, selected.clone());
                return;
            }
        }
        if let Expr::Logical { op, left, right } = expression {
            if (*op == LogicalOp::And && truth) || (*op == LogicalOp::Or && !truth) {
                self.narrow_declared_union(left, truth);
                self.narrow_declared_union(right, truth);
            }
            if let Some(left) = null_guard(left)
                && let Some(right) = null_guard(right)
                && left.local == right.local
                && left.kind != right.kind
                && left.equal == right.equal
                && ((*op == LogicalOp::Or && !truth && left.equal)
                    || (*op == LogicalOp::And && truth && !left.equal))
                && let Some(inner) = self
                    .local_types
                    .get(&left.local)
                    .and_then(crate::waffle_backend::nullable::inner)
            {
                self.narrowings.insert(left.local, inner.clone());
            }
            return;
        }
        let (member, label, equal) = match expression {
            Expr::PropertyGet {
                object, property, ..
            } if property == "ok" => ((object.as_ref(), property.as_str()), None, truth),
            Expr::Compare {
                op: CompareOp::Eq | CompareOp::Ne,
                left,
                right,
            } => {
                let (member, label) = match (left.as_ref(), right.as_ref()) {
                    (
                        Expr::PropertyGet {
                            object, property, ..
                        },
                        Expr::String(label),
                    )
                    | (
                        Expr::String(label),
                        Expr::PropertyGet {
                            object, property, ..
                        },
                    ) => ((object.as_ref(), property.as_str()), Some(label)),
                    _ => return,
                };
                let Expr::Compare { op, .. } = expression else {
                    unreachable!()
                };
                (member, label, (*op == CompareOp::Eq) == truth)
            }
            _ => return,
        };
        let Expr::LocalGet(id) = member.0 else { return };
        let Some(HirType::Union(variants)) =
            self.narrowings.get(id).or_else(|| self.local_types.get(id))
        else {
            return;
        };
        if member.1 == "ok" && label.is_none() {
            if let Some(selected) = result_variant(variants, equal) {
                self.narrowings.insert(*id, selected.clone());
            }
            return;
        }
        let Some(label) = label else {
            return;
        };
        let selected: Vec<_> = variants
            .iter()
            .filter(|ty| {
                let HirType::Object(record) = ty else {
                    return false;
                };
                let Some(field) = record.properties.get(member.1) else {
                    return false;
                };
                matches!(&field.ty, HirType::StringLiteral(found) if (found == label) == equal)
            })
            .cloned()
            .collect();
        if selected.len() == 1 {
            self.narrowings.insert(*id, selected[0].clone());
        }
    }
}

/// Perry erases boolean literal types; result payload fields distinguish the arms.
fn result_variant(variants: &[HirType], ok: bool) -> Option<&HirType> {
    let [HirType::Object(left), HirType::Object(right)] = variants else {
        return None;
    };
    if ![left, right].iter().all(|record| {
        record
            .properties
            .get("ok")
            .is_some_and(|field| field.ty == HirType::Boolean)
    }) {
        return None;
    }
    for (field, success) in [("value", true), ("error", false)] {
        let present = left.properties.contains_key(field);
        if present != right.properties.contains_key(field) {
            return Some(if present == (ok == success) {
                &variants[0]
            } else {
                &variants[1]
            });
        }
    }
    None
}

#[derive(PartialEq)]
enum AbsenceKind {
    Null,
    Undefined,
}

struct NullGuard {
    local: perry_hir::types::LocalId,
    kind: AbsenceKind,
    equal: bool,
}

fn null_guard(expression: &Expr) -> Option<NullGuard> {
    use perry_hir::ir::CompareOp;
    let Expr::Compare {
        op: op @ (CompareOp::Eq | CompareOp::Ne),
        left,
        right,
    } = expression
    else {
        return None;
    };
    let (local, kind) = match (left.as_ref(), right.as_ref()) {
        (Expr::LocalGet(id), Expr::Null) | (Expr::Null, Expr::LocalGet(id)) => {
            (*id, AbsenceKind::Null)
        }
        (Expr::LocalGet(id), Expr::Undefined) | (Expr::Undefined, Expr::LocalGet(id)) => {
            (*id, AbsenceKind::Undefined)
        }
        _ => return None,
    };
    Some(NullGuard {
        local,
        kind,
        equal: *op == CompareOp::Eq,
    })
}
