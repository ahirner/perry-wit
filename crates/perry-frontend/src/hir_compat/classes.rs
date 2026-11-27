//! Class field declarations and constructor assignments preserved by Perry.
use super::{Expr, MemberKey, Stmt, member_assignment};
use perry_hir::ir::{Class, ClassField};
use perry_parser::swc_ecma_ast as ast;
use swc_ecma_visit::{VisitMut, VisitMutWith};

pub(super) fn preserve_field_types(module: &mut ast::Module) {
    struct Fields;
    impl VisitMut for Fields {
        fn visit_mut_class_prop(&mut self, property: &mut ast::ClassProp) {
            property.visit_mut_children_with(self);
            if property.is_optional
                && let Some(mut annotation) = property.type_ann.take()
            {
                let undefined = Box::new(ast::TsType::TsKeywordType(ast::TsKeywordType {
                    span: annotation.span,
                    kind: ast::TsKeywordTypeKind::TsUndefinedKeyword,
                }));
                annotation.type_ann = Box::new(
                    ast::TsUnionType {
                        span: annotation.span,
                        types: vec![annotation.type_ann, undefined],
                    }
                    .into(),
                );
                property.type_ann = Some(annotation);
            }
        }
    }
    module.visit_mut_with(&mut Fields);
}

pub(crate) fn private_field(field: &ClassField) -> bool {
    field.is_private && field.name.starts_with('#')
}

pub(crate) fn direct_constructor(class: &Class) -> bool {
    let Some(ctor) = &class.constructor else {
        return false;
    };
    if ctor.params.len() != class.fields.len() || ctor.body.len() != class.fields.len() {
        return false;
    }
    ctor.body
        .iter()
        .zip(&ctor.params)
        .zip(&class.fields)
        .all(|((stmt, param), field)| {
            field.init.is_none()
                && param.default.is_none()
                && !param.is_rest
                && assigned_parameter(class, stmt, field) == Some(param.id)
        })
}

fn assigned_parameter(class: &Class, stmt: &Stmt, field: &ClassField) -> Option<super::LocalId> {
    let Stmt::Expr(expr) = stmt else { return None };
    let expr = match expr {
        Expr::Sequence(parts) if let [Expr::String(_), assignment] = parts.as_slice() => assignment,
        expr => expr,
    };
    if matches!(expr, Expr::PutValueSet { receiver, .. } if !matches!(receiver.as_ref(), Expr::This))
    {
        return None;
    }
    let (object, MemberKey::Named(name, _), Expr::LocalGet(id)) = member_assignment(expr)? else {
        return None;
    };
    let is_this = match object {
        Expr::PrivateGuard {
            object, class_id, ..
        } if field.is_private && *class_id == class.id => matches!(object.as_ref(), Expr::This),
        expr => matches!(expr, Expr::This),
    };
    (is_this && name == field.name).then_some(*id)
}
