//! Preserve decode options before upstream HIR's one-shot specialization discards them.
use super::*;
use perry_parser::swc_ecma_ast as ast;
use swc_ecma_visit::{VisitMut, VisitMutWith};

pub(super) const DECODE: &str = "$$perryDecodeOptions";

pub(super) fn preserve_options(module: &mut ast::Module) -> Result<()> {
    #[derive(Default)]
    struct Options {
        error: Option<anyhow::Error>,
    }
    impl VisitMut for Options {
        fn visit_mut_new_expr(&mut self, new: &mut ast::NewExpr) {
            new.visit_mut_children_with(self);
            if matches!(&*new.callee, ast::Expr::Ident(name) if name.sym == "TextDecoder") {
                let result = normalize_constructor(new);
                if let Err(error) = result {
                    self.error = Some(error);
                }
            }
        }
        fn visit_mut_call_expr(&mut self, call: &mut ast::CallExpr) {
            call.visit_mut_children_with(self);
            if call.args.len() > 1
                && let ast::Callee::Expr(callee) = &mut call.callee
                && let ast::Expr::Member(member) = &mut **callee
                && let ast::MemberProp::Ident(method) = &mut member.prop
                && method.sym == "decode"
            {
                method.sym = DECODE.into();
            }
        }
    }
    let mut options = Options::default();
    module.visit_mut_with(&mut options);
    options.error.map_or(Ok(()), Err)
}

fn normalize_constructor(new: &mut ast::NewExpr) -> Result<()> {
    let Some(args) = new.args.as_mut() else {
        return Ok(());
    };
    ensure!(
        args.len() <= 2 && args.iter().all(|a| a.spread.is_none()),
        "TextDecoder takes a literal UTF-8 label and inline options"
    );
    let Some(options) = args.get_mut(1) else {
        return Ok(());
    };
    if matches!(&*options.expr, ast::Expr::Ident(i) if i.sym == "undefined") {
        return Ok(());
    }
    let ast::Expr::Object(options) = &mut *options.expr else {
        bail!("TextDecoder options must be an inline object");
    };
    let mut names = std::collections::HashSet::new();
    for property in &mut options.props {
        let ast::PropOrSpread::Prop(property) = property else {
            bail!("TextDecoder option spreads are unsupported");
        };
        if let ast::Prop::Shorthand(id) = &**property {
            **property = ast::Prop::KeyValue(ast::KeyValueProp {
                key: ast::PropName::Ident(ast::IdentName::new(id.sym.clone(), id.span)),
                value: Box::new(ast::Expr::Ident(id.clone())),
            });
        }
        let ast::Prop::KeyValue(field) = &**property else {
            bail!("TextDecoder options need ordinary fields");
        };
        let name = match &field.key {
            ast::PropName::Ident(id) => id.sym.as_ref(),
            ast::PropName::Str(text) => text.value.as_str().unwrap_or(""),
            _ => bail!("TextDecoder option keys must be literal"),
        };
        ensure!(
            matches!(name, "fatal" | "ignoreBOM") && names.insert(name.to_owned()),
            "unknown or repeated TextDecoder option {name}"
        );
        ensure!(
            matches!(
                &*field.value,
                ast::Expr::Ident(_) | ast::Expr::Lit(ast::Lit::Bool(_))
            ),
            "TextDecoder constructor flags must be booleans or local bindings"
        );
    }
    Ok(())
}

impl FunctionBuilder<'_> {
    pub(super) fn decode(
        &mut self,
        ctx: &mut Context,
        decoder: Value,
        input: Option<&Expr>,
        options: Option<&Expr>,
    ) -> Result<Value> {
        let bytes = match input {
            None | Some(Expr::Undefined) => {
                let zero = self.number(ctx, 0.0);
                self.intrinsic(ctx, Intrinsic::BytesNew, vec![zero])
                    .expect("bytes")
            }
            Some(input) => self.typed_operand(ctx, input, Kind::Bytes, "a Uint8Array")?,
        };
        let mut stream = self.boolean(ctx, false);
        for (name, value) in self.init_fields(options)? {
            ensure!(
                name == "stream",
                "unsupported TextDecoder.decode option {name}"
            );
            stream = self.condition(ctx, value)?;
        }
        Ok(self
            .intrinsic(ctx, Intrinsic::TextDecode, vec![decoder, bytes, stream])
            .expect("text"))
    }
}
