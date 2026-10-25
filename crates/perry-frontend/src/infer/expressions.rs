//! Expression type inference and nullish/type-var helpers for `infer`.
use super::*;
use crate::collections::IteratorMethod;

pub(super) fn expression(expr: &Expr, env: &mut FunctionEnv<'_>) -> HirType {
    if let Some(constructor) = env.metadata.constructors.get(expr) {
        use hir_compat::Constructor;
        return match constructor {
            Constructor::Map { types, init } => {
                let types = types.map(|(key, value)| (key.clone(), value.clone()));
                let inferred = init.map(|init| {
                    if let Expr::Array(entries, _) = init
                        && let Some(Expr::Array(pair, _)) = entries.first()
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
    if let Some((receiver, call)) = crate::methods::Call::expression(expr) {
        let local;
        let object = match receiver {
            crate::methods::CallReceiver::Expression(expr) => expr,
            crate::methods::CallReceiver::Local(id) => {
                local = Expr::LocalGet(id);
                &local
            }
        };
        return methods::result(
            expr,
            object,
            |index| call.argument(index),
            call.result(),
            env,
        );
    }
    let ty = match expr {
        Expr::Binary {
            op: perry_hir::ir::BinaryOp::Add,
            left,
            right,
        } => {
            let left = expression(left, env);
            let right = expression(right, env);
            let mixed = |ty: &HirType| {
                let HirType::Union(members) = ty else {
                    return false;
                };
                members.iter().any(|ty| {
                    !matches!(
                        ty,
                        HirType::Number
                            | HirType::Int32
                            | HirType::Boolean
                            | HirType::Null
                            | HirType::Void
                    )
                })
            };
            if matches!(left, HirType::String | HirType::StringLiteral(_))
                || matches!(right, HirType::String | HirType::StringLiteral(_))
            {
                HirType::String
            } else if mixed(&left) || mixed(&right) {
                HirType::Union(vec![HirType::Number, HirType::String])
            } else {
                perry_hir::infer_expr_type(expr, env)
            }
        }
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
            let undefined_check = match &**condition {
                Expr::Compare { op, left, right }
                    if matches!(**left, Expr::Undefined) || matches!(**right, Expr::Undefined) =>
                {
                    Some(op)
                }
                _ => None,
            };
            let absent_branch = if matches!(**then_expr, Expr::Undefined | Expr::Null) {
                Some((
                    then_expr,
                    else_expr,
                    matches!(undefined_check, Some(CompareOp::Eq)),
                ))
            } else if matches!(**else_expr, Expr::Undefined | Expr::Null) {
                Some((
                    else_expr,
                    then_expr,
                    matches!(undefined_check, Some(CompareOp::Ne)),
                ))
            } else {
                None
            };
            if let Some((absent, other, narrowed)) = absent_branch {
                let state = if matches!(**absent, Expr::Null) {
                    Absence::Null
                } else {
                    Absence::Undefined
                };
                let other = expression(other, env);
                let (inner, states) = if narrowed {
                    let (inner, states) = strip_nullish(other);
                    (inner, without_undefined(states))
                } else {
                    (other, None)
                };
                with_absences(inner, merge_absence(states, Some(state)))
            } else {
                let mut then_ty = expression(then_expr, env);
                if matches!(undefined_check, Some(CompareOp::Ne)) {
                    let (inner, states) = strip_nullish(then_ty);
                    then_ty = with_absences(inner, without_undefined(states));
                }
                let (_, states) = strip_nullish(expression(else_expr, env));
                let states = if matches!(undefined_check, Some(CompareOp::Eq)) {
                    without_undefined(states)
                } else {
                    states
                };
                with_absences(then_ty, states)
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
            let (left_inner, absences) = strip_nullish(expression(left, env));
            let (right_inner, right_absences) = strip_nullish(expression(right, env));
            let absences = merge_absence(absences, right_absences);
            let base = if is_resolvable_type(&right_inner) {
                right_inner
            } else {
                left_inner
            };
            with_absences(base, absences)
        }
        Expr::ObjectIs(_, _) | Expr::ArrayIsArray(_) | Expr::In { .. } | Expr::Delete(_) => {
            HirType::Boolean
        }
        Expr::MapSize(_) | Expr::SetSize(_) => HirType::Number,
        Expr::ObjectKeys(_) | Expr::ForInKeys(_) => HirType::Array(Box::new(HirType::String)),
        Expr::ObjectValues(object) => {
            let obj_ty = strip_nullish(expression(object, env)).0;
            env.decls
                .dict_entry_types(env.ctx, &obj_ty)
                .map(|(_, val_ty)| HirType::Array(Box::new(val_ty)))
                .unwrap_or(HirType::Any)
        }
        Expr::ObjectEntries(object) => {
            let obj_ty = strip_nullish(expression(object, env)).0;
            env.decls
                .dict_entry_types(env.ctx, &obj_ty)
                .map(|(key_ty, val_ty)| {
                    HirType::Array(Box::new(HirType::Tuple(vec![key_ty, val_ty])))
                })
                .unwrap_or(HirType::Any)
        }
        Expr::ObjectFromEntries(entries) => match strip_nullish(expression(entries, env)).0 {
            HirType::Array(inner) => match *inner {
                HirType::Tuple(mut items) if items.len() == 2 => HirType::Generic {
                    base: String::from("Record"),
                    type_args: vec![HirType::String, items.swap_remove(1)],
                },
                _ => HirType::Any,
            },
            _ => HirType::Any,
        },
        Expr::Array(elements, _) => {
            let mut types = elements.iter().map(|element| expression(element, env));
            let first = types.next().unwrap_or(HirType::Any);
            let second = types.next();
            match second {
                Some(second)
                    if elements.len() == 2
                        && first != second
                        && !matches!(first, HirType::Any | HirType::Unknown)
                        && !matches!(second, HirType::Any | HirType::Unknown) =>
                {
                    HirType::Tuple(vec![first, second])
                }
                second => {
                    let uniform = second.into_iter().chain(types).all(|ty| ty == first);
                    if uniform {
                        HirType::Array(Box::new(first))
                    } else {
                        perry_hir::infer_expr_type(expr, env)
                    }
                }
            }
        }

        Expr::ArrayFrom(source, _) => {
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
        Expr::StructuredClone { value, .. } => expression(value, env),
        Expr::MapGroupBy { items, key_fn } => methods::map_group_by(items, key_fn, env),
        Expr::Call {
            callee,
            args,
            type_args,
            ..
        } => {
            if let Expr::FuncRef(id) = callee.as_ref()
                && let Some(function) = env
                    .generic_functions
                    .and_then(|functions| functions.get(id))
                && let Ok(arguments) = functions::arguments(function, args, type_args, env)
            {
                let bindings = function
                    .type_params
                    .iter()
                    .zip(&arguments)
                    .map(|(parameter, argument)| (parameter.name.as_str(), argument))
                    .collect();
                let mut result = function.return_type.clone();
                substitute_type_vars(&mut result, &bindings);
                result
            } else {
                methods::call(expr, callee, args, env)
            }
        }
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
        } => {
            let source = strip_nullish(expression(object, env)).0;
            let builtin = crate::methods::Method::inferred(&source, property, true);
            let fallback = |env: &mut FunctionEnv<'_>| match builtin {
                Some(rule) => methods::result(expr, object, |_| None, rule, env),
                None => perry_hir::infer_expr_type(expr, env),
            };
            match source {
                HirType::Generic { base, .. }
                    if matches!(base.as_str(), "Map" | "ReadonlyMap" | "Set" | "ReadonlySet")
                        && property == "size" =>
                {
                    HirType::Number
                }
                HirType::Generic { base, type_args } => env
                    .decls
                    .property_type(env.ctx, &base, &type_args, property)
                    .or_else(|| env.class_member_type(&base, &type_args, property))
                    .or_else(|| {
                        env.decls
                            .dict_entry_types(env.ctx, &HirType::Generic { base, type_args })
                            .map(|(_, val_ty)| with_absences(val_ty, Absence::Undefined))
                    })
                    .unwrap_or_else(|| fallback(env)),
                HirType::Named(base) => env
                    .decls
                    .property_type(env.ctx, &base, &[], property)
                    .or_else(|| env.named_property_type(&base, property))
                    .or_else(|| {
                        env.decls
                            .dict_entry_types(env.ctx, &HirType::Named(base))
                            .map(|(_, val_ty)| with_absences(val_ty, Absence::Undefined))
                    })
                    .unwrap_or_else(|| fallback(env)),
                HirType::Object(object) => object
                    .properties
                    .get(property)
                    .map(|p| shapes::generics::optional_property(p.ty.clone(), p.optional))
                    .or_else(|| {
                        object
                            .index_signature
                            .as_ref()
                            .map(|v| with_absences((**v).clone(), Absence::Undefined))
                    })
                    .unwrap_or_else(|| fallback(env)),
                HirType::Array(_) if property == "groups" => HirType::Unknown,
                HirType::Array(_) if property == "index" => HirType::Number,
                HirType::Array(_) if property == "input" => HirType::String,
                HirType::String | HirType::Array(_) if property == "length" => HirType::Number,
                _ => fallback(env),
            }
        }
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
                    if let Some((_, val_ty)) = env.decls.dict_entry_types(env.ctx, &other) {
                        with_absences(val_ty, Absence::Undefined)
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
            object: Some(object),
            method,
            args,
            ..
        } => {
            let source = strip_nullish(expression(object, env)).0;
            let rule = args
                .is_empty()
                .then(|| crate::methods::Method::inferred(&source, method, true))
                .flatten()
                .or_else(|| crate::methods::Method::inferred(&source, method, false));
            match rule {
                Some(rule) => methods::result(expr, object, |index| args.get(index), rule, env),
                None => perry_hir::infer_expr_type(expr, env),
            }
        }
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
                    let (ty, optional) = match (val_expr, props.remove(key)) {
                        (Expr::Null, Some(existing)) => {
                            (with_absences(existing.ty, Absence::Null), existing.optional)
                        }
                        (Expr::Undefined, Some(existing)) => (
                            with_absences(existing.ty, Absence::Undefined),
                            existing.optional,
                        ),
                        _ => (val_ty, false),
                    };
                    props.insert(
                        key.clone(),
                        perry_hir::types::PropertyInfo {
                            ty,
                            optional,
                            readonly: false,
                        },
                    );
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
                    let resolved_props =
                        env.decls
                            .object_properties(env.ctx, &src_ty)
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
                                (abs @ (HirType::Null | HirType::Void), Some(existing)) => (
                                    with_absences(existing.ty, strip_nullish(abs).1),
                                    existing.optional,
                                ),
                                (ty, Some(existing)) if opt => {
                                    let (inner, prop_abs) = strip_nullish(ty);
                                    let (_, existing_abs) = strip_nullish(existing.ty);
                                    let existing_abs = merge_absence(existing_abs, prop_abs);
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
                    && let Some(orig) = env.decls.object_properties(env.ctx, &named)
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
            if let Some(resolved) = env.decls.object_properties(env.ctx, &src_ty) {
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
            if hir_compat::is_object_literal(class_name)
                && let Some(class) = env.classes.iter().find(|class| class.name == *class_name)
            {
                let mut properties = HashMap::new();
                for (field, argument) in class.fields.iter().zip(args) {
                    properties.insert(
                        field.name.clone(),
                        perry_hir::types::PropertyInfo {
                            ty: expression(argument, env),
                            optional: false,
                            readonly: false,
                        },
                    );
                }
                HirType::Object(perry_hir::types::ObjectType {
                    name: None,
                    properties,
                    property_order: Some(
                        class
                            .fields
                            .iter()
                            .map(|field| field.name.clone())
                            .collect(),
                    ),
                    index_signature: None,
                })
            } else if !type_args.is_empty() {
                HirType::Generic {
                    base: class_name.clone(),
                    type_args: type_args.clone(),
                }
            } else if let Some(class) = env.classes.iter().find(|c| c.name == *class_name)
                && !class.type_params.is_empty()
            {
                if let Some(type_args) = functions::class_arguments(class, args, env) {
                    HirType::Generic {
                        base: class_name.clone(),
                        type_args,
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

/// Return the result type of `method` on `source` (a live view of a `Map` or `Set`, or a
/// fresh array of indices, elements, or `[index, element]` pairs), or `None` for other receivers.
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
