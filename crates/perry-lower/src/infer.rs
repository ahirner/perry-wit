//! Conservative completion-aware inference for unresolved local helper returns.
use super::*;

use perry_hir::HirTypeFacts;
use std::collections::HashMap;

fn contains_type_var(ty: &HirType) -> bool {
    match ty {
        HirType::TypeVar(_) => true,
        HirType::Array(inner) | HirType::Promise(inner) => contains_type_var(inner),
        HirType::Tuple(items) | HirType::Union(items) => items.iter().any(contains_type_var),
        HirType::Generic { type_args, .. } => type_args.iter().any(contains_type_var),
        HirType::Object(object) => object.properties.values().any(|p| contains_type_var(&p.ty)),
        _ => false,
    }
}

fn is_resolvable_type(ty: &HirType) -> bool {
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
        HirType::Generic { type_args, .. } => type_args.iter().all(is_resolvable_type),
        HirType::Object(object) => {
            object.index_signature.is_none()
                && object
                    .properties
                    .values()
                    .all(|p| is_resolvable_type(&p.ty))
        }
        _ => true,
    }
}

fn expression(expr: &Expr, env: &FunctionEnv<'_>) -> HirType {
    let ty = match expr {
        Expr::Call { callee, .. } | Expr::CallSpread { callee, .. } => match callee.as_ref() {
            Expr::PropertyGet {
                object, property, ..
            } if matches!(property.as_str(), "concat" | array_methods::CONCAT) => {
                expression(object, env)
            }
            _ => perry_hir::infer_expr_type(expr, env),
        },
        Expr::PropertyGet {
            object, property, ..
        } => match expression(object, env) {
            HirType::Generic { base, type_args } => shapes::generics::property_type(
                env.interfaces,
                env.type_aliases,
                &base,
                &type_args,
                property,
            )
            .unwrap_or_else(|| perry_hir::infer_expr_type(expr, env)),
            HirType::Named(base) => shapes::generics::property_type(
                env.interfaces,
                env.type_aliases,
                &base,
                &[],
                property,
            )
            .or_else(|| env.named_property_type(&base, property))
            .unwrap_or_else(|| perry_hir::infer_expr_type(expr, env)),
            HirType::Object(object) => object
                .properties
                .get(property)
                .map(|p| p.ty.clone())
                .unwrap_or_else(|| perry_hir::infer_expr_type(expr, env)),
            HirType::String | HirType::Array(_) if property == "length" => HirType::Number,
            _ => perry_hir::infer_expr_type(expr, env),
        },
        Expr::IndexGet { object, .. } => match expression(object, env) {
            HirType::Array(element) => *element,
            _ => perry_hir::infer_expr_type(expr, env),
        },
        _ => perry_hir::infer_expr_type(expr, env),
    };
    if contains_type_var(&ty) {
        HirType::Any
    } else {
        ty
    }
}

struct FunctionEnv<'a> {
    base: &'a perry_hir::HirTypeEnv,
    interfaces: &'a [perry_hir::ir::Interface],
    type_aliases: &'a [perry_hir::ir::TypeAlias],
    locals: &'a mut HashMap<LocalId, HirType>,
    integer_aliases: &'a HashMap<String, HirType>,
}

const NUMBER_TYPE: HirType = HirType::Number;
const BIGINT_TYPE: HirType = HirType::BigInt;

impl FunctionEnv<'_> {
    fn normalize<'t>(&self, ty: &'t HirType) -> &'t HirType {
        match ty {
            HirType::Named(name) => match self.integer_aliases.get(name) {
                Some(HirType::BigInt) => &BIGINT_TYPE,
                Some(_) => &NUMBER_TYPE,
                None => ty,
            },
            ty => ty,
        }
    }
}

impl HirTypeFacts for FunctionEnv<'_> {
    fn local_type(&self, id: LocalId) -> Option<&HirType> {
        self.locals
            .get(&id)
            .or_else(|| self.base.local_type(id))
            .map(|ty| self.normalize(ty))
    }

    fn global_type(&self, id: perry_hir::types::GlobalId) -> Option<&HirType> {
        self.base.global_type(id).map(|ty| self.normalize(ty))
    }

    fn function_return_type(&self, id: FuncId) -> Option<&HirType> {
        self.base
            .function_return_type(id)
            .map(|ty| self.normalize(ty))
    }

    fn extern_function_return_type(&self, name: &str) -> Option<&HirType> {
        self.base
            .extern_function_return_type(name)
            .map(|ty| self.normalize(ty))
    }

    fn this_type(&self) -> Option<HirType> {
        self.base.this_type()
    }

    fn enum_member_type(&self, enum_name: &str, member_name: &str) -> Option<HirType> {
        self.base.enum_member_type(enum_name, member_name)
    }

    fn static_field_type(&self, class_name: &str, field_name: &str) -> Option<&HirType> {
        self.base
            .static_field_type(class_name, field_name)
            .map(|ty| self.normalize(ty))
    }

    fn static_method_return_type(&self, class_name: &str, method_name: &str) -> Option<&HirType> {
        self.base
            .static_method_return_type(class_name, method_name)
            .map(|ty| self.normalize(ty))
    }

    fn named_property_type(&self, type_name: &str, property: &str) -> Option<HirType> {
        self.base
            .named_property_type(type_name, property)
            .map(|ty| match ty {
                HirType::Named(name) => self
                    .integer_aliases
                    .get(&name)
                    .cloned()
                    .unwrap_or(HirType::Named(name)),
                ty => ty,
            })
    }

    fn super_property_type(&self, property: &str) -> Option<HirType> {
        self.base.super_property_type(property)
    }

    fn super_method_return_type(&self, method: &str) -> Option<HirType> {
        self.base.super_method_return_type(method)
    }
}

enum ScanMode<'a> {
    Returns(&'a mut Vec<HirType>),
    ResolveLocals,
}

fn collect_integer_aliases(
    hir: &HirModule,
    foreign: &HashMap<String, HirType>,
) -> HashMap<String, HirType> {
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
    let user_types: HashSet<&str> = hir
        .interfaces
        .iter()
        .map(|i| i.name.as_str())
        .chain(hir.type_aliases.iter().map(|a| a.name.as_str()))
        .chain(hir.classes.iter().map(|c| c.name.as_str()))
        .collect();
    for import in &hir.imports {
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
            for candidate in [
                format!("wit:{}#{imported}", import.source),
                format!("wit:{imported}"),
            ] {
                if let Some(HirType::Named(target)) = foreign.get(&candidate)
                    && let Some(carrier) = names.get(target).cloned()
                {
                    names.insert(local.clone(), carrier);
                    break;
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
    names
}

fn clear_inferred_type_vars<V>(
    statements: &mut [Stmt],
    inferred: &HashSet<LocalId>,
    spans: &HashMap<LocalId, V>,
    integer_aliases: &HashMap<String, HirType>,
) {
    for statement in statements {
        match statement {
            Stmt::Let { id, ty, .. } if inferred.contains(id) || !spans.contains_key(id) => {
                if contains_type_var(ty) {
                    *ty = HirType::Any;
                } else if let HirType::Named(name) = ty
                    && let Some(carrier) = integer_aliases.get(name)
                {
                    *ty = carrier.clone();
                }
            }
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                clear_inferred_type_vars(then_branch, inferred, spans, integer_aliases);
                if let Some(body) = else_branch {
                    clear_inferred_type_vars(body, inferred, spans, integer_aliases);
                }
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
                clear_inferred_type_vars(body, inferred, spans, integer_aliases);
            }
            Stmt::For { init, body, .. } => {
                if let Some(init) = init {
                    clear_inferred_type_vars(
                        std::slice::from_mut(init),
                        inferred,
                        spans,
                        integer_aliases,
                    );
                }
                clear_inferred_type_vars(body, inferred, spans, integer_aliases);
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                clear_inferred_type_vars(body, inferred, spans, integer_aliases);
                if let Some(catch) = catch {
                    clear_inferred_type_vars(&mut catch.body, inferred, spans, integer_aliases);
                }
                if let Some(body) = finally {
                    clear_inferred_type_vars(body, inferred, spans, integer_aliases);
                }
            }
            Stmt::Switch { cases, .. } => {
                for case in cases {
                    clear_inferred_type_vars(&mut case.body, inferred, spans, integer_aliases);
                }
            }
            Stmt::Labeled { body, .. } => {
                clear_inferred_type_vars(
                    std::slice::from_mut(body),
                    inferred,
                    spans,
                    integer_aliases,
                );
            }
            _ => {}
        }
    }
}

fn scan(statements: &mut [Stmt], env: &mut FunctionEnv<'_>, mode: &mut ScanMode<'_>) {
    for statement in statements {
        match statement {
            Stmt::Let {
                id,
                ty,
                init: Some(init),
                ..
            } if matches!(ty, HirType::Any | HirType::Unknown)
                && !matches!(init, Expr::Undefined) =>
            {
                let inferred = expression(init, env);
                if matches!(mode, ScanMode::ResolveLocals) && is_resolvable_type(&inferred) {
                    *ty = inferred.clone();
                }
                if !matches!(inferred, HirType::Any | HirType::Unknown) {
                    env.locals.insert(*id, inferred);
                }
            }
            Stmt::Expr(Expr::LocalSet(id, value))
                if env
                    .base
                    .local_type(*id)
                    .is_none_or(|ty| matches!(ty, HirType::Any | HirType::Unknown))
                    && !env.locals.contains_key(id) =>
            {
                let inferred = expression(value, env);
                if !matches!(inferred, HirType::Any | HirType::Unknown | HirType::Void) {
                    env.locals.insert(*id, inferred);
                }
            }
            Stmt::Return(value) => {
                if let ScanMode::Returns(returned) = mode {
                    returned.push(
                        value
                            .as_ref()
                            .map_or(HirType::Void, |value| expression(value, env)),
                    );
                }
            }
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                scan(then_branch, env, mode);
                if let Some(body) = else_branch {
                    scan(body, env, mode);
                }
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => scan(body, env, mode),
            Stmt::For { init, body, .. } => {
                if let Some(init) = init {
                    scan(std::slice::from_mut(init), env, mode);
                }
                scan(body, env, mode);
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                scan(body, env, mode);
                if let Some(catch) = catch {
                    scan(&mut catch.body, env, mode);
                }
                if let Some(body) = finally {
                    scan(body, env, mode);
                }
            }
            Stmt::Switch { cases, .. } => {
                for case in cases {
                    scan(&mut case.body, env, mode);
                }
            }
            Stmt::Labeled { body, .. } => scan(std::slice::from_mut(body), env, mode),
            _ => {}
        }
    }
}

fn push_return_type(
    types: &mut Vec<HirType>,
    ty: HirType,
    integer_aliases: &HashMap<String, HirType>,
) {
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
            .cloned()
            .unwrap_or(HirType::Named(name)),
        HirType::StringLiteral(_) => HirType::String,
        ty => ty,
    };
    if !types.contains(&normalized) {
        types.push(normalized);
    }
}

pub(super) fn unresolved_types(
    hir: &mut HirModule,
    inferred: &HashSet<LocalId>,
    locations: &crate::locations::Locations<'_>,
    foreign: &HashMap<String, HirType>,
) {
    let integer_aliases = collect_integer_aliases(hir, foreign);
    clear_inferred_type_vars(
        &mut hir.init,
        inferred,
        &hir.local_source_spans,
        &integer_aliases,
    );
    for function in &mut hir.functions {
        clear_inferred_type_vars(
            &mut function.body,
            inferred,
            &hir.local_source_spans,
            &integer_aliases,
        );
    }
    let mut locals = HashMap::new();
    let mut module_env = perry_hir::HirTypeEnv::from_module(hir);
    for _ in 0..=hir.functions.len() {
        let mut changed = false;
        for function in &mut hir.functions {
            if locations.has_explicit_return(&function.name) {
                continue;
            }
            locals.clear();
            for param in &function.params {
                locals.insert(param.id, param.ty.clone());
            }
            let mut env = FunctionEnv {
                base: &module_env,
                interfaces: &hir.interfaces,
                type_aliases: &hir.type_aliases,
                locals: &mut locals,
                integer_aliases: &integer_aliases,
            };
            let mut returned = Vec::new();
            scan(
                &mut function.body,
                &mut env,
                &mut ScanMode::Returns(&mut returned),
            );
            if function.is_async {
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
                continue;
            }
            if array_methods::returns_optional(&function.body) {
                returned.push(HirType::Void);
            }
            let mut types = Vec::new();
            for ty in returned {
                push_return_type(&mut types, ty, &integer_aliases);
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
                _ => continue,
            };
            let inferred = if function.is_async {
                HirType::Promise(Box::new(ty))
            } else {
                ty
            };
            if inferred != function.return_type {
                function.return_type = inferred;
                changed = true;
            }
        }
        if !changed {
            break;
        }
        module_env = perry_hir::HirTypeEnv::from_module(hir);
    }
    locals.clear();
    let mut env = FunctionEnv {
        base: &module_env,
        interfaces: &hir.interfaces,
        type_aliases: &hir.type_aliases,
        locals: &mut locals,
        integer_aliases: &integer_aliases,
    };
    scan(&mut hir.init, &mut env, &mut ScanMode::ResolveLocals);
    for function in &mut hir.functions {
        locals.clear();
        for param in &function.params {
            locals.insert(param.id, param.ty.clone());
        }
        let mut env = FunctionEnv {
            base: &module_env,
            interfaces: &hir.interfaces,
            type_aliases: &hir.type_aliases,
            locals: &mut locals,
            integer_aliases: &integer_aliases,
        };
        scan(&mut function.body, &mut env, &mut ScanMode::ResolveLocals);
    }
}
