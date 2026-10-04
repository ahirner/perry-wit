//! Preserve decoder options that Perry's native decoder HIR cannot represent.

use anyhow::{Result, bail, ensure};
use perry_parser::swc_ecma_ast as ast;
use swc_common::SyntaxContext;

use super::SourceCalls;
use crate::waffle_backend::visit::visit_function_expressions;

impl SourceCalls {
    pub(super) fn rewrite_decoder_constructor(&mut self, expression: &mut ast::Expr) -> Result<()> {
        let ast::Expr::New(constructor) = expression else {
            return Ok(());
        };
        let mut callee = constructor.callee.as_ref();
        loop {
            callee = match callee {
                ast::Expr::Paren(value) => &value.expr,
                ast::Expr::TsAs(value) => &value.expr,
                ast::Expr::TsNonNull(value) => &value.expr,
                ast::Expr::TsTypeAssertion(value) => &value.expr,
                _ => break,
            };
        }
        let builtin = match callee {
            ast::Expr::Ident(name) => name.sym == "TextDecoder" && name.ctxt == self.unresolved,
            ast::Expr::Member(member) if matches!(member.obj.as_ref(), ast::Expr::Ident(name) if name.sym == "globalThis" && name.ctxt == self.unresolved) => {
                matches!(&member.prop, ast::MemberProp::Ident(name) if name.sym == "TextDecoder")
                    || matches!(&member.prop, ast::MemberProp::Computed(key) if matches!(key.expr.as_ref(), ast::Expr::Lit(ast::Lit::Str(name)) if name.value.as_str() == Some("TextDecoder")))
            }
            _ => false,
        };
        if !builtin {
            return Ok(());
        }
        let arguments = constructor.args.as_deref().unwrap_or_default();
        ensure!(
            arguments.len() <= 2 && constructor.type_args.is_none(),
            "TextDecoder construction accepts a label and options"
        );
        ensure!(
            arguments.iter().all(|argument| argument.spread.is_none()),
            "Spread TextDecoder arguments are unsupported"
        );
        if let Some(options) = arguments.get(1) {
            validate_options(&options.expr)?;
        }
        let name = if let Some(name) = &self.decoder_constructor {
            name.clone()
        } else {
            let name = self.fresh_name();
            self.decoder_constructor = Some(name.clone());
            name
        };
        *expression = ast::Expr::Call(ast::CallExpr {
            span: constructor.span,
            ctxt: SyntaxContext::empty(),
            callee: ast::Callee::Expr(Box::new(ast::Expr::Ident(ast::Ident::new(
                name.into(),
                constructor.span,
                SyntaxContext::empty(),
            )))),
            args: constructor.args.take().unwrap_or_default(),
            type_args: None,
        });
        Ok(())
    }
}

pub(crate) fn validate_lowering(hir: &perry_hir::ir::Module) -> Result<()> {
    let mut native_decoder = false;
    for function in &hir.functions {
        visit_function_expressions(function, &mut |expression| {
            native_decoder |= matches!(
                expression,
                perry_hir::ir::Expr::TextDecoderNew { .. }
                    | perry_hir::ir::Expr::TextDecoderDecode { .. }
            );
        });
    }
    ensure!(
        !native_decoder,
        "Unsupported TextDecoder syntax would lose decoder options during HIR lowering"
    );
    Ok(())
}

pub(super) fn validate_decode_options(call: &ast::CallExpr) -> Result<()> {
    let ast::Callee::Expr(callee) = &call.callee else {
        return Ok(());
    };
    let ast::Expr::Member(member) = callee.as_ref() else {
        return Ok(());
    };
    let decode = match &member.prop {
        ast::MemberProp::Ident(name) => name.sym == "decode",
        ast::MemberProp::Computed(key) => {
            matches!(key.expr.as_ref(), ast::Expr::Lit(ast::Lit::Str(name)) if name.value == *"decode")
        }
        _ => false,
    };
    if decode {
        ensure!(
            call.args.iter().all(|argument| argument.spread.is_none()),
            "Spread decoder arguments are unsupported"
        );
        if let Some(options) = call.args.get(1) {
            validate_options(&options.expr)?;
        }
    }
    Ok(())
}

fn validate_options(expression: &ast::Expr) -> Result<()> {
    if let ast::Expr::Object(object) = expression {
        for property in &object.props {
            match property {
                ast::PropOrSpread::Prop(property) => match property.as_ref() {
                    ast::Prop::Shorthand(_) => {}
                    ast::Prop::KeyValue(pair) => {
                        let name = match &pair.key {
                            ast::PropName::Ident(name) => name.sym.as_ref(),
                            ast::PropName::Str(name) => name.value.as_str().unwrap_or(""),
                            _ => {
                                bail!("Decoder options require plain properties with static names")
                            }
                        };
                        ensure!(
                            name != "__proto__",
                            "Decoder option prototypes are unsupported"
                        );
                    }
                    _ => bail!("Decoder options require plain properties with static names"),
                },
                _ => bail!("Spread decoder options are unsupported"),
            }
        }
    }
    Ok(())
}
