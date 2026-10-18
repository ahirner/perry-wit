//! Expression type inference and nullish/type-var helpers for `infer`.
use super::*;
use crate::collections::IteratorMethod;

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
            iterable, map_fn, ..
        } => {
            let source = strip_nullish(expression(iterable, env)).0;
            let element = shapes::generics::iterated_element(source).unwrap_or(HirType::Any);
            infer_closure_return(element, map_fn, env, false)
        }
        Expr::ArrayMap { array, callback } | Expr::ArrayFlatMap { array, callback } => {
            infer_array_closure_return(
                array,
                callback,
                env,
                matches!(expr, Expr::ArrayFlatMap { .. }),
            )
        }
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
            let key_ty = match infer_array_closure_return(items, key_fn, env, true) {
                HirType::Any | HirType::Unknown => HirType::String,
                other => other,
            };
            HirType::Generic {
                base: String::from("Map"),
                type_args: vec![key_ty, HirType::Array(Box::new(elem_ty))],
            }
        }
        Expr::Call { callee, args, .. } => methods::call(expr, callee, args, env),
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
            HirType::Tuple(items) => {
                // Only an array index names a tuple position; `t[1.5]` is a dynamic key.
                let position = match index.as_ref() {
                    Expr::Number(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as usize),
                    Expr::Integer(n) => usize::try_from(*n).ok(),
                    _ => None,
                };
                match position {
                    // Reading past the end yields `undefined`.
                    Some(position) => items.into_iter().nth(position).unwrap_or(HirType::Void),
                    None => perry_hir::infer_expr_type(expr, env),
                }
            }
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
                        && let Some(existing) = props.remove(key)
                    {
                        let abs = if matches!(val_expr, Expr::Null) {
                            HirType::Null
                        } else {
                            HirType::Void
                        };
                        let widened = with_absences(existing.ty, vec![abs]);
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
                            let (ty, optional) = match (prop_ty, props.remove(&name)) {
                                (abs @ (HirType::Null | HirType::Void), Some(existing)) => {
                                    (with_absences(existing.ty, vec![abs]), existing.optional)
                                }
                                (ty, Some(existing)) if opt => {
                                    let (inner, prop_abs) = strip_nullish(ty);
                                    let (_, mut existing_abs) = strip_nullish(existing.ty);
                                    for abs in prop_abs {
                                        if !existing_abs.contains(&abs) {
                                            existing_abs.push(abs);
                                        }
                                    }
                                    (with_absences(inner, existing_abs), existing.optional)
                                }
                                (ty, _) => (ty, opt),
                            };
                            props.insert(
                                name,
                                perry_hir::types::PropertyInfo {
                                    ty,
                                    optional,
                                    readonly: false,
                                },
                            );
                        }
                    } else {
                        all_spreads_typed = false;
                    }
                }
            }
            if !all_spreads_typed || props.is_empty() {
                perry_hir::infer_expr_type(expr, env)
            } else {
                if let Some(named) = single_spread_named.map(HirType::Named)
                    && let Some(orig) = shapes::generics::object_properties(
                        env.ctx,
                        env.interfaces,
                        env.type_aliases,
                        &named,
                    )
                    && orig.len() == props.len()
                    && orig.iter().all(|(k, t, opt)| {
                        props
                            .get(k)
                            .is_some_and(|p| &p.ty == t && p.optional == *opt)
                    })
                {
                    named
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
pub(super) fn iterator_view(source: HirType, method: IteratorMethod) -> Option<HirType> {
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
