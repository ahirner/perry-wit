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
        | Expr::ArrayFilter { array, callback }
        | Expr::ArraySome { array, callback }
        | Expr::ArrayEvery { array, callback } => (
            strip_nullish(expression(array, env)).0,
            Callback::Elements,
            callback.as_mut(),
            None,
        ),
        Expr::ArrayFind { array, callback } | Expr::ArrayFindIndex { array, callback } => (
            strip_nullish(expression(array, env)).0,
            Callback::AllIndices,
            callback.as_mut(),
            None,
        ),
        Expr::ArrayFromMapped {
            iterable, map_fn, ..
        } => {
            let source = strip_nullish(expression(iterable, env)).0;
            let element = shapes::generics::iterated_element(source).unwrap_or(HirType::Any);
            return Some((map_fn.as_mut(), vec![element, HirType::Number]));
        }
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
        // `sort` orders `undefined` elements last without comparing them.
        Callback::Comparator => {
            let (defined, absence) = strip_nullish(*element);
            let defined = with_absences(defined, without_undefined(absence));
            vec![defined.clone(), defined]
        }
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
        Callback::Elements | Callback::AllIndices => {
            let value = if matches!(contract, Callback::AllIndices) {
                with_absences((*element).clone(), Absence::Undefined)
            } else {
                (*element).clone()
            };
            vec![value, HirType::Number, HirType::Array(element)]
        }
        // Replacement callbacks returned their inputs above.
        Callback::Replacement => return None,
    };
    Some((callback, inputs))
}

pub(super) fn bind_callback(expr: &mut Expr, env: &mut FunctionEnv<'_>) {
    if let Some((Expr::Closure { params, body, .. }, inputs)) = inputs(expr, env) {
        bind_inputs(params, inputs, env);
        walk::scan(body, env, None, None);
    }
}

/// Give every inline closure the result type lowering stores: a callback the result of the inputs
/// its built-in passes, any other closure the result of its typed parameters, and a closure
/// without a value `return` `void`. Returns how many closures it typed; a closure calling another
/// sees that one's result on the next round. Lowering reports a closure whose result stays open.
pub(super) fn results(statements: &mut [Stmt], env: &mut FunctionEnv<'_>) -> usize {
    let mut results = Results { env, settled: 0 };
    let Ok(()) = walk::visit_body(statements, &mut results);
    results.settled
}

struct Results<'e, 'a> {
    env: &'e mut FunctionEnv<'a>,
    settled: usize,
}

impl walk::Visitor for Results<'_, '_> {
    type Error = std::convert::Infallible;

    fn node(&mut self, _: walk::Node<'_>) -> Result<(), Self::Error> {
        Ok(())
    }

    // Locals take their types before the statements that read them. An unannotated local whose
    // binding is still open reads as its initializer here; the binding itself is not rewritten.
    fn body(&mut self, statements: &mut [Stmt]) -> Result<(), Self::Error> {
        walk::scan(statements, self.env, None, None);
        for statement in statements.iter() {
            if let Stmt::Let {
                id,
                ty,
                init: Some(init),
                ..
            } = statement
                && open(ty)
                && self.env.metadata.inferred_bindings.contains(id)
                && self.env.locals.get(id).is_none_or(open)
            {
                let inferred = expression(init, self.env);
                if !open(&inferred) {
                    self.env.locals.insert(*id, inferred);
                }
            }
        }
        for statement in statements {
            self.statement(statement)?;
        }
        Ok(())
    }

    // Parameters bind before the body, so a nested closure sees the enclosing ones; results
    // settle after it, so an enclosing result sees the nested ones.
    fn expression(&mut self, expr: &mut Expr) -> Result<(), Self::Error> {
        if let Some((Expr::Closure { params, .. }, inputs)) = inputs(expr, self.env) {
            bind_inputs(params, inputs, self.env);
        }
        if let Expr::Closure { params, .. } = expr {
            let declared: Vec<_> = params.iter().map(|param| param.ty.clone()).collect();
            bind_inputs(params, declared, self.env);
        }
        walk::visit_expr(expr, self)?;
        if let Some((callback, inputs)) = inputs(expr, self.env)
            && matches!(callback, Expr::Closure { return_type, .. } if open(return_type))
        {
            let ty = result(callback, inputs, self.env);
            self.settled += settle(callback, ty, self.env);
        }
        if let Expr::Closure {
            return_type,
            body,
            is_async,
            ..
        } = expr
            && open(return_type)
        {
            let mut ty = HirType::Any;
            infer_body_return(body, *is_async, &mut ty, self.env, self.env.decls);
            self.settled += settle(expr, ty, self.env);
        }
        Ok(())
    }
}

pub(super) fn open(ty: &HirType) -> bool {
    ty.any(&mut |ty| matches!(ty, HirType::Any | HirType::Unknown | HirType::TypeVar(_)))
}

/// Record `ty` as the closure's result, or `void` when an open body returns no value. Returns 1
/// when the closure's result became known.
fn settle(closure: &mut Expr, ty: HirType, env: &mut FunctionEnv<'_>) -> usize {
    let Expr::Closure {
        return_type, body, ..
    } = closure
    else {
        return 0;
    };
    if !open(&ty) {
        *return_type = ty;
        return 1;
    }
    let mut returned = Vec::new();
    walk::scan(body, env, Some(&mut returned), None);
    if returned.iter().all(|ty| matches!(ty, HirType::Void)) {
        *return_type = HirType::Void;
        return 1;
    }
    0
}
