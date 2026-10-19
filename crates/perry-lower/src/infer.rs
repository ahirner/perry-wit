//! Conservative completion-aware inference for unresolved local helper returns.
use super::*;

use crate::coerce::merge_absence;
use perry_hir::HirTypeFacts;
use perry_mir::Absence;
use std::collections::HashMap;

pub(super) fn contains_type_var(ty: &HirType) -> bool {
    ty.any(&mut |node| matches!(node, HirType::TypeVar(_)))
}

mod absence;
mod closures;
mod environment;
mod functions;
use absence::*;
use closures::{infer_array_closure_return, infer_closure_return};
pub(super) use functions::specialize_functions;
mod expressions;
mod methods;
mod walk;
use environment::FunctionEnv;

struct Inference<'a> {
    ctx: &'a Context,
    interfaces: &'a [perry_hir::ir::Interface],
    type_aliases: &'a [perry_hir::ir::TypeAlias],
    integer_aliases: &'a HashMap<String, HirType>,
    imported_types: &'a HashMap<String, String>,
    foreign: &'a HashMap<String, crate::hir::Foreign>,
    metadata: &'a hir_compat::Metadata<'a>,
    generic_functions: Option<&'a HashMap<FuncId, perry_hir::ir::Function>>,
}

impl Inference<'_> {
    fn function<'a>(
        &'a self,
        base: &'a perry_hir::HirTypeEnv,
        classes: &'a [perry_hir::ir::Class],
        locals: &'a mut HashMap<LocalId, HirType>,
    ) -> FunctionEnv<'a> {
        self.function_with_params(base, classes, locals, &[])
    }

    fn function_with_params<'a>(
        &'a self,
        base: &'a perry_hir::HirTypeEnv,
        classes: &'a [perry_hir::ir::Class],
        locals: &'a mut HashMap<LocalId, HirType>,
        params: &[perry_hir::ir::Param],
    ) -> FunctionEnv<'a> {
        locals.clear();
        for param in params {
            locals.insert(param.id, param.ty.clone());
        }
        FunctionEnv {
            ctx: self.ctx,
            base,
            interfaces: self.interfaces,
            type_aliases: self.type_aliases,
            classes,
            locals,
            integer_aliases: self.integer_aliases,
            imported_types: self.imported_types,
            foreign: self.foreign,
            metadata: self.metadata,
            generic_functions: self.generic_functions,
            instantiated_returns: HashMap::new(),
        }
    }
}

use expressions::*;

fn collect_foreign_imports(
    hir: &HirModule,
    foreign: &HashMap<String, crate::hir::Foreign>,
) -> (HashMap<String, HirType>, HashMap<String, String>) {
    let mut names: HashMap<String, HirType> =
        ["u8", "s8", "u16", "s16", "u32", "s32", "u64", "s64"]
            .into_iter()
            .filter_map(|tag| {
                let (width, _) = shapes::perry_integer_spec(tag)?;
                Some((
                    format!("perry:types/{tag}"),
                    shapes::integer_hir_carrier(width),
                ))
            })
            .chain(foreign.iter().filter_map(|(key, entry)| match entry {
                crate::hir::Foreign::Integer { width, .. } => {
                    Some((key.clone(), shapes::integer_hir_carrier(*width)))
                }
                _ => None,
            }))
            .collect();
    let user_types: HashSet<&str> = hir
        .interfaces
        .iter()
        .map(|i| i.name.as_str())
        .chain(hir.type_aliases.iter().map(|a| a.name.as_str()))
        .chain(hir.classes.iter().map(|c| c.name.as_str()))
        .collect();
    let mut imported_types = HashMap::new();
    for import in &hir.imports {
        let source_module = import
            .source
            .strip_prefix("wit:")
            .unwrap_or(import.source.as_str());
        for specifier in &import.specifiers {
            let perry_hir::ir::ImportSpecifier::Named { imported, local } = specifier else {
                continue;
            };
            if user_types.contains(local.as_str()) {
                continue;
            }
            if import.source == "perry:types"
                && let Some((width, _)) = shapes::perry_integer_spec(imported)
            {
                names.insert(local.clone(), shapes::integer_hir_carrier(width));
                continue;
            }
            if let Some((candidate, entry)) =
                [format!("{source_module}#{imported}"), imported.clone()]
                    .into_iter()
                    .find_map(|c| foreign.get(&c).map(|e| (c, e)))
            {
                match entry {
                    crate::hir::Foreign::Integer { width, .. } => {
                        names.insert(local.clone(), shapes::integer_hir_carrier(*width));
                    }
                    crate::hir::Foreign::Type(HirType::Named(target)) => {
                        if let Some(carrier) = names.get(target).cloned() {
                            names.insert(local.clone(), carrier);
                        }
                    }
                    crate::hir::Foreign::Type(_)
                    | crate::hir::Foreign::Resource(_)
                    | crate::hir::Foreign::Input(_) => {}
                    crate::hir::Foreign::Function(_) => continue,
                }
                imported_types.insert(local.clone(), candidate);
            }
        }
    }
    while hir.type_aliases.iter().any(|alias| {
        alias.type_params.is_empty()
            && matches!(&alias.ty, HirType::Named(target) if names.contains_key(target) && !names.contains_key(&alias.name))
    }) {
        for alias in &hir.type_aliases {
            if alias.type_params.is_empty()
                && let HirType::Named(target) = &alias.ty
                && let Some(carrier) = names.get(target).cloned()
            {
                names.entry(alias.name.clone()).or_insert(carrier);
            }
        }
    }
    (names, imported_types)
}

fn clear_inferred_type_vars<V>(
    statements: &mut [Stmt],
    inferred: &HashSet<LocalId>,
    spans: &HashMap<LocalId, V>,
    integer_aliases: &HashMap<String, HirType>,
) {
    walk::locals_mut(statements, &mut |id, ty| {
        if inferred.contains(&id) || !spans.contains_key(&id) {
            if contains_type_var(ty)
                || matches!(ty, HirType::Named(name) if name == "Map" || name == "Set")
            {
                *ty = HirType::Any;
            } else if let HirType::Named(name) = ty
                && let Some(carrier) = integer_aliases.get(name)
                && *carrier != HirType::BigInt
            {
                *ty = carrier.clone();
            }
        }
    });
}

fn push_return_type(
    types: &mut Vec<HirType>,
    ty: HirType,
    integer_aliases: &HashMap<String, HirType>,
) {
    if matches!(ty, HirType::Never) {
        return;
    }
    if let HirType::Union(members) = ty {
        if members
            .iter()
            .any(|m| matches!(m, HirType::Null | HirType::Void))
        {
            let mut defined = Vec::new();
            let mut absences = Vec::new();
            for member in members {
                if matches!(member, HirType::Null | HirType::Void) {
                    absences.push(member);
                } else {
                    defined.push(member);
                }
            }
            match defined.len() {
                0 => {}
                1 => push_return_type(types, defined.swap_remove(0), integer_aliases),
                _ => push_return_type(types, HirType::Union(defined), integer_aliases),
            }
            for absence in absences {
                push_return_type(types, absence, integer_aliases);
            }
            return;
        }
        let union = HirType::Union(members);
        if !types.contains(&union) {
            types.push(union);
        }
        return;
    }
    let normalized = match ty {
        HirType::Int32 => HirType::Number,
        HirType::Named(name) => integer_aliases
            .get(&name)
            .filter(|carrier| **carrier != HirType::BigInt)
            .cloned()
            .unwrap_or(HirType::Named(name)),
        HirType::StringLiteral(_) => HirType::String,
        ty => ty,
    };
    if !types.contains(&normalized) {
        types.push(normalized);
    }
}

fn substitute_type_vars(ty: &mut HirType, bounds: &HashMap<&str, &HirType>) {
    ty.transform(&mut |node| {
        if let HirType::TypeVar(name) | HirType::Named(name) = node
            && let Some(bound) = bounds.get(name.as_str())
        {
            *node = (*bound).clone();
        }
    });
}

fn infer_params(params: &mut [perry_hir::ir::Param], is_exported: bool, env: &mut FunctionEnv<'_>) {
    for param in params {
        if matches!(param.ty, HirType::Any | HirType::Unknown) {
            if let Some(default_expr) = &param.default {
                let def_ty = expression(default_expr, env);
                if is_resolvable_type(&def_ty) {
                    param.ty = def_ty;
                }
            } else if param.is_rest {
                param.ty = HirType::Array(Box::new(HirType::Number));
            }
        } else if let Some(default_expr) = &param.default
            && is_resolvable_type(&param.ty)
        {
            if matches!(default_expr, Expr::Null) {
                param.ty = with_absences(param.ty.clone(), Absence::Null);
            } else if matches!(default_expr, Expr::Undefined) && !is_exported {
                param.ty = with_absences(param.ty.clone(), Absence::Undefined);
            }
        }
        env.locals.insert(param.id, param.ty.clone());
    }
}

fn infer_body_return(
    body: &[Stmt],
    is_async: bool,
    return_type: &mut HirType,
    env: &mut FunctionEnv<'_>,
    integer_aliases: &HashMap<String, HirType>,
) -> bool {
    let mut returned = Vec::new();
    walk::scan(body, env, Some(&mut returned), None);
    if is_async {
        for ty in &mut returned {
            while let HirType::Promise(inner) = ty {
                *ty = std::mem::replace(inner.as_mut(), HirType::Void);
            }
        }
    }
    if returned
        .iter()
        .any(|ty| matches!(ty, HirType::Any | HirType::Unknown))
    {
        return false;
    }
    if inline::returns_optional(body) {
        returned.push(HirType::Void);
    }
    let mut types = Vec::new();
    for ty in returned {
        push_return_type(&mut types, ty, integer_aliases);
    }
    let ty = match types.as_slice() {
        [] => HirType::Void,
        [_] => types.swap_remove(0),
        _ if types
            .iter()
            .filter(|ty| !matches!(ty, HirType::Null | HirType::Void))
            .count()
            == 1 =>
        {
            HirType::Union(types)
        }
        _ => return false,
    };
    let inferred = if is_async {
        HirType::Promise(Box::new(ty))
    } else {
        ty
    };
    if inferred != *return_type {
        *return_type = inferred;
        true
    } else {
        false
    }
}

pub(super) fn unresolved_types(
    ctx: &Context,
    hir: &mut HirModule,
    metadata: &hir_compat::Metadata<'_>,
    foreign: &HashMap<String, crate::hir::Foreign>,
) {
    let inferred = &metadata.inferred_bindings;
    let locations = &metadata.locations;
    let (integer_aliases, imported_types) = collect_foreign_imports(hir, foreign);
    clear_inferred_type_vars(
        &mut hir.init,
        inferred,
        &hir.local_source_spans,
        &integer_aliases,
    );
    for function in &mut hir.functions {
        if !function.type_params.is_empty() {
            let bounds: HashMap<&str, &HirType> = function
                .type_params
                .iter()
                .filter_map(|tp| {
                    tp.constraint
                        .as_deref()
                        .or(tp.default.as_deref())
                        .map(|bound| (tp.name.as_str(), bound))
                })
                .collect();
            if !bounds.is_empty() {
                for param in &mut function.params {
                    substitute_type_vars(&mut param.ty, &bounds);
                }
                substitute_type_vars(&mut function.return_type, &bounds);
                walk::locals_mut(&mut function.body, &mut |_, ty| {
                    substitute_type_vars(ty, &bounds)
                });
            }
        }
        clear_inferred_type_vars(
            &mut function.body,
            inferred,
            &hir.local_source_spans,
            &integer_aliases,
        );
    }
    for class in &mut hir.classes {
        for function in shapes::class_functions_mut(class) {
            clear_inferred_type_vars(
                &mut function.body,
                inferred,
                &hir.local_source_spans,
                &integer_aliases,
            );
        }
    }
    let inference = Inference {
        ctx,
        interfaces: &hir.interfaces,
        type_aliases: &hir.type_aliases,
        integer_aliases: &integer_aliases,
        imported_types: &imported_types,
        foreign,
        metadata,
        generic_functions: None,
    };
    let mut locals = HashMap::new();
    let mut module_env = perry_hir::HirTypeEnv::from_module(hir);
    for function in &mut hir.functions {
        let mut env = inference.function(&module_env, &hir.classes, &mut locals);
        infer_params(&mut function.params, function.is_exported, &mut env);
    }
    for class_idx in 0..hir.classes.len() {
        module_env.set_current_class(Some(hir.classes[class_idx].name.clone()));
        for sf_idx in 0..hir.classes[class_idx].static_fields.len() {
            let def_ty = {
                let sfield = &hir.classes[class_idx].static_fields[sf_idx];
                if !matches!(sfield.ty, HirType::Any | HirType::Unknown) {
                    None
                } else {
                    sfield.init.as_ref().map(|init| {
                        let mut env = inference.function(&module_env, &hir.classes, &mut locals);
                        expression(init, &mut env)
                    })
                }
            };
            if let Some(def_ty) = def_ty.filter(is_resolvable_type) {
                hir.classes[class_idx].static_fields[sf_idx].ty = def_ty;
            }
        }
        let fn_count = shapes::class_functions(&hir.classes[class_idx]).count();
        for fn_idx in 0..fn_count {
            let mut params = std::mem::take(
                &mut shapes::class_functions_mut(&mut hir.classes[class_idx])
                    .nth(fn_idx)
                    .unwrap()
                    .params,
            );
            let mut env = inference.function(&module_env, &hir.classes, &mut locals);
            infer_params(&mut params, false, &mut env);
            shapes::class_functions_mut(&mut hir.classes[class_idx])
                .nth(fn_idx)
                .unwrap()
                .params = params;
        }
        for f_idx in 0..hir.classes[class_idx].fields.len() {
            let inferred_ty = {
                let class = &hir.classes[class_idx];
                let field = &class.fields[f_idx];
                if !matches!(field.ty, HirType::Any | HirType::Unknown) {
                    None
                } else if let Some(init) = &field.init {
                    let mut env = inference.function(&module_env, &hir.classes, &mut locals);
                    Some(expression(init, &mut env)).filter(is_resolvable_type)
                } else if let Some(ctor) = &class.constructor {
                    let mut env = inference.function_with_params(
                        &module_env,
                        &hir.classes,
                        &mut locals,
                        &ctor.params,
                    );
                    ctor.body.iter().find_map(|stmt| match stmt {
                        Stmt::Expr(Expr::PropertySet {
                            object,
                            property,
                            value,
                        }) if matches!(object.as_ref(), Expr::This) && *property == field.name => {
                            Some(expression(value, &mut env)).filter(is_resolvable_type)
                        }
                        _ => None,
                    })
                } else {
                    None
                }
            };
            if let Some(ty) = inferred_ty {
                hir.classes[class_idx].fields[f_idx].ty = ty;
            }
        }
    }
    module_env.set_current_class(None);
    module_env = perry_hir::HirTypeEnv::from_module(hir);
    {
        let env = inference.function(&module_env, &hir.classes, &mut locals);
        walk::resolve_locals(&mut hir.init, env);
    }
    module_env = perry_hir::HirTypeEnv::from_module(hir);
    let max_iters = hir.functions.len() + hir.classes.len();
    for _ in 0..=max_iters {
        let mut changed = false;
        for function in &mut hir.functions {
            if locations.has_explicit_return(&function.name) {
                continue;
            }
            let mut env = inference.function_with_params(
                &module_env,
                &hir.classes,
                &mut locals,
                &function.params,
            );
            if infer_body_return(
                &function.body,
                function.is_async,
                &mut function.return_type,
                &mut env,
                &integer_aliases,
            ) {
                changed = true;
            }
        }
        for class_idx in 0..hir.classes.len() {
            module_env.set_current_class(Some(hir.classes[class_idx].name.clone()));
            let fn_count = shapes::class_functions(&hir.classes[class_idx]).count();
            for fn_idx in 0..fn_count {
                let updated = {
                    let class = &hir.classes[class_idx];
                    let method = shapes::class_functions(class).nth(fn_idx).unwrap();
                    if class
                        .constructor
                        .as_ref()
                        .is_some_and(|c| std::ptr::eq(c, method))
                        || class.setters.iter().any(|(_, s)| std::ptr::eq(s, method))
                        || !matches!(method.return_type, HirType::Any | HirType::Unknown)
                    {
                        None
                    } else {
                        let mut ret = method.return_type.clone();
                        let mut env = inference.function_with_params(
                            &module_env,
                            &hir.classes,
                            &mut locals,
                            &method.params,
                        );
                        infer_body_return(
                            &method.body,
                            method.is_async,
                            &mut ret,
                            &mut env,
                            &integer_aliases,
                        )
                        .then_some(ret)
                    }
                };
                if let Some(ret) = updated {
                    shapes::class_functions_mut(&mut hir.classes[class_idx])
                        .nth(fn_idx)
                        .unwrap()
                        .return_type = ret;
                    changed = true;
                }
            }
        }
        module_env.set_current_class(None);
        if !changed {
            break;
        }
        module_env = perry_hir::HirTypeEnv::from_module(hir);
    }
    let env = inference.function(&module_env, &hir.classes, &mut locals);
    walk::resolve_locals(&mut hir.init, env);
    module_env = perry_hir::HirTypeEnv::from_module(hir);
    for function in &mut hir.functions {
        let env = inference.function_with_params(
            &module_env,
            &hir.classes,
            &mut locals,
            &function.params,
        );
        walk::resolve_locals(&mut function.body, env);
    }
    for class_idx in 0..hir.classes.len() {
        module_env.set_current_class(Some(hir.classes[class_idx].name.clone()));
        let fn_count = shapes::class_functions(&hir.classes[class_idx]).count();
        for fn_idx in 0..fn_count {
            let mut body = std::mem::take(
                &mut shapes::class_functions_mut(&mut hir.classes[class_idx])
                    .nth(fn_idx)
                    .unwrap()
                    .body,
            );
            let params = &shapes::class_functions(&hir.classes[class_idx])
                .nth(fn_idx)
                .unwrap()
                .params;
            let env =
                inference.function_with_params(&module_env, &hir.classes, &mut locals, params);
            walk::resolve_locals(&mut body, env);
            shapes::class_functions_mut(&mut hir.classes[class_idx])
                .nth(fn_idx)
                .unwrap()
                .body = body;
        }
    }
    module_env.set_current_class(None);
}
