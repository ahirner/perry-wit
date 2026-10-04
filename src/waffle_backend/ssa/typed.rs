//! Structural checks for values used by resolved WIT implementations.

use super::{FunctionLowerer, options::literal_properties};
use anyhow::{Result, ensure};
use perry_hir::{ir::Expr, types::Type as HirType};

impl FunctionLowerer<'_> {
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
        if actual == *expected {
            return true;
        }
        if let HirType::Union(variants) = expected {
            if variants.len() == 2
                && variants.iter().all(|ty| matches!(ty, HirType::Object(record) if record.properties.contains_key("ok")))
                && let Ok(Some(fields)) = literal_properties(self.contract, expression)
                && let Some((_, Expr::Bool(ok))) = fields.iter().find(|(name, _)| name == "ok")
            {
                let field = if *ok { "value" } else { "error" };
                if let Some(variant) = variants.iter().find(|ty| matches!(ty, HirType::Object(record) if record.properties.contains_key(field))) {
                    return self.matches_typed_value(expression, variant);
                }
            }
            return variants
                .iter()
                .any(|ty| self.matches_typed_value(expression, ty));
        }
        match (expression, expected) {
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
                            found.ty == field.ty && (!found.optional || field.optional)
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
