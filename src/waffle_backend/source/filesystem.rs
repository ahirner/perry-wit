//! Preserve read result types before Perry lowers calls to capability declarations.

use anyhow::Result;
use perry_parser::swc_ecma_ast as ast;
use swc_common::SyntaxContext;

use super::{options::validate_plain_options, underlying_expression};
use crate::waffle_backend::capabilities::{CapabilityOperation, FilesystemOperation};

pub(super) fn specialize(
    operation: CapabilityOperation,
    arguments: &[ast::ExprOrSpread],
    unresolved: SyntaxContext,
) -> Result<CapabilityOperation> {
    let (filesystem, asynchronous) = match operation {
        CapabilityOperation::Filesystem(filesystem) => (filesystem, false),
        CapabilityOperation::FilesystemPromise(filesystem) => (filesystem, true),
        _ => return Ok(operation),
    };
    let specialize = |operation| {
        if asynchronous {
            CapabilityOperation::FilesystemPromise(operation)
        } else {
            CapabilityOperation::Filesystem(operation)
        }
    };
    let index = if filesystem == FilesystemOperation::WriteFile {
        2
    } else {
        1
    };
    let Some(options) = arguments.get(index) else {
        return Ok(operation);
    };
    validate_plain_options(&options.expr, "Filesystem")?;
    if !matches!(
        filesystem,
        FilesystemOperation::ReadBytes
            | FilesystemOperation::ReadText
            | FilesystemOperation::ReadValue
    ) {
        return Ok(operation);
    }
    let mut encoding = underlying_expression(&options.expr);
    if let ast::Expr::Object(object) = encoding {
        let mut selected = None;
        for property in &object.props {
            if let ast::PropOrSpread::Prop(property) = property {
                match property.as_ref() {
                    ast::Prop::KeyValue(pair)
                        if matches!(&pair.key, ast::PropName::Ident(name) if name.sym == "encoding")
                            || matches!(&pair.key, ast::PropName::Str(name) if name.value == *"encoding") =>
                    {
                        selected = Some(property.as_ref());
                    }
                    ast::Prop::Shorthand(name) if name.sym == "encoding" => {
                        selected = Some(property.as_ref());
                    }
                    _ => {}
                }
            }
        }
        encoding = match selected {
            Some(ast::Prop::KeyValue(pair)) => underlying_expression(&pair.value),
            Some(_) => {
                return Ok(specialize(FilesystemOperation::ReadValue));
            }
            None => return Ok(operation),
        };
    }
    let text = match encoding {
        ast::Expr::Lit(ast::Lit::Str(label)) => label.value.as_str().is_some_and(|label| {
            label.eq_ignore_ascii_case("utf8") || label.eq_ignore_ascii_case("utf-8")
        }),
        ast::Expr::Lit(_) => false,
        ast::Expr::Ident(name) if name.sym == "undefined" && name.ctxt == unresolved => false,
        _ => {
            return Ok(specialize(FilesystemOperation::ReadValue));
        }
    };
    Ok(specialize(if text {
        FilesystemOperation::ReadText
    } else {
        FilesystemOperation::ReadBytes
    }))
}
