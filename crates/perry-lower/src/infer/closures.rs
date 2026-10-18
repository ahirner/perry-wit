use super::*;

pub(super) fn infer_closure_return(
    elem_ty: HirType,
    callback: &Expr,
    env: &mut FunctionEnv<'_>,
    flat: bool,
) -> HirType {
    let Expr::Closure {
        params,
        return_type,
        body,
        ..
    } = callback
    else {
        return HirType::Any;
    };
    if !matches!(return_type, HirType::Any | HirType::Unknown) && !contains_type_var(return_type) {
        return if flat {
            return_type.clone()
        } else {
            HirType::Array(Box::new(return_type.clone()))
        };
    }
    let mut saved = Vec::new();
    if let Some(first) = params.first() {
        let param_ty = if !matches!(first.ty, HirType::Any | HirType::Unknown)
            && !contains_type_var(&first.ty)
        {
            first.ty.clone()
        } else {
            elem_ty
        };
        if !matches!(param_ty, HirType::Any | HirType::Unknown) {
            let prev = env.locals.insert(first.id, param_ty);
            saved.push((first.id, prev));
        }
    }
    if let Some(second) = params.get(1) {
        let prev = env.locals.insert(second.id, HirType::Number);
        saved.push((second.id, prev));
    }
    let mut returned = Vec::new();
    walk::scan(body, env, &mut ScanMode::Returns(&mut returned));
    for (id, prev) in saved.into_iter().rev() {
        if let Some(prev) = prev {
            env.locals.insert(id, prev);
        } else {
            env.locals.remove(&id);
        }
    }
    let ret = returned
        .into_iter()
        .find(|ty| !matches!(ty, HirType::Any | HirType::Unknown | HirType::Void))
        .unwrap_or(HirType::Any);
    if matches!(ret, HirType::Any | HirType::Unknown) {
        HirType::Any
    } else if flat {
        ret
    } else {
        HirType::Array(Box::new(ret))
    }
}
pub(super) fn infer_array_closure_return(
    array: &Expr,
    callback: &Expr,
    env: &mut FunctionEnv<'_>,
    flat: bool,
) -> HirType {
    let element = match strip_nullish(expression(array, env)).0 {
        HirType::Array(inner) => *inner,
        _ => HirType::Any,
    };
    infer_closure_return(element, callback, env, flat)
}
