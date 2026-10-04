//! Recover source-order fields from plain literals validated by the frontend.

use anyhow::{Result, ensure};
use perry_hir::ir::Expr;

use crate::waffle_backend::resolve::ResolvedContract;

pub(super) fn literal_properties<'a>(
    contract: &ResolvedContract,
    expression: &'a Expr,
) -> Result<Option<Vec<(String, &'a Expr)>>> {
    Ok(match expression {
        Expr::Object(properties) => Some(
            properties
                .iter()
                .map(|(name, value)| (name.clone(), value))
                .collect(),
        ),
        Expr::New {
            class_name, args, ..
        } if contract.literal_shapes.contains_key(class_name) => {
            let fields = &contract.literal_shapes[class_name];
            ensure!(
                fields.len() == args.len(),
                "Literal object shape does not match its values"
            );
            Some(fields.iter().cloned().zip(args).collect())
        }
        _ => None,
    })
}
