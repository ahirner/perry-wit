//! Reject literal option syntax whose effects cannot survive frontend lowering.

use anyhow::{Result, bail, ensure};
use perry_parser::swc_ecma_ast as ast;

pub(super) fn validate_plain_options(expression: &ast::Expr, domain: &str) -> Result<()> {
    if let ast::Expr::Object(object) = super::underlying_expression(expression) {
        ensure!(
            !object
                .props
                .iter()
                .any(|property| matches!(property, ast::PropOrSpread::Spread(_))),
            "Spread {domain} options are unsupported"
        );
    }
    validate_properties(expression, domain)
}

pub(super) fn validate_properties(expression: &ast::Expr, domain: &str) -> Result<()> {
    if let ast::Expr::Object(object) = super::underlying_expression(expression) {
        for property in &object.props {
            match property {
                ast::PropOrSpread::Prop(property) => match property.as_ref() {
                    ast::Prop::Shorthand(_) => {}
                    ast::Prop::KeyValue(pair) => {
                        let name = match &pair.key {
                            ast::PropName::Ident(name) => name.sym.as_ref(),
                            ast::PropName::Str(name) => name.value.as_str().unwrap_or(""),
                            _ => {
                                bail!("{domain} options require plain properties with static names")
                            }
                        };
                        ensure!(
                            name != "__proto__",
                            "{domain} option prototypes are unsupported"
                        );
                    }
                    _ => bail!("{domain} options require plain properties with static names"),
                },
                ast::PropOrSpread::Spread(_) => {}
            }
        }
    }
    Ok(())
}
