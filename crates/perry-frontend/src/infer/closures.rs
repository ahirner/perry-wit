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
    bind_inputs(params, [elem_ty, HirType::Number], env);
    let mut returned = Vec::new();
    walk::scan(body, env, Some(&mut returned), None);
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
fn bind_inputs(
    params: &[perry_hir::ir::Param],
    inputs: impl IntoIterator<Item = HirType>,
    env: &mut FunctionEnv<'_>,
) {
    for (param, input) in params.iter().zip(inputs) {
        let ty = if !matches!(param.ty, HirType::Any | HirType::Unknown)
            && !contains_type_var(&param.ty)
        {
            param.ty.clone()
        } else {
            input
        };
        if !matches!(ty, HirType::Any | HirType::Unknown) {
            env.locals.insert(param.id, ty);
        }
    }
}

pub(super) fn bind_array_callback(expr: &Expr, env: &mut FunctionEnv<'_>) {
    use crate::methods::{Call, CallReceiver, Receiver};
    let (array, call) = match expr {
        Expr::Call { callee, args, .. } => {
            let (
                Expr::PropertyGet {
                    object, property, ..
                },
                [Expr::Closure { .. }],
            ) = (&**callee, args.as_slice())
            else {
                return;
            };
            let Some(call) = Call::parse(
                Receiver::Value(Kind::Array),
                hir_compat::method(property).name(),
                args,
            )
            .ok()
            .flatten() else {
                return;
            };
            (object.as_ref(), call)
        }
        _ => {
            let Some((CallReceiver::Expression(array), call)) = Call::expression(expr) else {
                return;
            };
            (array, call)
        }
    };
    let callback = match call {
        Call::ArrayMap(callback)
        | Call::ArrayFlatMap(callback)
        | Call::ArrayFilter(callback)
        | Call::ArrayFind(callback)
        | Call::ArrayFindIndex(callback)
        | Call::ArrayFindLast(callback)
        | Call::ArrayFindLastIndex(callback)
        | Call::ArraySome(callback)
        | Call::ArrayEvery(callback)
        | Call::ArrayForEach(callback) => callback,
        _ => return,
    };
    let Expr::Closure { params, body, .. } = callback else {
        return;
    };
    let HirType::Array(element) = strip_nullish(expression(array, env)).0 else {
        return;
    };
    let third = params.get(2).map(|_| HirType::Array(element.clone()));
    bind_inputs(
        params,
        [*element, HirType::Number].into_iter().chain(third),
        env,
    );
    walk::scan(body, env, None, None);
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
