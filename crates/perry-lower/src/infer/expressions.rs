//! Expression type inference and nullish/type-var helpers for `infer`.
use super::*;

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
        HirType::Any | HirType::Unknown | HirType::Void | HirType::Null | HirType::TypeVar(_) => {
            false
        }
        HirType::Named(name) => !name.starts_with("__AnonShape_"),
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
            then_expr,
            else_expr,
            ..
        } => {
            if matches!(**then_expr, Expr::Undefined | Expr::Null) {
                let absence = if matches!(**then_expr, Expr::Null) {
                    HirType::Null
                } else {
                    HirType::Void
                };
                with_absences(expression(else_expr, env), vec![absence])
            } else if matches!(**else_expr, Expr::Undefined | Expr::Null) {
                let absence = if matches!(**else_expr, Expr::Null) {
                    HirType::Null
                } else {
                    HirType::Void
                };
                with_absences(expression(then_expr, env), vec![absence])
            } else {
                let then_ty = expression(then_expr, env);
                let (_, else_absences) = strip_nullish(expression(else_expr, env));
                with_absences(then_ty, else_absences)
            }
        }
        Expr::Logical {
            op: LogicalOp::Coalesce,
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
        Expr::New {
            class_name, args, ..
        } if class_name == "Map" => {
            if let Some((k, v, _)) = collections::parse_map_type_args(args) {
                HirType::Generic {
                    base: String::from("Map"),
                    type_args: vec![k, v],
                }
            } else if let Some(first) = args.first()
                && let HirType::Array(inner) = strip_nullish(expression(first, env)).0
                && let HirType::Tuple(items) = *inner
                && items.len() == 2
            {
                HirType::Generic {
                    base: String::from("Map"),
                    type_args: items,
                }
            } else {
                perry_hir::infer_expr_type(expr, env)
            }
        }
        Expr::New {
            class_name, args, ..
        } if class_name == "Set" => {
            if let Some((t, _)) = collections::parse_set_type_args(args) {
                HirType::Generic {
                    base: String::from("Set"),
                    type_args: vec![t],
                }
            } else if let Some(first) = args.first()
                && let HirType::Array(inner) = strip_nullish(expression(first, env)).0
            {
                HirType::Generic {
                    base: String::from("Set"),
                    type_args: vec![*inner],
                }
            } else {
                perry_hir::infer_expr_type(expr, env)
            }
        }
        Expr::MapNewFromArray(items) => {
            if let Some((k, v, _)) = collections::parse_map_new_arg(items) {
                return HirType::Generic {
                    base: String::from("Map"),
                    type_args: vec![k, v],
                };
            }
            match strip_nullish(expression(items, env)).0 {
                HirType::Array(inner) => match *inner {
                    HirType::Tuple(tuple) if tuple.len() == 2 => HirType::Generic {
                        base: String::from("Map"),
                        type_args: tuple,
                    },
                    _ => HirType::Any,
                },
                _ => HirType::Any,
            }
        }
        Expr::SetNewFromArray(items) => {
            if let Some((t, _)) = collections::parse_set_new_arg(items) {
                return HirType::Generic {
                    base: String::from("Set"),
                    type_args: vec![t],
                };
            }
            match strip_nullish(expression(items, env)).0 {
                HirType::Array(inner) => HirType::Generic {
                    base: String::from("Set"),
                    type_args: vec![*inner],
                },
                _ => HirType::Any,
            }
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
        Expr::MapKeys(map) | Expr::ArrayKeys(map) => match strip_nullish(expression(map, env)).0 {
            HirType::Generic { base, type_args }
                if matches!(base.as_str(), "Map" | "ReadonlyMap" | "Set" | "ReadonlySet")
                    && !type_args.is_empty() =>
            {
                HirType::Array(Box::new(type_args[0].clone()))
            }
            HirType::Array(_) => HirType::Array(Box::new(HirType::Number)),
            _ => HirType::Any,
        },
        Expr::MapValues(map) | Expr::ArrayValues(map) => {
            match strip_nullish(expression(map, env)).0 {
                HirType::Generic { base, type_args }
                    if matches!(base.as_str(), "Map" | "ReadonlyMap") && type_args.len() == 2 =>
                {
                    HirType::Array(Box::new(type_args[1].clone()))
                }
                HirType::Generic { base, type_args }
                    if matches!(base.as_str(), "Set" | "ReadonlySet") && !type_args.is_empty() =>
                {
                    HirType::Array(Box::new(type_args[0].clone()))
                }
                HirType::Array(elem) => HirType::Array(elem),
                _ => HirType::Any,
            }
        }
        Expr::MapEntries(map) | Expr::ArrayEntries(map) => {
            match strip_nullish(expression(map, env)).0 {
                HirType::Generic { base, type_args }
                    if matches!(base.as_str(), "Map" | "ReadonlyMap") && type_args.len() == 2 =>
                {
                    HirType::Array(Box::new(HirType::Tuple(type_args)))
                }
                HirType::Generic { base, type_args }
                    if matches!(base.as_str(), "Set" | "ReadonlySet") && !type_args.is_empty() =>
                {
                    let t = type_args[0].clone();
                    HirType::Array(Box::new(HirType::Tuple(vec![t.clone(), t])))
                }
                HirType::Array(elem) => {
                    HirType::Array(Box::new(HirType::Tuple(vec![HirType::Number, *elem])))
                }
                _ => HirType::Any,
            }
        }
        Expr::SetValues(set) => match strip_nullish(expression(set, env)).0 {
            HirType::Generic { base, type_args }
                if matches!(base.as_str(), "Set" | "ReadonlySet") && !type_args.is_empty() =>
            {
                HirType::Array(Box::new(type_args[0].clone()))
            }
            HirType::Generic { base, type_args }
                if matches!(base.as_str(), "Map" | "ReadonlyMap") && type_args.len() == 2 =>
            {
                HirType::Array(Box::new(type_args[1].clone()))
            }
            HirType::Array(elem) => HirType::Array(elem),
            _ => HirType::Any,
        },
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
        Expr::Call { callee, args, .. } => match callee.as_ref() {
            Expr::PropertyGet {
                object, property, ..
            } if matches!(property.as_str(), "map" | "flatMap") => match args.first() {
                Some(callback) => {
                    infer_closure_return(object, callback, env, property == "flatMap")
                }
                None => HirType::Any,
            },
            Expr::PropertyGet {
                object, property, ..
            } if matches!(
                property.as_str(),
                "concat" | array_methods::CONCAT | "filter" | "sort" | "toSorted"
            ) =>
            {
                expression(object, env)
            }
            Expr::PropertyGet {
                object, property, ..
            } if property == "find" => match strip_nullish(expression(object, env)).0 {
                HirType::Array(element) => with_absences(*element, vec![HirType::Void]),
                _ => HirType::Any,
            },
            Expr::PropertyGet {
                object, property, ..
            } if matches!(property.as_str(), "reduce" | "reduceRight") => {
                if let Some(init) = args.get(1) {
                    let init_ty = expression(init, env);
                    if !matches!(init_ty, HirType::Any | HirType::Unknown) {
                        return init_ty;
                    }
                }
                match strip_nullish(expression(object, env)).0 {
                    HirType::Array(element) => *element,
                    _ => HirType::Any,
                }
            }
            Expr::PropertyGet { property, .. }
                if matches!(property.as_str(), "findIndex" | "localeCompare") =>
            {
                HirType::Number
            }
            Expr::PropertyGet { property, .. }
                if matches!(
                    property.as_str(),
                    "some" | "every" | "includes" | "has" | "delete" | "is" | "isArray"
                ) =>
            {
                HirType::Boolean
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
            } if matches!(property.as_str(), "set" | "add") => {
                let obj_ty = strip_nullish(expression(object, env)).0;
                if matches!(&obj_ty, HirType::Generic { base, .. } if matches!(base.as_str(), "Map" | "Set"))
                {
                    obj_ty
                } else {
                    perry_hir::infer_expr_type(expr, env)
                }
            }
            Expr::PropertyGet {
                object, property, ..
            } if matches!(property.as_str(), "keys" | "values" | "entries") => {
                match strip_nullish(expression(object, env)).0 {
                    HirType::Generic { base, type_args }
                        if matches!(base.as_str(), "Map" | "ReadonlyMap")
                            && type_args.len() == 2 =>
                    {
                        match property.as_str() {
                            "keys" => HirType::Array(Box::new(type_args[0].clone())),
                            "values" => HirType::Array(Box::new(type_args[1].clone())),
                            _ => HirType::Array(Box::new(HirType::Tuple(type_args))),
                        }
                    }
                    HirType::Generic { base, type_args }
                        if matches!(base.as_str(), "Set" | "ReadonlySet")
                            && !type_args.is_empty() =>
                    {
                        let t = type_args[0].clone();
                        match property.as_str() {
                            "entries" => HirType::Array(Box::new(HirType::Tuple(vec![t.clone(), t]))),
                            _ => HirType::Array(Box::new(t)),
                        }
                    }
                    HirType::Array(elem) => match property.as_str() {
                        "keys" => HirType::Array(Box::new(HirType::Number)),
                        "values" => HirType::Array(elem),
                        _ => HirType::Array(Box::new(HirType::Tuple(vec![
                            HirType::Number,
                            *elem,
                        ]))),
                    },
                    _ => perry_hir::infer_expr_type(expr, env),
                }
            }
            _ => perry_hir::infer_expr_type(expr, env),
        },
        Expr::CallSpread { callee, .. } => match callee.as_ref() {
            Expr::PropertyGet {
                object, property, ..
            } if matches!(
                property.as_str(),
                "concat" | array_methods::CONCAT | "filter" | "sort" | "toSorted"
            ) =>
            {
                expression(object, env)
            }
            _ => perry_hir::infer_expr_type(expr, env),
        },
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
            HirType::Array(element) => *element,
            HirType::Tuple(items) => match index.as_ref() {
                Expr::Number(n) if *n >= 0.0 => {
                    items.get(*n as usize).cloned().unwrap_or(HirType::Any)
                }
                Expr::Integer(n) if *n >= 0 => {
                    items.get(*n as usize).cloned().unwrap_or(HirType::Any)
                }
                _ => perry_hir::infer_expr_type(expr, env),
            },
            HirType::Generic { base, type_args }
                if matches!(base.as_str(), "Map" | "ReadonlyMap") && type_args.len() == 2 =>
            {
                HirType::Tuple(type_args)
            }
            HirType::Generic { base, type_args }
                if matches!(base.as_str(), "Set" | "ReadonlySet") && !type_args.is_empty() =>
            {
                type_args[0].clone()
            }
            other => {
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
        _ => perry_hir::infer_expr_type(expr, env),
    };
    if contains_type_var(&ty) {
        HirType::Any
    } else {
        ty
    }
}
