//! Pure floating-point helper imports.

use perry_hir::ir::{BinaryOp, Expr, Module as HirModule};
use waffle::{Func, FuncDecl, Import, ImportKind, Module, SignatureData, Type};

pub(crate) fn remainder_required(hir: &HirModule) -> bool {
    let mut required = false;
    for function in &hir.functions {
        super::visit::visit_function_expressions(function, &mut |expression| {
            required |= matches!(
                expression,
                Expr::Binary {
                    op: BinaryOp::Mod,
                    ..
                }
            );
        });
    }
    required
}

pub(crate) fn declare_remainder(module: &mut Module<'static>, hir: &HirModule) -> Option<Func> {
    if !remainder_required(hir) {
        return None;
    }
    let signature = module.signatures.push(SignatureData {
        params: vec![Type::F64; 2],
        returns: vec![Type::F64],
    });
    let name = "number_remainder";
    let function = module.funcs.push(FuncDecl::Import(signature, name.into()));
    module.imports.push(Import {
        module: super::link::HELPER_MODULE.into(),
        name: name.into(),
        kind: ImportKind::Func(function),
    });
    Some(function)
}
