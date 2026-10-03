//! Preserve call arguments before Perry's specialized lowering drops them.

use std::collections::HashMap;

use perry_parser::Spanned;
use perry_parser::swc_ecma_ast::{
    CallExpr, Callee, Expr, Id, Ident, IdentName, ImportSpecifier, MemberExpr, MemberProp, Module,
    ModuleDecl, ModuleItem,
};
use swc_common::{GLOBALS, Globals, Mark, SyntaxContext};
use swc_ecma_transforms_base::resolver;
use swc_ecma_visit::{VisitMut, VisitMutWith};

pub(super) fn preserve_options(module: &mut Module) {
    GLOBALS.set(&Globals::new(), || {
        module.visit_mut_with(&mut resolver(Mark::new(), Mark::new(), true));
        let mut filesystem_imports = HashMap::new();
        for item in &module.body {
            let ModuleItem::ModuleDecl(ModuleDecl::Import(import)) = item else {
                continue;
            };
            if import.type_only || !matches!(import.src.value.as_str(), Some("fs" | "node:fs")) {
                continue;
            }
            for specifier in &import.specifiers {
                let binding = match specifier {
                    ImportSpecifier::Default(_) | ImportSpecifier::Namespace(_) => {
                        FilesystemImport::Namespace
                    }
                    ImportSpecifier::Named(named)
                        if !named.is_type_only
                            && named.imported.as_ref().map_or_else(
                                || named.local.sym == "unlinkSync",
                                |name| name.atom().as_ref() == "unlinkSync",
                            ) =>
                    {
                        FilesystemImport::Unlink
                    }
                    _ => continue,
                };
                filesystem_imports.insert(specifier.local().to_id(), binding);
            }
        }
        module.visit_mut_with(&mut PreservedCalls { filesystem_imports });
        // Perry lowers lexical names; resolver contexts must not escape GLOBALS.
        module.visit_mut_with(&mut ClearContexts);
    });
}

enum FilesystemImport {
    Namespace,
    Unlink,
}

struct PreservedCalls {
    filesystem_imports: HashMap<Id, FilesystemImport>,
}

impl VisitMut for PreservedCalls {
    fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
        call.visit_mut_children_with(self);
        let Callee::Expr(callee) = &mut call.callee else {
            return;
        };
        let method = match callee.as_ref() {
            Expr::Ident(ident)
                if matches!(
                    self.filesystem_imports.get(&ident.to_id()),
                    Some(FilesystemImport::Unlink)
                ) =>
            {
                "fs_unlink_sync"
            }
            Expr::Member(member)
                if matches!(&member.prop, MemberProp::Ident(property) if property.sym == "unlinkSync")
                    && matches!(member.obj.as_ref(), Expr::Ident(ident)
                        if matches!(self.filesystem_imports.get(&ident.to_id()), Some(FilesystemImport::Namespace))) =>
            {
                "fs_unlink_sync"
            }
            Expr::Ident(ident) if ident.sym == "fetch" => "fetch_request",
            _ => return,
        };
        let span = callee.span();
        **callee = Expr::Member(MemberExpr {
            span,
            obj: Box::new(Expr::Ident(Ident::new(
                "undefined".into(),
                span,
                SyntaxContext::empty(),
            ))),
            prop: MemberProp::Ident(IdentName {
                span,
                sym: method.into(),
            }),
        });
    }
}

struct ClearContexts;

impl VisitMut for ClearContexts {
    fn visit_mut_syntax_context(&mut self, context: &mut SyntaxContext) {
        *context = SyntaxContext::empty();
    }
}
