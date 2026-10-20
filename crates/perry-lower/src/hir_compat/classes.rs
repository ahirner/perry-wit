//! Preserve optional class-field types before upstream HIR drops question marks.
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
