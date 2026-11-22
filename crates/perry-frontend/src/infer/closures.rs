use super::*;

pub(super) fn result(
    callback: &Expr,
    inputs: impl IntoIterator<Item = HirType>,
    env: &mut FunctionEnv<'_>,
) -> HirType {
    match callback {
        Expr::FuncRef(id) => env
            .function_return_type(*id)
            .cloned()
            .unwrap_or(HirType::Any),
        Expr::Closure {
            params,
            return_type,
            body,
            is_async,
            ..
        } => {
            if !matches!(return_type, HirType::Any | HirType::Unknown)
                && !contains_type_var(return_type)
            {
                return return_type.clone();
            }
            bind_inputs(params, inputs, env);
            let mut result = HirType::Any;
            infer_body_return(body, *is_async, &mut result, env, env.decls);
            result
        }
        _ => HirType::Any,
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

/// Source arguments supplied by each built-in, in JavaScript invocation order.
pub(super) fn inputs<'a>(
    expr: &'a mut Expr,
    env: &mut FunctionEnv<'_>,
) -> Option<(&'a mut Expr, Vec<HirType>)> {
    use crate::methods::{Callback, Method, Receiver};
    let (receiver, contract, callback, initial) = match expr {
        Expr::Call { callee, args, .. } => {
            let Expr::PropertyGet {
                object, property, ..
            } = callee.as_ref()
            else {
                return None;
            };
            if !args
                .iter()
                .take(2)
                .any(|arg| matches!(arg, Expr::FuncRef(_) | Expr::Closure { .. }))
            {
                return None;
            }
            let receiver = strip_nullish(expression(object, env)).0;
            let kind = match receiver {
                HirType::String => Kind::String,
                HirType::Array(_) => Kind::Array,
                _ => return None,
            };
            let contract =
                Method::lookup(Receiver::Value(kind), hir_compat::method(property).name())?
                    .callback()?;
            let (callback, tail) = args.split_first_mut()?;
            if let Callback::Replacement = contract {
                let mut inputs = vec![HirType::String];
                let captures = match callback {
                    Expr::RegExp { pattern, .. } => Some(crate::methods::regex_captures(pattern)),
                    Expr::String(_) => Some((0, false)),
                    _ => None,
                };
                if let Some((count, named)) = captures {
                    inputs.extend(std::iter::repeat_n(
                        with_absences(HirType::String, Absence::Undefined),
                        count,
                    ));
                    inputs.extend([HirType::Number, HirType::String]);
                    if named {
                        inputs.push(HirType::Unknown);
                    }
                }
                return Some((tail.first_mut()?, inputs));
            }

            (receiver, contract, callback, tail.first())
        }
        Expr::ArrayMap { array, callback }
        | Expr::ArrayFlatMap { array, callback }
        | Expr::ArrayFind { array, callback }
        | Expr::ArrayFindIndex { array, callback }
        | Expr::ArrayFilter { array, callback }
        | Expr::ArraySome { array, callback }
        | Expr::ArrayEvery { array, callback } => (
            strip_nullish(expression(array, env)).0,
            Callback::Elements,
            callback.as_mut(),
            None,
        ),
        Expr::ArraySort { array, comparator } => (
            strip_nullish(expression(array, env)).0,
            Callback::Comparator,
            comparator.as_mut(),
            None,
        ),
        Expr::ArrayToSorted {
            array, comparator, ..
        } => (
            strip_nullish(expression(array, env)).0,
            Callback::Comparator,
            comparator.as_deref_mut()?,
            None,
        ),
        _ => return None,
    };
    let HirType::Array(element) = receiver else {
        return None;
    };
    let inputs = match contract {
        Callback::Comparator => vec![(*element).clone(), *element],
        Callback::Fold => {
            let accumulator =
                initial.map_or_else(|| (*element).clone(), |value| expression(value, env));
            vec![
                accumulator,
                (*element).clone(),
                HirType::Number,
                HirType::Array(element),
            ]
        }
        Callback::Elements => vec![(*element).clone(), HirType::Number, HirType::Array(element)],
        Callback::Replacement => unreachable!(),
    };
    Some((callback, inputs))
}

pub(super) fn bind_callback(expr: &mut Expr, env: &mut FunctionEnv<'_>) {
    if let Some((Expr::Closure { params, body, .. }, inputs)) = inputs(expr, env) {
        bind_inputs(params, inputs, env);
        walk::scan(body, env, None, None);
    }
}
