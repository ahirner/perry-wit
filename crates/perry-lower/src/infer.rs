//! Conservative completion-aware inference for unresolved local helper returns.
use super::*;

use crate::coerce::merge_absence;
use perry_hir::HirTypeFacts;
use perry_mir::Absence;
use std::collections::HashMap;

pub(super) fn contains_type_var(ty: &HirType) -> bool {
    match ty {
        HirType::TypeVar(_) => true,
        HirType::Array(inner) | HirType::Promise(inner) => contains_type_var(inner),
        HirType::Tuple(items) | HirType::Union(items) => items.iter().any(contains_type_var),
        HirType::Generic { type_args, .. } => type_args.iter().any(contains_type_var),
        HirType::Object(object) => {
            object.properties.values().any(|p| contains_type_var(&p.ty))
                || object
                    .index_signature
                    .as_deref()
                    .is_some_and(contains_type_var)
        }
        HirType::Function(function) => {
            function
                .params
                .iter()
                .any(|(_, ty, _)| contains_type_var(ty))
                || contains_type_var(&function.return_type)
        }
        _ => false,
    }
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

enum ScanMode<'a> {
    Returns(&'a mut Vec<HirType>),
    ResolveLocals(&'a mut HashMap<LocalId, HirType>),
}

fn collect_foreign_imports(
    hir: &HirModule,
    foreign: &HashMap<String, crate::hir::Foreign>,
) -> (HashMap<String, HirType>, HashMap<String, String>) {
    let mut names: HashMap<String, HirType> = [
        ("perry:types/u8", HirType::Number),
        ("perry:types/s8", HirType::Number),
        ("perry:types/u16", HirType::Number),
        ("perry:types/s16", HirType::Number),
        ("perry:types/u32", HirType::Number),
        ("perry:types/s32", HirType::Number),
        ("perry:types/u64", HirType::BigInt),
        ("perry:types/s64", HirType::BigInt),
    ]
    .into_iter()
    .map(|(k, v)| (String::from(k), v))
    .collect();
    for (key, entry) in foreign {
        if let crate::hir::Foreign::Integer { width, .. } = entry {
            names.insert(
                key.clone(),
                if *width == 64 {
                    HirType::BigInt
                } else {
                    HirType::Number
                },
            );
        }
    }
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
            if import.source == "perry:types" {
                match imported.as_str() {
                    "u8" | "s8" | "u16" | "s16" | "u32" | "s32" => {
                        names.insert(local.clone(), HirType::Number);
                        continue;
                    }
                    "u64" | "s64" => {
                        names.insert(local.clone(), HirType::BigInt);
                        continue;
                    }
                    _ => {}
                }
            }
            for candidate in [format!("{source_module}#{imported}"), imported.clone()] {
                match foreign.get(&candidate) {
                    Some(crate::hir::Foreign::Integer { width, .. }) => {
                        names.insert(
                            local.clone(),
                            if *width == 64 {
                                HirType::BigInt
                            } else {
                                HirType::Number
                            },
                        );
                        imported_types.insert(local.clone(), candidate);
                        break;
                    }
                    Some(crate::hir::Foreign::Type(HirType::Named(target))) => {
                        if let Some(carrier) = names.get(target).cloned() {
                            names.insert(local.clone(), carrier);
                        }
                        imported_types.insert(local.clone(), candidate);
                        break;
                    }
                    Some(
                        crate::hir::Foreign::Type(_)
                        | crate::hir::Foreign::Resource(_)
                        | crate::hir::Foreign::Input(_),
                    ) => {
                        imported_types.insert(local.clone(), candidate);
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    loop {
        let mut added = false;
        for alias in &hir.type_aliases {
            if alias.type_params.is_empty()
                && let HirType::Named(target) = &alias.ty
                && let Some(carrier) = names.get(target).cloned()
                && !names.contains_key(&alias.name)
            {
                names.insert(alias.name.clone(), carrier);
                added = true;
            }
        }
        if !added {
            break;
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
    match ty {
        HirType::TypeVar(name) | HirType::Named(name) => {
            if let Some(bound) = bounds.get(name.as_str()) {
                *ty = (*bound).clone();
            }
        }
        HirType::Array(inner) | HirType::Promise(inner) => substitute_type_vars(inner, bounds),
        HirType::Tuple(items) | HirType::Union(items) => {
            for item in items {
                substitute_type_vars(item, bounds);
            }
        }
        HirType::Generic { type_args, .. } => {
            for arg in type_args {
                substitute_type_vars(arg, bounds);
            }
        }
        HirType::Object(object) => {
            for prop in object.properties.values_mut() {
                substitute_type_vars(&mut prop.ty, bounds);
            }
            if let Some(index) = &mut object.index_signature {
                substitute_type_vars(index, bounds);
            }
        }
        HirType::Function(function) => {
            for (_, param, _) in &mut function.params {
                substitute_type_vars(param, bounds);
            }
            substitute_type_vars(&mut function.return_type, bounds);
        }
        _ => {}
    }
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
    walk::scan(body, env, &mut ScanMode::Returns(&mut returned));
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
        for function in functions::class_functions(class) {
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
    let classes_snapshot = hir.classes.clone();
    for class in &mut hir.classes {
        module_env.set_current_class(Some(class.name.clone()));
        for sfield in &mut class.static_fields {
            if matches!(sfield.ty, HirType::Any | HirType::Unknown)
                && let Some(init) = &sfield.init
            {
                let mut env = inference.function(&module_env, &classes_snapshot, &mut locals);
                let def_ty = expression(init, &mut env);
                if is_resolvable_type(&def_ty) {
                    sfield.ty = def_ty;
                }
            }
        }
        for function in functions::class_functions(class) {
            let mut env = inference.function(&module_env, &classes_snapshot, &mut locals);
            infer_params(&mut function.params, false, &mut env);
        }
        for field in &mut class.fields {
            if !matches!(field.ty, HirType::Any | HirType::Unknown) {
                continue;
            }
            if let Some(init) = &field.init {
                let mut env = inference.function(&module_env, &classes_snapshot, &mut locals);
                let def_ty = expression(init, &mut env);
                if is_resolvable_type(&def_ty) {
                    field.ty = def_ty;
                    continue;
                }
            }
            if let Some(ctor) = &class.constructor {
                let mut env = inference.function_with_params(
                    &module_env,
                    &classes_snapshot,
                    &mut locals,
                    &ctor.params,
                );
                for stmt in &ctor.body {
                    if let Stmt::Expr(Expr::PropertySet {
                        object,
                        property,
                        value,
                    }) = stmt
                        && matches!(object.as_ref(), Expr::This)
                        && *property == field.name
                    {
                        let val_ty = expression(value, &mut env);
                        if is_resolvable_type(&val_ty) {
                            field.ty = val_ty;
                            break;
                        }
                    }
                }
            }
        }
    }
    module_env.set_current_class(None);
    module_env = perry_hir::HirTypeEnv::from_module(hir);
    {
        let mut env = inference.function(&module_env, &hir.classes, &mut locals);
        walk::resolve_locals(&mut hir.init, &mut env);
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
        if !hir.classes.is_empty() {
            let classes_snapshot = hir.classes.clone();
            for class in &mut hir.classes {
                module_env.set_current_class(Some(class.name.clone()));
                for method in class
                    .methods
                    .iter_mut()
                    .chain(class.getters.iter_mut().map(|(_, f)| f))
                    .chain(class.static_methods.iter_mut())
                {
                    if !matches!(method.return_type, HirType::Any | HirType::Unknown) {
                        continue;
                    }
                    let mut env = inference.function_with_params(
                        &module_env,
                        &classes_snapshot,
                        &mut locals,
                        &method.params,
                    );
                    if infer_body_return(
                        &method.body,
                        method.is_async,
                        &mut method.return_type,
                        &mut env,
                        &integer_aliases,
                    ) {
                        changed = true;
                    }
                }
            }
            module_env.set_current_class(None);
        }
        if !changed {
            break;
        }
        module_env = perry_hir::HirTypeEnv::from_module(hir);
    }
    let mut env = inference.function(&module_env, &hir.classes, &mut locals);
    walk::resolve_locals(&mut hir.init, &mut env);
    module_env = perry_hir::HirTypeEnv::from_module(hir);
    for function in &mut hir.functions {
        let mut env = inference.function_with_params(
            &module_env,
            &hir.classes,
            &mut locals,
            &function.params,
        );
        walk::resolve_locals(&mut function.body, &mut env);
    }
    if !hir.classes.is_empty() {
        let classes_snapshot = hir.classes.clone();
        for class in &mut hir.classes {
            module_env.set_current_class(Some(class.name.clone()));
            for function in functions::class_functions(class) {
                let mut env = inference.function_with_params(
                    &module_env,
                    &classes_snapshot,
                    &mut locals,
                    &function.params,
                );
                walk::resolve_locals(&mut function.body, &mut env);
            }
        }
        module_env.set_current_class(None);
    }
}
