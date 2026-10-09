//! Construction and access preserve the declared dictionary value type.
use super::*;
use perry_mir::dictionaries::{AccessOp, Action, NewOp, element};
use perry_parser::swc_ecma_ast as ast;
use swc_ecma_visit::{Visit, VisitWith};

#[derive(Default)]
struct IndexSignatures {
    error: Option<&'static str>,
}

impl IndexSignatures {
    fn members(&mut self, members: &[ast::TsTypeElement]) {
        if members.len() != 1
            && members
                .iter()
                .any(|member| matches!(member, ast::TsTypeElement::TsIndexSignature(_)))
        {
            self.error = Some(
                "mixed named fields and index signatures require a constrained dictionary type, which is not supported yet",
            );
        }
    }
}

impl Visit for IndexSignatures {
    fn visit_ts_type_lit(&mut self, literal: &ast::TsTypeLit) {
        self.members(&literal.members);
        literal.visit_children_with(self);
    }

    fn visit_ts_interface_body(&mut self, body: &ast::TsInterfaceBody) {
        self.members(&body.body);
        body.visit_children_with(self);
    }

    fn visit_ts_index_signature(&mut self, signature: &ast::TsIndexSignature) {
        let string_key = match signature.params.as_slice() {
            [ast::TsFnParam::Ident(key)] => key.type_ann.as_ref().is_some_and(|annotation| {
                matches!(annotation.type_ann.as_ref(), ast::TsType::TsKeywordType(keyword)
                    if keyword.kind == ast::TsKeywordTypeKind::TsStringKeyword)
            }),
            _ => false,
        };
        if !string_key || signature.type_ann.is_none() {
            self.error = Some(
                "dictionary index signatures require a string key and an explicit value type (subset rule R1)",
            );
        }
        signature.visit_children_with(self);
    }
}

/// The pinned HIR drops interface index signatures. Equivalent type literals retain them.
pub(super) fn preserve_interface_indices(module: &mut ast::Module) -> Result<()> {
    let mut signatures = IndexSignatures::default();
    module.visit_with(&mut signatures);
    if let Some(error) = signatures.error {
        bail!(error);
    }
    for item in &mut module.body {
        let declaration = match item {
            ast::ModuleItem::Stmt(ast::Stmt::Decl(declaration)) => declaration,
            ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportDecl(export)) => &mut export.decl,
            _ => continue,
        };
        let ast::Decl::TsInterface(interface) = declaration else {
            continue;
        };
        if !interface
            .body
            .body
            .iter()
            .any(|member| matches!(member, ast::TsTypeElement::TsIndexSignature(_)))
        {
            continue;
        }
        ensure!(
            interface.extends.is_empty() && interface.body.body.len() == 1,
            "mixed named fields, inheritance and index signatures require a constrained dictionary type, which is not supported yet"
        );
        *declaration = ast::Decl::TsTypeAlias(Box::new(ast::TsTypeAliasDecl {
            span: interface.span,
            declare: interface.declare,
            id: interface.id.clone(),
            type_params: interface.type_params.take(),
            type_ann: Box::new(ast::TsType::TsTypeLit(ast::TsTypeLit {
                span: interface.body.span,
                members: std::mem::take(&mut interface.body.body),
            })),
        }));
    }
    Ok(())
}

impl FunctionBuilder<'_> {
    pub(super) fn dictionary_access(
        &mut self,
        ctx: &mut Context,
        action: Action,
        args: Vec<Value>,
    ) -> Option<Value> {
        let op = AccessOp::new(ctx, action, args);
        self.append(ctx, &op);
        let op = op.get_operation().deref(ctx);
        (op.get_num_results() == 1).then(|| op.get_result(0))
    }

    pub(super) fn dictionary_literal<'e>(
        &mut self,
        ctx: &mut Context,
        element: TypeHandle,
        fields: impl Iterator<Item = (&'e str, &'e Expr)>,
    ) -> Result<Value> {
        let dictionary = self.emit_value(ctx, |ctx| NewOp::new(ctx, element));
        for (name, expr) in fields {
            ensure!(
                name != "__proto__",
                "prototype setters are outside the closed-shape subset"
            );
            let key = self.emit_value(ctx, |ctx| StringConstOp::new(ctx, name));
            self.dictionary_set(ctx, dictionary, key, expr)?;
        }
        Ok(dictionary)
    }

    pub(super) fn dictionary_set(
        &mut self,
        ctx: &mut Context,
        dictionary: Value,
        key: Value,
        expr: &Expr,
    ) -> Result<Value> {
        let element = element(ctx, dictionary.get_type(ctx)).expect("dictionary operand");
        let value = self.expression_with(ctx, expr, Some(element))?;
        let fitted = self.fit(ctx, value, element)?;
        self.dictionary_access(ctx, Action::Set, vec![dictionary, key, fitted]);
        Ok(value)
    }
}
