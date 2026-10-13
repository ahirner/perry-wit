//! Conservative completion-aware inference for unresolved local helper returns.
use super::*;

use perry_hir::HirTypeFacts;
use std::collections::HashMap;

fn expression(expr: &Expr, env: &impl HirTypeFacts) -> HirType {
    if let Expr::Call { callee, .. } = expr
        && let Expr::PropertyGet {
            object, property, ..
        } = &**callee
        && matches!(property.as_str(), "concat" | array_methods::CONCAT)
    {
        return perry_hir::infer_expr_type(object, env);
    }
    perry_hir::infer_expr_type(expr, env)
}

struct FunctionEnv<'a> {
    base: &'a perry_hir::HirTypeEnv,
    locals: &'a mut HashMap<LocalId, HirType>,
}

impl HirTypeFacts for FunctionEnv<'_> {
    fn local_type(&self, id: LocalId) -> Option<&HirType> {
        self.locals.get(&id).or_else(|| self.base.local_type(id))
    }

    fn global_type(&self, id: perry_hir::types::GlobalId) -> Option<&HirType> {
        self.base.global_type(id)
    }

    fn function_return_type(&self, id: FuncId) -> Option<&HirType> {
        self.base.function_return_type(id)
    }

    fn extern_function_return_type(&self, name: &str) -> Option<&HirType> {
        self.base.extern_function_return_type(name)
    }

    fn this_type(&self) -> Option<HirType> {
        self.base.this_type()
    }

    fn enum_member_type(&self, enum_name: &str, member_name: &str) -> Option<HirType> {
        self.base.enum_member_type(enum_name, member_name)
    }

    fn static_field_type(&self, class_name: &str, field_name: &str) -> Option<&HirType> {
        self.base.static_field_type(class_name, field_name)
    }

    fn static_method_return_type(&self, class_name: &str, method_name: &str) -> Option<&HirType> {
        self.base.static_method_return_type(class_name, method_name)
    }

    fn named_property_type(&self, type_name: &str, property: &str) -> Option<HirType> {
        self.base.named_property_type(type_name, property)
    }

    fn super_property_type(&self, property: &str) -> Option<HirType> {
        self.base.super_property_type(property)
    }

    fn super_method_return_type(&self, method: &str) -> Option<HirType> {
        self.base.super_method_return_type(method)
    }
}

fn scan(
    statements: &mut [Stmt],
    env: &mut FunctionEnv<'_>,
    returned: &mut Vec<HirType>,
    resolve: bool,
) {
    for statement in statements {
        match statement {
            Stmt::Let {
                id,
                ty,
                init: Some(init),
                ..
            } if matches!(ty, HirType::Any | HirType::Unknown) => {
                let inferred = expression(init, env);
                env.locals.insert(*id, inferred.clone());
                if resolve
                    && !matches!(
                        inferred,
                        HirType::Any | HirType::Unknown | HirType::Void | HirType::Null
                    )
                {
                    *ty = inferred;
                }
            }
            Stmt::Return(value) => returned.push(
                value
                    .as_ref()
                    .map_or(HirType::Void, |value| expression(value, env)),
            ),
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                scan(then_branch, env, returned, resolve);
                if let Some(body) = else_branch {
                    scan(body, env, returned, resolve);
                }
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
                scan(body, env, returned, resolve)
            }
            Stmt::For { init, body, .. } => {
                if let Some(init) = init {
                    scan(std::slice::from_mut(init), env, returned, resolve);
                }
                scan(body, env, returned, resolve);
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                scan(body, env, returned, resolve);
                if let Some(catch) = catch {
                    scan(&mut catch.body, env, returned, resolve);
                }
                if let Some(body) = finally {
                    scan(body, env, returned, resolve);
                }
            }
            Stmt::Switch { cases, .. } => {
                for case in cases {
                    scan(&mut case.body, env, returned, resolve);
                }
            }
            Stmt::Labeled { body, .. } => scan(std::slice::from_mut(body), env, returned, resolve),
            _ => {}
        }
    }
}

pub(super) fn unresolved_types(hir: &mut HirModule, locations: &crate::locations::Locations<'_>) {
    let mut locals = HashMap::new();
    let mut module_env = perry_hir::HirTypeEnv::from_module(hir);
    for _ in 0..=hir.functions.len() {
        let mut changed = false;
        for function in &mut hir.functions {
            if locations.explicit_returns.contains(&function.name) {
                continue;
            }
            locals.clear();
            let mut env = FunctionEnv {
                base: &module_env,
                locals: &mut locals,
            };
            let mut returned = Vec::new();
            scan(&mut function.body, &mut env, &mut returned, false);
            if function.is_async {
                for ty in &mut returned {
                    while let HirType::Promise(inner) = ty {
                        *ty = *inner.clone();
                    }
                }
            }
            if returned
                .iter()
                .any(|ty| matches!(ty, HirType::Any | HirType::Unknown))
            {
                continue;
            }
            if array_methods::returns_optional(&function.body) {
                returned.push(HirType::Void);
            }
            let mut types = Vec::new();
            for ty in returned {
                if !types.contains(&ty) {
                    types.push(ty);
                }
            }
            let ty = match types.as_slice() {
                [] => HirType::Void,
                [ty] => ty.clone(),
                _ if types
                    .iter()
                    .filter(|ty| !matches!(ty, HirType::Null | HirType::Void))
                    .count()
                    == 1 =>
                {
                    HirType::Union(types)
                }
                _ => continue,
            };
            let inferred = if function.is_async {
                HirType::Promise(Box::new(ty))
            } else {
                ty
            };
            if inferred != function.return_type {
                function.return_type = inferred;

                changed = true;
            }
        }
        if !changed {
            break;
        }
        module_env = perry_hir::HirTypeEnv::from_module(hir);
    }
    for function in &mut hir.functions {
        locals.clear();
        let mut env = FunctionEnv {
            base: &module_env,
            locals: &mut locals,
        };
        scan(&mut function.body, &mut env, &mut Vec::new(), true);
    }
}
