use super::*;
use crate::{
    collections::IteratorMethod,
    methods::{Method, MethodResult},
};

pub(super) fn call(
    expr: &Expr,
    callee: &Expr,
    args: &[Expr],
    env: &mut FunctionEnv<'_>,
) -> HirType {
    if matches!(callee, Expr::ExternFuncRef { name, .. } if name == "structuredClone") {
        return args
            .first()
            .map(|arg| expression(arg, env))
            .unwrap_or(HirType::Any);
    }
    let Expr::PropertyGet {
        object, property, ..
    } = callee
    else {
        return perry_hir::infer_expr_type(expr, env);
    };
    let method = hir_compat::method(property);
    let name = method.name();
    if name == "groupBy"
        && matches!(object.as_ref(),
        Expr::PropertyGet { object: global, property: class, .. }
            if matches!(global.as_ref(), Expr::GlobalGet(_)) && class == "Map")
    {
        let [items, callback] = args else {
            return HirType::Any;
        };
        let source = strip_nullish(expression(items, env)).0;
        let element = shapes::generics::iterated_element(source).unwrap_or(HirType::Any);
        let key = match infer_array_closure_return(items, callback, env, true) {
            HirType::Any | HirType::Unknown => HirType::String,
            other => other,
        };
        return HirType::Generic {
            base: "Map".into(),
            type_args: vec![key, HirType::Array(Box::new(element))],
        };
    }
    if let Some(result) = env.class_method_return_type(object, name) {
        return result;
    }
    if method == hir_compat::Method::Iteration {
        return expression(object, env);
    }
    if let Some(method) = Method::parse(name) {
        return match method.result() {
            MethodResult::Mapped | MethodResult::FlatMapped => {
                args.first().map_or(HirType::Any, |callback| {
                    infer_array_closure_return(
                        object,
                        callback,
                        env,
                        matches!(method.result(), MethodResult::FlatMapped),
                    )
                })
            }
            MethodResult::Receiver => expression(object, env),
            MethodResult::Flat => {
                let zero_depth = matches!(args.first(), Some(Expr::Number(n)) if *n <= 0.0)
                    || matches!(args.first(), Some(Expr::Integer(n)) if *n <= 0);
                match strip_nullish(expression(object, env)).0 {
                    HirType::Array(inner)
                        if !zero_depth && matches!(inner.as_ref(), HirType::Array(_)) =>
                    {
                        *inner
                    }
                    other => other,
                }
            }
            MethodResult::Element | MethodResult::At => {
                match strip_nullish(expression(object, env)).0 {
                    HirType::Array(element) => with_absences(*element, vec![HirType::Void]),
                    HirType::String if method == Method::At => {
                        with_absences(HirType::String, vec![HirType::Void])
                    }
                    _ => HirType::Any,
                }
            }
            MethodResult::Reduced => {
                if let Some(Expr::Closure {
                    params,
                    return_type,
                    ..
                }) = args.first()
                {
                    if let Some(first) = params.first()
                        && !matches!(first.ty, HirType::Any | HirType::Unknown)
                    {
                        return first.ty.clone();
                    }
                    if !matches!(return_type, HirType::Any | HirType::Unknown | HirType::Void) {
                        return return_type.clone();
                    }
                }
                if let Some(initial) = args.get(1) {
                    let ty = expression(initial, env);
                    if is_resolvable_type(&ty) {
                        return ty;
                    }
                }
                match strip_nullish(expression(object, env)).0 {
                    HirType::Array(element) => *element,
                    _ => HirType::Any,
                }
            }
            MethodResult::Void => HirType::Void,
            MethodResult::String => HirType::String,
            MethodResult::Strings => HirType::Array(Box::new(HirType::String)),
            MethodResult::NullableStrings => with_absences(
                HirType::Array(Box::new(HirType::String)),
                vec![HirType::Null],
            ),
            MethodResult::Number => HirType::Number,
            MethodResult::Boolean => HirType::Boolean,
            MethodResult::Duration => HirType::Object(perry_hir::types::ObjectType {
                name: None,
                properties: [
                    ("blank", HirType::Boolean),
                    ("days", HirType::Number),
                    ("months", HirType::Number),
                    ("sign", HirType::Number),
                    ("weeks", HirType::Number),
                    ("years", HirType::Number),
                ]
                .into_iter()
                .map(|(name, ty)| {
                    (
                        name.into(),
                        perry_hir::types::PropertyInfo {
                            ty,
                            optional: false,
                            readonly: false,
                        },
                    )
                })
                .collect(),
                property_order: None,
                index_signature: None,
            }),
            MethodResult::Lookup => match strip_nullish(expression(object, env)).0 {
                HirType::Generic { base, type_args }
                    if matches!(base.as_str(), "Map" | "ReadonlyMap") =>
                {
                    match <[HirType; 2]>::try_from(type_args) {
                        Ok([_, value]) => with_absences(value, vec![HirType::Void]),
                        Err(_) => perry_hir::infer_expr_type(expr, env),
                    }
                }
                _ => perry_hir::infer_expr_type(expr, env),
            },
            MethodResult::Collection => {
                let ty = strip_nullish(expression(object, env)).0;
                if matches!(&ty, HirType::Generic { base, .. } if matches!(base.as_str(), "Map" | "Set"))
                    || matches!(&ty, HirType::Named(name) if matches!(name.as_str(), "Temporal.PlainDate" | "Temporal.PlainDateTime"))
                {
                    ty
                } else {
                    perry_hir::infer_expr_type(expr, env)
                }
            }
        };
    }
    if let Expr::PropertyGet {
        object: namespace,
        property: class,
        ..
    } = object.as_ref()
        && matches!(namespace.as_ref(), Expr::PropertyGet { object: global, property: name, .. }
            if matches!(global.as_ref(), Expr::GlobalGet(_)) && name == "Temporal")
    {
        return match (class.as_str(), property.as_str()) {
            ("Instant", "from" | "fromEpochMilliseconds") | ("Now", "instant") => {
                HirType::Named("Temporal.Instant".into())
            }
            ("PlainDate", "from") | ("Now", "plainDateISO") => {
                HirType::Named("Temporal.PlainDate".into())
            }
            ("PlainDate", "compare") => HirType::Number,
            ("PlainDateTime", "from") | ("Now", "plainDateTimeISO") => {
                HirType::Named("Temporal.PlainDateTime".into())
            }
            ("Now", "timeZoneId") => HirType::String,
            _ => perry_hir::infer_expr_type(expr, env),
        };
    }
    if let Some(method) = IteratorMethod::parse(name) {
        let source = strip_nullish(expression(object, env)).0;
        return iterator_view(source, method)
            .unwrap_or_else(|| perry_hir::infer_expr_type(expr, env));
    }
    perry_hir::infer_expr_type(expr, env)
}
