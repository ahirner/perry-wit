//! Expression type inference and nullish/type-var helpers for `infer`.
use super::*;
use crate::collections::IteratorMethod;

fn infer_closure_return(
    array: &Expr,
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
    let elem_ty = match strip_nullish(expression(array, env)).0 {
        HirType::Array(inner) => *inner,
        _ => HirType::Any,
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
    let mut body_clone = body.clone();
    scan(&mut body_clone, env, &mut ScanMode::Returns(&mut returned));
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

pub(super) fn is_resolvable_type(ty: &HirType) -> bool {
    match ty {
        HirType::Any
        | HirType::Unknown
        | HirType::Void
        | HirType::Null
        | HirType::Never
        | HirType::TypeVar(_) => false,
        HirType::Named(name) => !hir_compat::is_object_literal(name),
        HirType::Array(inner) | HirType::Promise(inner) => is_resolvable_type(inner),
        HirType::Tuple(items) => items.iter().all(is_resolvable_type),
        HirType::Union(items) => {
            items
                .iter()
                .any(|item| !matches!(item, HirType::Void | HirType::Null))
                && items.iter().all(|item| {
                    matches!(item, HirType::Void | HirType::Null) || is_resolvable_type(item)
                })
        }
        HirType::Generic { base, type_args } => {
            !matches!(base.as_str(), "Map" | "Set" | "Record" if type_args.is_empty())
                && type_args.iter().all(is_resolvable_type)
        }
        HirType::Object(object) => {
            object
                .index_signature
                .as_ref()
                .is_none_or(|v| is_resolvable_type(v))
                && object
                    .properties
                    .values()
                    .all(|p| is_resolvable_type(&p.ty))
        }
        _ => true,
    }
}

pub(super) fn strip_nullish(ty: HirType) -> (HirType, Vec<HirType>) {
    match ty {
        HirType::Null => (HirType::Void, vec![HirType::Null]),
        HirType::Void => (HirType::Void, vec![HirType::Void]),
        HirType::Union(items) => {
            let mut defined = Vec::new();
            let mut absences = Vec::new();
            for item in items {
                if matches!(item, HirType::Null | HirType::Void) {
                    if !absences.contains(&item) {
                        absences.push(item);
                    }
                } else {
                    defined.push(item);
                }
            }
            let inner = match defined.len() {
                0 => HirType::Void,
                1 => defined.swap_remove(0),
                _ => HirType::Union(defined),
            };
            (inner, absences)
        }
        other => (other, Vec::new()),
    }
}

pub(super) fn with_absences(ty: HirType, absences: Vec<HirType>) -> HirType {
    if absences.is_empty() {
        return ty;
    }
    let (inner, mut existing) = strip_nullish(ty);
    for a in absences {
        if !existing.contains(&a) {
            existing.push(a);
        }
    }
    if matches!(inner, HirType::Any | HirType::Unknown | HirType::Void) {
        return inner;
    }
    let mut items = match inner {
        HirType::Union(members) => members,
        other => vec![other],
    };
    items.extend(existing);
    HirType::Union(items)
}

pub(super) fn expression(expr: &Expr, env: &mut FunctionEnv<'_>) -> HirType {
    if let Some(constructor) = env.metadata.constructors.get(expr) {
        use hir_compat::Constructor;
        return match constructor {
            Constructor::Map { types, init } => {
                let types = types.cloned();
                let inferred = init.map(|init| {
                    if let Expr::Array(entries) = init
                        && let Some(Expr::Array(pair)) = entries.first()
                        && let [key, value] = pair.as_slice()
                    {
                        HirType::Array(Box::new(HirType::Tuple(vec![
                            expression(key, env),
                            expression(value, env),
                        ])))
                    } else {
                        strip_nullish(expression(init, env)).0
                    }
                });
                let type_args = match (types, inferred) {
                    (Some((key, value)), _) => vec![key, value],
                    (_, Some(HirType::Array(inner))) => match *inner {
                        HirType::Tuple(items) if items.len() == 2 => items,
                        _ => return HirType::Any,
                    },
                    _ => return HirType::Any,
                };
                HirType::Generic {
                    base: "Map".into(),
                    type_args,
                }
            }
            Constructor::Set { element, init } => {
                let element = element.cloned();
                let inferred = init.map(|init| strip_nullish(expression(init, env)).0);
                let element = match (element, inferred) {
                    (Some(element), _) => element,
                    (_, Some(HirType::Array(element))) => *element,
                    _ => return HirType::Any,
                };
                HirType::Generic {
                    base: "Set".into(),
                    type_args: vec![element],
                }
            }
        };
    }
    let ty = match expr {
        Expr::ScopedTemp { id, value, body } => {
            let val_ty = expression(value, env);
            let prev = if !matches!(val_ty, HirType::Any | HirType::Unknown) {
                env.locals.insert(*id, val_ty)
            } else {
                None
            };
            let out = expression(body, env);
            if let Some(prev) = prev {
                env.locals.insert(*id, prev);
            } else {
                env.locals.remove(id);
            }
            out
        }
        Expr::Conditional {
            condition,
            then_expr,
            else_expr,
        } => {
            let is_undef_eq = matches!(
                &**condition,
                Expr::Compare {
                    op: CompareOp::Eq,
                    left,
                    right
                } if matches!(**left, Expr::Undefined) || matches!(**right, Expr::Undefined)
            );
            let is_undef_ne = matches!(
                &**condition,
                Expr::Compare {
                    op: CompareOp::Ne,
                    left,
                    right
                } if matches!(**left, Expr::Undefined) || matches!(**right, Expr::Undefined)
            );
            if matches!(**then_expr, Expr::Undefined | Expr::Null) {
                let absence = if matches!(**then_expr, Expr::Null) {
                    HirType::Null
                } else {
                    HirType::Void
                };
                let else_ty = expression(else_expr, env);
                if is_undef_eq {
                    let (inner, mut absences) = strip_nullish(else_ty);
                    absences.retain(|a| !matches!(a, HirType::Void));
                    absences.push(absence);
                    with_absences(inner, absences)
                } else {
                    with_absences(else_ty, vec![absence])
                }
            } else if matches!(**else_expr, Expr::Undefined | Expr::Null) {
                let absence = if matches!(**else_expr, Expr::Null) {
                    HirType::Null
                } else {
                    HirType::Void
                };
                let then_ty = expression(then_expr, env);
                if is_undef_ne {
                    let (inner, mut absences) = strip_nullish(then_ty);
                    absences.retain(|a| !matches!(a, HirType::Void));
                    absences.push(absence);
                    with_absences(inner, absences)
                } else {
                    with_absences(then_ty, vec![absence])
                }
            } else {
                let mut then_ty = expression(then_expr, env);
                if is_undef_ne {
                    let (inner, mut absences) = strip_nullish(then_ty);
                    absences.retain(|a| !matches!(a, HirType::Void));
                    then_ty = with_absences(inner, absences);
                }
                let (_, mut else_absences) = strip_nullish(expression(else_expr, env));
                if is_undef_eq {
                    else_absences.retain(|a| !matches!(a, HirType::Void));
                }
                with_absences(then_ty, else_absences)
            }
        }
        Expr::Logical {
            op: LogicalOp::Coalesce | LogicalOp::Or,
            left,
            right,
        } => {
            let (left_inner, _) = strip_nullish(expression(left, env));
            let (right_inner, right_absences) = strip_nullish(expression(right, env));
            let base = if is_resolvable_type(&left_inner) {
                left_inner
            } else {
                right_inner
            };
            with_absences(base, right_absences)
        }
        Expr::Logical {
            op: LogicalOp::And,
            left,
            right,
        } => {
            let (left_inner, mut absences) = strip_nullish(expression(left, env));
            let (right_inner, right_absences) = strip_nullish(expression(right, env));
            for a in right_absences {
                if !absences.contains(&a) {
                    absences.push(a);
                }
            }
            let base = if is_resolvable_type(&right_inner) {
                right_inner
            } else {
                left_inner
            };
            with_absences(base, absences)
        }
        Expr::MapGet { map, .. } => match strip_nullish(expression(map, env)).0 {
            HirType::Generic { base, type_args }
                if matches!(base.as_str(), "Map" | "ReadonlyMap") && type_args.len() == 2 =>
            {
                with_absences(type_args[1].clone(), vec![HirType::Void])
            }
            _ => HirType::Any,
        },
        Expr::MapSet { map, .. } => expression(map, env),
        Expr::SetAdd { set_id, .. } => env.local_type(*set_id).cloned().unwrap_or(HirType::Any),
        Expr::MapHas { .. }
        | Expr::MapDelete { .. }
        | Expr::SetHas { .. }
        | Expr::SetDelete { .. }
        | Expr::ObjectIs(_, _)
        | Expr::ArrayIsArray(_)
        | Expr::In { .. }
        | Expr::Delete(_) => HirType::Boolean,
        Expr::MapSize(_) | Expr::SetSize(_) => HirType::Number,
        Expr::MapKeys(source) | Expr::ArrayKeys(source) => {
            let source = strip_nullish(expression(source, env)).0;
            iterator_view(source, IteratorMethod::Keys).unwrap_or(HirType::Any)
        }
        Expr::MapValues(source) | Expr::ArrayValues(source) | Expr::SetValues(source) => {
            let source = strip_nullish(expression(source, env)).0;
            iterator_view(source, IteratorMethod::Values).unwrap_or(HirType::Any)
        }
        Expr::MapEntries(source) | Expr::ArrayEntries(source) => {
            let source = strip_nullish(expression(source, env)).0;
            iterator_view(source, IteratorMethod::Entries).unwrap_or(HirType::Any)
        }
        Expr::ObjectKeys(_) | Expr::ForInKeys(_) => HirType::Array(Box::new(HirType::String)),
        Expr::ObjectValues(object) => {
            let obj_ty = strip_nullish(expression(object, env)).0;
            shapes::generics::dict_entry_types(env.ctx, env.interfaces, env.type_aliases, &obj_ty)
                .map(|(_, val_ty)| HirType::Array(Box::new(val_ty)))
                .unwrap_or(HirType::Any)
        }
        Expr::ObjectEntries(object) => {
            let obj_ty = strip_nullish(expression(object, env)).0;
            shapes::generics::dict_entry_types(env.ctx, env.interfaces, env.type_aliases, &obj_ty)
                .map(|(key_ty, val_ty)| {
                    HirType::Array(Box::new(HirType::Tuple(vec![key_ty, val_ty])))
                })
                .unwrap_or(HirType::Any)
        }
        Expr::ObjectFromEntries(entries) => match strip_nullish(expression(entries, env)).0 {
            HirType::Array(inner) => match *inner {
                HirType::Tuple(items) if items.len() == 2 => HirType::Generic {
                    base: String::from("Record"),
                    type_args: vec![HirType::String, items[1].clone()],
                },
                _ => HirType::Any,
            },
            _ => HirType::Any,
        },
        Expr::Array(elements) => {
            if let [first, second] = elements.as_slice() {
                let first_ty = expression(first, env);
                let second_ty = expression(second, env);
                if first_ty != second_ty
                    && !matches!(first_ty, HirType::Any | HirType::Unknown)
                    && !matches!(second_ty, HirType::Any | HirType::Unknown)
                {
                    HirType::Tuple(vec![first_ty, second_ty])
                } else {
                    perry_hir::infer_expr_type(expr, env)
                }
            } else {
                perry_hir::infer_expr_type(expr, env)
            }
        }
        Expr::ArrayFrom(source) => {
            if let Expr::Object(props) = source.as_ref()
                && props.iter().any(|(k, _)| k == "length")
            {
                HirType::Array(Box::new(HirType::Any))
            } else {
                let src_ty = strip_nullish(expression(source, env)).0;
                match shapes::generics::iterated_element(src_ty) {
                    Ok(elem) => HirType::Array(Box::new(elem)),
                    Err(_) => HirType::Any,
                }
            }
        }
        Expr::ArrayFromMapped {
            iterable: source,
            map_fn,
            ..
        } => {
            if let Expr::Object(props) = source.as_ref()
                && props.iter().any(|(k, _)| k == "length")
            {
                infer_closure_return(source, map_fn, env, false)
            } else {
                let src_ty = strip_nullish(expression(source, env)).0;
                let elem_ty = shapes::generics::iterated_element(src_ty).unwrap_or(HirType::Any);
                let Expr::Closure {
                    params,
                    return_type,
                    body,
                    ..
                } = map_fn.as_ref()
                else {
                    return HirType::Any;
                };
                if !matches!(return_type, HirType::Any | HirType::Unknown)
                    && !contains_type_var(return_type)
                {
                    return HirType::Array(Box::new(return_type.clone()));
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
                let mut body_clone = body.clone();
                scan(&mut body_clone, env, &mut ScanMode::Returns(&mut returned));
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
                } else {
                    HirType::Array(Box::new(ret))
                }
            }
        }
        Expr::ArrayMap { array, callback } => infer_closure_return(array, callback, env, false),
        Expr::ArrayFlatMap { array, callback } => infer_closure_return(array, callback, env, true),
        Expr::ArrayFilter { array, .. }
        | Expr::ArraySort { array, .. }
        | Expr::ArrayToSorted { array, .. } => expression(array, env),
        Expr::ArrayFind { array, .. } => match strip_nullish(expression(array, env)).0 {
            HirType::Array(element) => with_absences(*element, vec![HirType::Void]),
            _ => HirType::Any,
        },
        Expr::ArrayFindIndex { .. } => HirType::Number,
        Expr::ArraySome { .. } | Expr::ArrayEvery { .. } => HirType::Boolean,
        Expr::StructuredClone { value, .. } => expression(value, env),
        Expr::MapGroupBy { items, key_fn } => {
            let src_ty = strip_nullish(expression(items, env)).0;
            let elem_ty = shapes::generics::iterated_element(src_ty).unwrap_or(HirType::Any);
            let key_ty = match infer_closure_return(items, key_fn, env, true) {
                HirType::Any | HirType::Unknown => HirType::String,
                other => other,
            };
            HirType::Generic {
                base: String::from("Map"),
                type_args: vec![key_ty, HirType::Array(Box::new(elem_ty))],
            }
        }
        Expr::Call { callee, args, .. } => match callee.as_ref() {
            Expr::ExternFuncRef { name, .. } if name == "structuredClone" => args
                .first()
                .map(|arg| expression(arg, env))
                .unwrap_or(HirType::Any),
            Expr::PropertyGet {
                object, property, ..
            } if property == "groupBy"
                && matches!(
                    object.as_ref(),
                    Expr::PropertyGet {
                        object: global,
                        property: class,
                        ..
                    } if matches!(global.as_ref(), Expr::GlobalGet(_)) && class == "Map"
                ) =>
            {
                let [items, callback] = args.as_slice() else {
                    return HirType::Any;
                };
                let src_ty = strip_nullish(expression(items, env)).0;
                let elem_ty = shapes::generics::iterated_element(src_ty).unwrap_or(HirType::Any);
                let key_ty = match infer_closure_return(items, callback, env, true) {
                    HirType::Any | HirType::Unknown => HirType::String,
                    other => other,
                };
                HirType::Generic {
                    base: String::from("Map"),
                    type_args: vec![key_ty, HirType::Array(Box::new(elem_ty))],
                }
            }
            Expr::PropertyGet {
                object, property, ..
            } if let Some(ret_ty) = env.class_method_return_type(object, property) => ret_ty,
            Expr::PropertyGet {
                object, property, ..
            } if matches!(hir_compat::method(property).name(), "map" | "flatMap") => {
                let m = hir_compat::method(property).name();
                match args.first() {
                    Some(callback) => infer_closure_return(object, callback, env, m == "flatMap"),
                    None => HirType::Any,
                }
            }
            Expr::PropertyGet {
                object, property, ..
            } if matches!(
                hir_compat::method(property).name(),
                "concat"
                    | "filter"
                    | "sort"
                    | "toSorted"
                    | "slice"
                    | "reverse"
                    | "toReversed"
                    | "with"
            ) || hir_compat::method(property) == hir_compat::Method::Iteration =>
            {
                expression(object, env)
            }
            Expr::PropertyGet {
                object, property, ..
            } if hir_compat::method(property).name() == "flat" => {
                let zero_depth = matches!(
                    args.first(),
                    Some(Expr::Number(n)) if *n <= 0.0
                ) || matches!(
                    args.first(),
                    Some(Expr::Integer(n)) if *n <= 0
                );
                let obj_ty = strip_nullish(expression(object, env)).0;
                if !zero_depth
                    && let HirType::Array(inner) = &obj_ty
                    && matches!(inner.as_ref(), HirType::Array(_))
                {
                    (**inner).clone()
                } else {
                    obj_ty
                }
            }
            Expr::PropertyGet {
                object, property, ..
            } if matches!(
                hir_compat::method(property).name(),
                "find" | "findLast" | "pop"
            ) =>
            {
                match strip_nullish(expression(object, env)).0 {
                    HirType::Array(element) => with_absences(*element, vec![HirType::Void]),
                    _ => HirType::Any,
                }
            }
            Expr::PropertyGet {
                object, property, ..
            } if hir_compat::method(property).name() == "at" => {
                match strip_nullish(expression(object, env)).0 {
                    HirType::Array(element) => with_absences(*element, vec![HirType::Void]),
                    HirType::String => with_absences(HirType::String, vec![HirType::Void]),
                    _ => HirType::Any,
                }
            }
            Expr::PropertyGet {
                object, property, ..
            } if matches!(
                hir_compat::method(property).name(),
                "reduce" | "reduceRight"
            ) =>
            {
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
                if let Some(init) = args.get(1) {
                    let init_ty = expression(init, env);
                    if is_resolvable_type(&init_ty) {
                        return init_ty;
                    }
                }
                match strip_nullish(expression(object, env)).0 {
                    HirType::Array(element) => *element,
                    _ => HirType::Any,
                }
            }
            Expr::PropertyGet { property, .. }
                if hir_compat::method(property).name() == "forEach" =>
            {
                HirType::Void
            }
            Expr::PropertyGet { property, .. }
                if matches!(
                    hir_compat::method(property).name(),
                    "join"
                        | "replace"
                        | "replaceAll"
                        | "padStart"
                        | "padEnd"
                        | "trim"
                        | "trimStart"
                        | "trimLeft"
                        | "trimEnd"
                        | "trimRight"
                        | "toString"
                        | "normalize"
                ) =>
            {
                HirType::String
            }
            Expr::PropertyGet { property, .. }
                if hir_compat::method(property).name() == "split" =>
            {
                HirType::Array(Box::new(HirType::String))
            }
            Expr::PropertyGet { property, .. }
                if matches!(hir_compat::method(property).name(), "exec" | "match") =>
            {
                with_absences(
                    HirType::Array(Box::new(HirType::String)),
                    vec![HirType::Null],
                )
            }
            Expr::PropertyGet { property, .. }
                if matches!(
                    hir_compat::method(property).name(),
                    "findIndex"
                        | "findLastIndex"
                        | "localeCompare"
                        | "search"
                        | "lastIndexOf"
                        | "indexOf"
                ) =>
            {
                HirType::Number
            }
            Expr::PropertyGet { property, .. }
                if matches!(
                    hir_compat::method(property).name(),
                    "some"
                        | "every"
                        | "includes"
                        | "has"
                        | "delete"
                        | "is"
                        | "isArray"
                        | "test"
                        | "equals"
                        | "isSubsetOf"
                        | "isSupersetOf"
                        | "isDisjointFrom"
                ) =>
            {
                HirType::Boolean
            }
            Expr::PropertyGet { property, .. }
                if matches!(hir_compat::method(property).name(), "until" | "since") =>
            {
                let mut props = HashMap::new();
                for (name, ty) in [
                    ("blank", HirType::Boolean),
                    ("days", HirType::Number),
                    ("months", HirType::Number),
                    ("sign", HirType::Number),
                    ("weeks", HirType::Number),
                    ("years", HirType::Number),
                ] {
                    props.insert(
                        String::from(name),
                        perry_hir::types::PropertyInfo {
                            ty,
                            optional: false,
                            readonly: false,
                        },
                    );
                }
                HirType::Object(perry_hir::types::ObjectType {
                    name: None,
                    properties: props,
                    property_order: None,
                    index_signature: None,
                })
            }
            Expr::PropertyGet {
                object, property, ..
            } if property == "get" => match strip_nullish(expression(object, env)).0 {
                HirType::Generic { base, type_args }
                    if matches!(base.as_str(), "Map" | "ReadonlyMap") && type_args.len() == 2 =>
                {
                    with_absences(type_args[1].clone(), vec![HirType::Void])
                }
                _ => perry_hir::infer_expr_type(expr, env),
            },
            Expr::PropertyGet {
                object, property, ..
            } if matches!(
                hir_compat::method(property).name(),
                "set"
                    | "add"
                    | "subtract"
                    | "union"
                    | "intersection"
                    | "difference"
                    | "symmetricDifference"
            ) =>
            {
                let obj_ty = strip_nullish(expression(object, env)).0;
                if matches!(&obj_ty, HirType::Generic { base, .. } if matches!(base.as_str(), "Map" | "Set"))
                    || matches!(
                        &obj_ty,
                        HirType::Named(name)
                            if matches!(
                                name.as_str(),
                                "Temporal.PlainDate" | "Temporal.PlainDateTime"
                            )
                    )
                {
                    obj_ty
                } else {
                    perry_hir::infer_expr_type(expr, env)
                }
            }
            Expr::PropertyGet {
                object, property, ..
            } if matches!(
                object.as_ref(),
                Expr::PropertyGet { object: namespace, .. }
                    if matches!(
                        namespace.as_ref(),
                        Expr::PropertyGet { object: global, property: name, .. }
                            if matches!(global.as_ref(), Expr::GlobalGet(_)) && name == "Temporal"
                    )
            ) =>
            {
                let Expr::PropertyGet {
                    property: class, ..
                } = object.as_ref()
                else {
                    return perry_hir::infer_expr_type(expr, env);
                };
                match (class.as_str(), property.as_str()) {
                    ("Instant", "from" | "fromEpochMilliseconds") | ("Now", "instant") => {
                        HirType::Named(String::from("Temporal.Instant"))
                    }
                    ("PlainDate", "from") | ("Now", "plainDateISO") => {
                        HirType::Named(String::from("Temporal.PlainDate"))
                    }
                    ("PlainDate", "compare") => HirType::Number,
                    ("PlainDateTime", "from") | ("Now", "plainDateTimeISO") => {
                        HirType::Named(String::from("Temporal.PlainDateTime"))
                    }
                    ("Now", "timeZoneId") => HirType::String,
                    _ => perry_hir::infer_expr_type(expr, env),
                }
            }
            Expr::PropertyGet {
                object, property, ..
            } if let Some(method) = IteratorMethod::parse(hir_compat::method(property).name()) => {
                let source = strip_nullish(expression(object, env)).0;
                iterator_view(source, method)
                    .unwrap_or_else(|| perry_hir::infer_expr_type(expr, env))
            }
            _ => perry_hir::infer_expr_type(expr, env),
        },
        Expr::CallSpread { callee, .. } => match callee.as_ref() {
            Expr::PropertyGet {
                object, property, ..
            } if matches!(
                hir_compat::method(property).name(),
                "concat" | "filter" | "sort" | "toSorted"
            ) =>
            {
                expression(object, env)
            }
            Expr::PropertyGet { property, .. }
                if matches!(hir_compat::method(property).name(), "max" | "min") =>
            {
                HirType::Number
            }
            _ => perry_hir::infer_expr_type(expr, env),
        },
        Expr::PropertyGet {
            object, property, ..
        } if let Expr::ClassRef(class_name) = object.as_ref() => env
            .static_field_type(class_name, property)
            .cloned()
            .or_else(|| {
                env.classes
                    .iter()
                    .find(|c| c.name == *class_name)?
                    .static_fields
                    .iter()
                    .find(|f| f.name == *property)
                    .map(|f| env.normalize_owned(f.ty.clone()))
            })
            .unwrap_or_else(|| perry_hir::infer_expr_type(expr, env)),
        Expr::PropertyGet {
            object, property, ..
        } => match strip_nullish(expression(object, env)).0 {
            HirType::Generic { base, .. }
                if matches!(base.as_str(), "Map" | "ReadonlyMap" | "Set" | "ReadonlySet")
                    && property == "size" =>
            {
                HirType::Number
            }
            HirType::Generic { base, type_args } => shapes::generics::property_type(
                env.ctx,
                env.interfaces,
                env.type_aliases,
                &base,
                &type_args,
                property,
            )
            .or_else(|| env.class_member_type(&base, &type_args, property))
            .or_else(|| {
                shapes::generics::dict_entry_types(
                    env.ctx,
                    env.interfaces,
                    env.type_aliases,
                    &HirType::Generic { base, type_args },
                )
                .map(|(_, val_ty)| with_absences(val_ty, vec![HirType::Void]))
            })
            .unwrap_or_else(|| perry_hir::infer_expr_type(expr, env)),
            HirType::Named(base) => shapes::generics::property_type(
                env.ctx,
                env.interfaces,
                env.type_aliases,
                &base,
                &[],
                property,
            )
            .or_else(|| env.named_property_type(&base, property))
            .or_else(|| {
                shapes::generics::dict_entry_types(
                    env.ctx,
                    env.interfaces,
                    env.type_aliases,
                    &HirType::Named(base),
                )
                .map(|(_, val_ty)| with_absences(val_ty, vec![HirType::Void]))
            })
            .unwrap_or_else(|| perry_hir::infer_expr_type(expr, env)),
            HirType::Object(object) => object
                .properties
                .get(property)
                .map(|p| shapes::generics::optional_property(p.ty.clone(), p.optional))
                .or_else(|| {
                    object
                        .index_signature
                        .as_ref()
                        .map(|v| with_absences((**v).clone(), vec![HirType::Void]))
                })
                .unwrap_or_else(|| perry_hir::infer_expr_type(expr, env)),
            HirType::String | HirType::Array(_) if property == "length" => HirType::Number,
            _ => perry_hir::infer_expr_type(expr, env),
        },
        Expr::IndexGet { object, index } => match strip_nullish(expression(object, env)).0 {
            HirType::Tuple(items) => match index.as_ref() {
                Expr::Number(n) if *n >= 0.0 => {
                    items.into_iter().nth(*n as usize).unwrap_or(HirType::Any)
                }
                Expr::Integer(n) if *n >= 0 => {
                    items.into_iter().nth(*n as usize).unwrap_or(HirType::Any)
                }
                _ => perry_hir::infer_expr_type(expr, env),
            },
            other => match shapes::generics::iterated_element(other) {
                Ok(element) => element,
                Err(other) => {
                    if let Some((_, val_ty)) = shapes::generics::dict_entry_types(
                        env.ctx,
                        env.interfaces,
                        env.type_aliases,
                        &other,
                    ) {
                        with_absences(val_ty, vec![HirType::Void])
                    } else if let HirType::Named(base) = other
                        && let Some(HirType::Array(element)) = env.resolve_foreign_type(&base)
                    {
                        (**element).clone()
                    } else {
                        perry_hir::infer_expr_type(expr, env)
                    }
                }
            },
        },
        Expr::NativeMethodCall {
            module,
            class_name: None,
            object: None,
            method,
            args,
        } if module == "__perry_runtime" && method == "requireObjectCoercible" => args
            .first()
            .map(|a| strip_nullish(expression(a, env)).0)
            .unwrap_or(HirType::Any),
        Expr::ObjectSpread { parts } => {
            let mut props: HashMap<String, perry_hir::types::PropertyInfo> = HashMap::new();
            let mut single_spread_named: Option<String> = None;
            let mut spread_count = 0usize;
            let mut all_spreads_typed = true;
            for (key, val_expr) in parts {
                if let Some(key) = key {
                    let val_ty = expression(val_expr, env);
                    if matches!(val_expr, Expr::Null | Expr::Undefined)
                        && let Some(existing) = props.get(key)
                    {
                        let abs = if matches!(val_expr, Expr::Null) {
                            HirType::Null
                        } else {
                            HirType::Void
                        };
                        let widened = with_absences(existing.ty.clone(), vec![abs]);
                        props.insert(
                            key.clone(),
                            perry_hir::types::PropertyInfo {
                                ty: widened,
                                optional: existing.optional,
                                readonly: false,
                            },
                        );
                    } else {
                        props.insert(
                            key.clone(),
                            perry_hir::types::PropertyInfo {
                                ty: val_ty,
                                optional: false,
                                readonly: false,
                            },
                        );
                    }
                } else {
                    spread_count += 1;
                    let src_ty = strip_nullish(expression(val_expr, env)).0;
                    if spread_count == 1
                        && let HirType::Named(name) = &src_ty
                    {
                        single_spread_named = Some(name.clone());
                    } else {
                        single_spread_named = None;
                    }
                    let resolved_props = shapes::generics::object_properties(
                        env.ctx,
                        env.interfaces,
                        env.type_aliases,
                        &src_ty,
                    )
                    .or_else(|| match src_ty {
                        HirType::Object(obj) => Some(
                            obj.properties
                                .into_iter()
                                .map(|(name, info)| (name, info.ty, info.optional))
                                .collect(),
                        ),
                        _ => None,
                    });
                    if let Some(resolved) = resolved_props {
                        for (name, prop_ty, opt) in resolved {
                            if matches!(prop_ty, HirType::Null | HirType::Void)
                                && let Some(existing) = props.get(&name)
                            {
                                let abs = if matches!(prop_ty, HirType::Null) {
                                    HirType::Null
                                } else {
                                    HirType::Void
                                };
                                let widened = with_absences(existing.ty.clone(), vec![abs]);
                                props.insert(
                                    name,
                                    perry_hir::types::PropertyInfo {
                                        ty: widened,
                                        optional: existing.optional,
                                        readonly: false,
                                    },
                                );
                            } else if opt && let Some(existing) = props.get(&name) {
                                let (inner, prop_abs) = strip_nullish(prop_ty);
                                let (_, mut existing_abs) = strip_nullish(existing.ty.clone());
                                for abs in prop_abs {
                                    if !existing_abs.contains(&abs) {
                                        existing_abs.push(abs);
                                    }
                                }
                                let merged = with_absences(inner, existing_abs);
                                props.insert(
                                    name,
                                    perry_hir::types::PropertyInfo {
                                        ty: merged,
                                        optional: existing.optional,
                                        readonly: false,
                                    },
                                );
                            } else {
                                props.insert(
                                    name,
                                    perry_hir::types::PropertyInfo {
                                        ty: prop_ty,
                                        optional: opt,
                                        readonly: false,
                                    },
                                );
                            }
                        }
                    } else {
                        all_spreads_typed = false;
                    }
                }
            }
            if !all_spreads_typed || props.is_empty() {
                perry_hir::infer_expr_type(expr, env)
            } else {
                if let Some(named) = single_spread_named
                    && let Some(orig) = shapes::generics::object_properties(
                        env.ctx,
                        env.interfaces,
                        env.type_aliases,
                        &HirType::Named(named.clone()),
                    )
                    && orig.len() == props.len()
                    && orig.iter().all(|(k, t, opt)| {
                        props
                            .get(k)
                            .is_some_and(|p| &p.ty == t && p.optional == *opt)
                    })
                {
                    HirType::Named(named)
                } else {
                    HirType::Object(perry_hir::types::ObjectType {
                        name: None,
                        properties: props,
                        property_order: None,
                        index_signature: None,
                    })
                }
            }
        }
        Expr::ObjectRest {
            object,
            exclude_keys,
        } => {
            let src_ty = strip_nullish(expression(object, env)).0;
            if let Some(resolved) = shapes::generics::object_properties(
                env.ctx,
                env.interfaces,
                env.type_aliases,
                &src_ty,
            ) {
                let mut props = HashMap::new();
                for (name, prop_ty, opt) in resolved {
                    if !exclude_keys.contains(&name) {
                        props.insert(
                            name,
                            perry_hir::types::PropertyInfo {
                                ty: prop_ty,
                                optional: opt,
                                readonly: false,
                            },
                        );
                    }
                }
                HirType::Object(perry_hir::types::ObjectType {
                    name: None,
                    properties: props,
                    property_order: None,
                    index_signature: None,
                })
            } else {
                perry_hir::infer_expr_type(expr, env)
            }
        }
        Expr::New {
            class_name,
            args,
            type_args,
            ..
        } => {
            if !type_args.is_empty() {
                HirType::Generic {
                    base: class_name.clone(),
                    type_args: type_args.clone(),
                }
            } else if let Some(class) = env.classes.iter().find(|c| c.name == *class_name)
                && !class.type_params.is_empty()
            {
                let mut inferred_map: HashMap<String, HirType> = HashMap::new();
                if let Some(ctor) = &class.constructor {
                    let params: Vec<(String, HirType)> = ctor
                        .params
                        .iter()
                        .filter_map(|p| match &p.ty {
                            HirType::TypeVar(name) | HirType::Named(name)
                                if class.type_params.iter().any(|tp| tp.name == *name) =>
                            {
                                Some((name.clone(), p.ty.clone()))
                            }
                            _ => None,
                        })
                        .collect();
                    for ((tp_name, _), arg_expr) in params.into_iter().zip(args.iter()) {
                        let arg_ty = expression(arg_expr, env);
                        if is_resolvable_type(&arg_ty) {
                            inferred_map.insert(tp_name, arg_ty);
                        }
                    }
                }
                let type_params: Vec<(String, Option<HirType>)> = class
                    .type_params
                    .iter()
                    .map(|tp| {
                        (
                            tp.name.clone(),
                            tp.default.as_deref().or(tp.constraint.as_deref()).cloned(),
                        )
                    })
                    .collect();
                let mut resolved_args = Vec::with_capacity(type_params.len());
                let mut all_ok = true;
                for (tp_name, fallback) in type_params {
                    if let Some(ty) = inferred_map.remove(&tp_name).or(fallback) {
                        resolved_args.push(ty);
                    } else {
                        all_ok = false;
                        break;
                    }
                }
                if all_ok {
                    HirType::Generic {
                        base: class_name.clone(),
                        type_args: resolved_args,
                    }
                } else {
                    HirType::Named(class_name.clone())
                }
            } else {
                perry_hir::infer_expr_type(expr, env)
            }
        }
        _ => perry_hir::infer_expr_type(expr, env),
    };
    if contains_type_var(&ty) {
        let is_class_type_var = if let Some(HirType::Named(cur_class)) = env.this_type()
            && let Some(class) = env.classes.iter().find(|c| c.name == cur_class)
        {
            match &ty {
                HirType::TypeVar(name) => class.type_params.iter().any(|tp| tp.name == *name),
                _ => false,
            }
        } else {
            false
        };
        if is_class_type_var { ty } else { HirType::Any }
    } else {
        ty
    }
}

/// The type `method` returns on `source`: a live view of a `Map` or `Set`, or a fresh array of
/// indices, elements or `[index, element]` pairs; `None` for other sources.
fn iterator_view(source: HirType, method: IteratorMethod) -> Option<HirType> {
    match source {
        HirType::Generic { base, type_args }
            if matches!(base.as_str(), "Map" | "ReadonlyMap") && type_args.len() == 2 =>
        {
            Some(HirType::Generic {
                base: method.map_view().type_name().into(),
                type_args,
            })
        }
        HirType::Generic {
            base,
            mut type_args,
        } if matches!(base.as_str(), "Set" | "ReadonlySet") && !type_args.is_empty() => {
            type_args.truncate(1);
            Some(HirType::Generic {
                base: method.set_view().type_name().into(),
                type_args,
            })
        }
        HirType::Array(element) => Some(HirType::Array(match method {
            IteratorMethod::Keys => Box::new(HirType::Number),
            IteratorMethod::Values => element,
            IteratorMethod::Entries => Box::new(HirType::Tuple(vec![HirType::Number, *element])),
        })),
        _ => None,
    }
}
