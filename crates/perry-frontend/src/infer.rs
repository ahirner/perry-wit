//! Conservative completion-aware inference for unresolved local helper returns.
use crate::methods::Kind;
use crate::shapes::HirTypeExt;
use crate::{Absence, hir_compat, merge_absence, shapes};
use anyhow::{Context as _, Result, ensure};
use perry_hir::{
    ir::{CompareOp, Expr, LogicalOp, Module as HirModule, Stmt},
    types::{FuncId, LocalId, Type as HirType},
};
use pliron::context::Context;
use std::collections::HashSet;

use perry_hir::HirTypeFacts;
use std::collections::HashMap;

pub(crate) fn contains_type_var(ty: &HirType) -> bool {
    ty.any(&mut |node| matches!(node, HirType::TypeVar(_)))
}

mod absence;
mod arrays;
mod closures;
mod environment;
mod functions;
use absence::*;
use closures::{infer_array_closure_return, infer_closure_return};
pub(crate) use functions::specialize_functions;
mod expressions;
mod methods;
pub(crate) mod walk;
use environment::FunctionEnv;

struct Inference<'a> {
    ctx: &'a Context,
    decls: &'a shapes::Declarations<'a>,
    metadata: &'a hir_compat::Metadata<'a>,
    generic_functions: Option<&'a HashMap<FuncId, perry_hir::ir::Function>>,
    open_arrays: HashSet<LocalId>,
    local_spans: &'a HashMap<LocalId, perry_hir::ir::LocalSourceSpan>,
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
            decls: self.decls,
            classes,
            locals,
            metadata: self.metadata,
            generic_functions: self.generic_functions,
            instantiated_returns: HashMap::new(),
            open_arrays: &self.open_arrays,
            local_spans: self.local_spans,
        }
    }
}

use expressions::*;

fn clear_inferred_type_vars<V>(
    statements: &mut [Stmt],
    inferred: &HashSet<LocalId>,
    spans: &HashMap<LocalId, V>,
    decls: &shapes::Declarations<'_>,
) {
    walk::locals_mut(statements, &mut |id, ty| {
        if inferred.contains(&id) || !spans.contains_key(&id) {
            if contains_type_var(ty)
                || matches!(ty, HirType::Named(name) if name == "Map" || name == "Set")
            {
                *ty = HirType::Any;
            } else if let HirType::Named(name) = ty
                && let Some(carrier) = decls.integer_carrier(name)
                && carrier != HirType::BigInt
            {
                *ty = carrier;
            }
        }
    });
}

fn push_return_type(types: &mut Vec<HirType>, ty: HirType, decls: &shapes::Declarations<'_>) {
    if matches!(ty, HirType::Never) {
        return;
    }
    if let HirType::Union(_) = ty {
        let (defined, absences) = strip_nullish(ty);
        if let Some(absence) = absences {
            if defined != HirType::Void {
                push_return_type(types, defined, decls);
            }
            if absence.admits_null() {
                push_return_type(types, HirType::Null, decls);
            }
            if absence.admits_undefined() {
                push_return_type(types, HirType::Void, decls);
            }
        } else if !types.contains(&defined) {
            types.push(defined);
        }
        return;
    }
    let normalized = match ty {
        HirType::Int32 => HirType::Number,
        HirType::Named(name) => decls
            .integer_carrier(&name)
            .filter(|carrier| *carrier != HirType::BigInt)
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
            let absence = match default_expr {
                Expr::Null => Some(Absence::Null),
                Expr::Undefined if !is_exported => Some(Absence::Undefined),
                _ => None,
            };
            param.ty = with_absences(std::mem::replace(&mut param.ty, HirType::Void), absence);
        }
        env.locals.insert(param.id, param.ty.clone());
    }
}

fn infer_body_return(
    body: &[Stmt],
    is_async: bool,
    return_type: &mut HirType,
    env: &mut FunctionEnv<'_>,
    decls: &shapes::Declarations<'_>,
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
    if returned.iter().any(|ty| {
        ty.any(&mut |node| matches!(node, HirType::Any | HirType::Unknown | HirType::TypeVar(_)))
    }) {
        return false;
    }
    if crate::completion::returns_optional(body) {
        returned.push(HirType::Void);
    }
    let mut types = Vec::new();
    for ty in returned {
        push_return_type(&mut types, ty, decls);
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

pub(crate) fn unresolved_types(
    ctx: &Context,
    hir: &mut HirModule,
    metadata: &hir_compat::Metadata<'_>,
    foreign: &HashMap<String, crate::hir::Foreign>,
) -> Result<()> {
    let open_arrays = arrays::open_bindings(hir);
    let inferred = &metadata.inferred_bindings;
    let locations = &metadata.locations;
    let decls = shapes::Declarations::new(
        &hir.interfaces,
        &hir.type_aliases,
        &hir.imports,
        &hir.classes,
        foreign,
    );
    clear_inferred_type_vars(&mut hir.init, inferred, &hir.local_source_spans, &decls);
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
            &decls,
        );
    }
    for class in &mut hir.classes {
        for function in shapes::class_functions_mut(class) {
            clear_inferred_type_vars(
                &mut function.body,
                inferred,
                &hir.local_source_spans,
                &decls,
            );
        }
    }
    let inference = Inference {
        ctx,
        decls: &decls,
        metadata,
        generic_functions: None,
        open_arrays,
        local_spans: &hir.local_source_spans,
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
            let Some(mut params) = shapes::class_functions_mut(&mut hir.classes[class_idx])
                .nth(fn_idx)
                .map(|f| std::mem::take(&mut f.params))
            else {
                break;
            };
            let mut env = inference.function(&module_env, &hir.classes, &mut locals);
            infer_params(&mut params, false, &mut env);
            if let Some(f) = shapes::class_functions_mut(&mut hir.classes[class_idx]).nth(fn_idx) {
                f.params = params;
            }
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
        walk::resolve_locals(&mut hir.init, env, false)?;
    }
    module_env = perry_hir::HirTypeEnv::from_module(hir);
    let max_iters = hir.functions.len() + hir.classes.len() + inference.open_arrays.len();
    for _ in 0..=max_iters {
        let env = inference.function(&module_env, &hir.classes, &mut locals);
        let mut changed = walk::resolve_locals(&mut hir.init, env, false)?;
        module_env = perry_hir::HirTypeEnv::from_module(hir);
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
                &decls,
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
                    let Some(method) = shapes::class_functions(class).nth(fn_idx) else {
                        break;
                    };
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
                        infer_body_return(&method.body, method.is_async, &mut ret, &mut env, &decls)
                            .then_some(ret)
                    }
                };
                if let Some(ret) = updated
                    && let Some(method) =
                        shapes::class_functions_mut(&mut hir.classes[class_idx]).nth(fn_idx)
                {
                    method.return_type = ret;
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
    walk::resolve_locals(&mut hir.init, env, true)?;
    module_env = perry_hir::HirTypeEnv::from_module(hir);
    for function in &mut hir.functions {
        let env = inference.function_with_params(
            &module_env,
            &hir.classes,
            &mut locals,
            &function.params,
        );
        walk::resolve_locals(&mut function.body, env, true)?;
    }
    for class_idx in 0..hir.classes.len() {
        module_env.set_current_class(Some(hir.classes[class_idx].name.clone()));
        let fn_count = shapes::class_functions(&hir.classes[class_idx]).count();
        for fn_idx in 0..fn_count {
            let Some(mut body) = shapes::class_functions_mut(&mut hir.classes[class_idx])
                .nth(fn_idx)
                .map(|f| std::mem::take(&mut f.body))
            else {
                break;
            };
            if let Some(method) = shapes::class_functions(&hir.classes[class_idx]).nth(fn_idx) {
                let env = inference.function_with_params(
                    &module_env,
                    &hir.classes,
                    &mut locals,
                    &method.params,
                );
                walk::resolve_locals(&mut body, env, true)?;
            }
            if let Some(method) =
                shapes::class_functions_mut(&mut hir.classes[class_idx]).nth(fn_idx)
            {
                method.body = body;
            }
        }
    }
    module_env.set_current_class(None);
    Ok(())
}
