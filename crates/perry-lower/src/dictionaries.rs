//! Construction and access preserve the declared dictionary value type.
use super::*;
use perry_mir::dictionaries::{AccessOp, Action, NewOp, element};
use perry_parser::swc_ecma_ast as ast;
use pliron::operation::Operation;
use swc_ecma_visit::{VisitMut, VisitMutWith};

#[derive(Default)]
struct IndexSignatures {
    error: Option<&'static str>,
}

impl VisitMut for IndexSignatures {
    fn visit_mut_ts_type_lit(&mut self, literal: &mut ast::TsTypeLit) {
        let indices = literal
            .members
            .iter()
            .filter(|member| matches!(member, ast::TsTypeElement::TsIndexSignature(_)))
            .count();
        if indices > 1 {
            self.error = Some("multiple index signatures are not supported yet");
        }
        if indices != 0
            && literal.members.iter().any(|member| {
                !matches!(
                    member,
                    ast::TsTypeElement::TsIndexSignature(_)
                        | ast::TsTypeElement::TsPropertySignature(_)
                )
            })
        {
            self.error = Some("dictionary methods and accessors are not supported yet");
        }
        // The pinned HIR returns at the index signature while extracting a type literal.
        literal
            .members
            .sort_by_key(|member| matches!(member, ast::TsTypeElement::TsIndexSignature(_)));
        literal.visit_mut_children_with(self);
    }

    fn visit_mut_ts_index_signature(&mut self, signature: &mut ast::TsIndexSignature) {
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
        signature.visit_mut_children_with(self);
    }
}

/// The pinned HIR drops interface index signatures. Equivalent type literals retain them.
pub(super) fn preserve_interface_indices(module: &mut ast::Module) -> Result<()> {
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
            interface.extends.is_empty(),
            "dictionary interface inheritance is not supported yet"
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
    let mut signatures = IndexSignatures::default();
    module.visit_mut_with(&mut signatures);
    if let Some(error) = signatures.error {
        bail!(error);
    }
    Ok(())
}

fn member_type(ctx: &Context, dictionary: Value, key: Value) -> Option<TypeHandle> {
    let constant = Operation::get_op::<StringConstOp>(key.defining_op()?, ctx)?;
    let key: String = constant.get_attr_perry_string_value(ctx)?.clone().into();
    dictionary
        .get_type(ctx)
        .deref(ctx)
        .downcast_ref::<DictionaryType>()?
        .field(&key)
        .map(|field| field.ty)
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
        ty: TypeHandle,
        fields: impl Iterator<Item = (&'e str, &'e Expr)>,
    ) -> Result<Value> {
        let schema = ty
            .deref(ctx)
            .downcast_ref::<DictionaryType>()
            .unwrap()
            .schema()
            .context("opaque dictionary literal")?
            .clone();
        let mut values = Vec::new();
        for (name, expr) in fields {
            ensure!(
                name != "__proto__",
                "prototype setters are outside the closed-shape subset"
            );
            let target = schema
                .fields
                .iter()
                .find(|field| field.name == name)
                .map_or(schema.element, |field| field.ty);
            let value = self.expression_with(ctx, expr, Some(target))?;
            let value = self.fit(ctx, value, target)?;
            let value = self.fit(ctx, value, schema.element)?;
            values.push((name.to_owned(), value));
        }
        for field in &schema.fields {
            ensure!(
                field.optional || values.iter().any(|(name, _)| *name == field.name),
                "dictionary literal is missing required field {}",
                field.name
            );
        }
        Ok(self.emit_value(ctx, |ctx| NewOp::new(ctx, ty, values)))
    }

    pub(super) fn dictionary_get(
        &mut self,
        ctx: &mut Context,
        dictionary: Value,
        key: Value,
    ) -> Result<Value> {
        let target = member_type(ctx, dictionary, key);
        let value = self
            .dictionary_access(ctx, Action::Get, vec![dictionary, key])
            .unwrap();
        match target {
            Some(target) => self.fit(ctx, value, target),
            None => Ok(value),
        }
    }

    pub(super) fn dictionary_set(
        &mut self,
        ctx: &mut Context,
        dictionary: Value,
        key: Value,
        expr: &Expr,
    ) -> Result<Value> {
        let element = element(ctx, dictionary.get_type(ctx)).expect("dictionary operand");
        let target = member_type(ctx, dictionary, key).unwrap_or(element);
        let value = self.expression_with(ctx, expr, Some(target))?;
        let fitted = self.fit(ctx, value, target)?;
        let fitted = self.fit(ctx, fitted, element)?;
        self.dictionary_access(ctx, Action::Set, vec![dictionary, key, fitted]);
        Ok(value)
    }
}
