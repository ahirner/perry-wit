//! Resolve Temporal factory bindings before name-based HIR lowering.

use super::{SourceCalls, underlying_expression};
use crate::waffle_backend::time::{TimeConstructor, TimeKind};
use anyhow::{Result, bail, ensure};
use perry_parser::swc_ecma_ast as ast;
use swc_common::SyntaxContext;

impl SourceCalls {
    pub(super) fn rewrite_time_call(&mut self, call: &mut ast::CallExpr) -> Result<bool> {
        let ast::Callee::Expr(callee) = &call.callee else {
            return Ok(false);
        };
        let ast::Expr::Member(method) = underlying_expression(callee) else {
            return Ok(false);
        };
        let ast::Expr::Member(class) = underlying_expression(&method.obj) else {
            return Ok(false);
        };
        if !matches!(underlying_expression(&class.obj), ast::Expr::Ident(name)
            if name.sym == "Temporal" && name.ctxt == self.unresolved)
        {
            return Ok(false);
        }
        let operation = match (member_name(&class.prop), member_name(&method.prop)) {
            (Some("Instant"), Some("from")) => TimeConstructor::InstantFrom,
            (Some("Instant"), Some("fromEpochMilliseconds")) => TimeConstructor::InstantFromMs,
            (Some("PlainDateTime"), Some("from")) => TimeConstructor::PlainFrom,
            (Some("PlainDate"), Some("from")) => TimeConstructor::PlainDateFrom,
            _ => bail!(
                "Unsupported Temporal factory; use Instant.from(string), Instant.fromEpochMilliseconds(number), PlainDateTime.from(string), or PlainDate.from(string)"
            ),
        };
        ensure!(
            call.args.len() == 1 && call.args[0].spread.is_none() && call.type_args.is_none(),
            "{} requires one non-spread argument; options are unsupported",
            operation.name()
        );
        let name = if let Some(name) = self.time_constructors.get(&operation) {
            name.clone()
        } else {
            let name = self.fresh_name();
            self.time_constructors.insert(operation, name.clone());
            name
        };
        call.callee = ast::Callee::Expr(Box::new(ast::Expr::Ident(ast::Ident::new(
            name.into(),
            call.span,
            SyntaxContext::empty(),
        ))));
        Ok(true)
    }

    pub(super) fn rewrite_time_type(&mut self, reference: &mut ast::TsTypeRef) -> Result<()> {
        let ast::TsEntityName::TsQualifiedName(qualified) = &reference.type_name else {
            return Ok(());
        };
        if !matches!(&qualified.left, ast::TsEntityName::Ident(name)
            if name.sym == "Temporal" && name.ctxt == self.unresolved)
        {
            return Ok(());
        }
        let kind = match qualified.right.sym.as_ref() {
            "Instant" => TimeKind::Instant,
            "PlainDateTime" => TimeKind::PlainDateTime,
            "PlainDate" => TimeKind::PlainDate,
            name => bail!("Unsupported Temporal type '{name}'"),
        };
        ensure!(
            reference.type_params.is_none(),
            "Temporal types accept no type parameters"
        );
        reference.type_name = ast::TsEntityName::Ident(ast::Ident::new(
            kind.type_name().into(),
            reference.span,
            SyntaxContext::empty(),
        ));
        Ok(())
    }
}

fn member_name(property: &ast::MemberProp) -> Option<&str> {
    match property {
        ast::MemberProp::Ident(name) => Some(name.sym.as_ref()),
        ast::MemberProp::Computed(key) => match underlying_expression(&key.expr) {
            ast::Expr::Lit(ast::Lit::Str(text)) => text.value.as_str(),
            _ => None,
        },
        _ => None,
    }
}
