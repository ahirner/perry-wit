//! Conservative completion-aware inference for unresolved local helper returns.
use super::*;

use perry_hir::HirTypeFacts;
use std::collections::HashMap;

pub(super) fn contains_type_var(ty: &HirType) -> bool {
    match ty {
        HirType::TypeVar(_) => true,
        HirType::Array(inner) | HirType::Promise(inner) => contains_type_var(inner),
        HirType::Tuple(items) | HirType::Union(items) => items.iter().any(contains_type_var),
        HirType::Generic { type_args, .. } => type_args.iter().any(contains_type_var),
        HirType::Object(object) => object.properties.values().any(|p| contains_type_var(&p.ty)),
        _ => false,
    }
}

mod expressions;
use expressions::*;

struct FunctionEnv<'a> {
    ctx: &'a Context,
    base: &'a perry_hir::HirTypeEnv,
    interfaces: &'a [perry_hir::ir::Interface],
    type_aliases: &'a [perry_hir::ir::TypeAlias],
    locals: &'a mut HashMap<LocalId, HirType>,
    integer_aliases: &'a HashMap<String, HirType>,
    imported_types: &'a HashMap<String, String>,
    foreign: &'a HashMap<String, crate::hir::Foreign>,
    metadata: &'a hir_compat::Metadata<'a>,
}

const NUMBER_TYPE: HirType = HirType::Number;
const BIGINT_TYPE: HirType = HirType::BigInt;

impl<'a> FunctionEnv<'a> {
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

    fn normalize_owned(&self, ty: HirType) -> HirType {
        match ty {
            HirType::Named(name) => self
                .integer_aliases
                .get(&name)
                .cloned()
                .unwrap_or(HirType::Named(name)),
            HirType::Array(inner) => HirType::Array(Box::new(self.normalize_owned(*inner))),
            HirType::Promise(inner) => HirType::Promise(Box::new(self.normalize_owned(*inner))),
            HirType::Tuple(items) => {
                HirType::Tuple(items.into_iter().map(|t| self.normalize_owned(t)).collect())
            }
            HirType::Union(items) => {
                HirType::Union(items.into_iter().map(|t| self.normalize_owned(t)).collect())
            }
            ty => ty,
        }
    }

    fn resolve_foreign_type(&self, type_name: &str) -> Option<&'a HirType> {
        let mut current = self
            .imported_types
            .get(type_name)
            .map_or(type_name, String::as_str);
        for _ in 0..8 {
            match self.foreign.get(current)? {
                crate::hir::Foreign::Type(HirType::Named(next)) if next != current => {
                    current = next;
                }
                crate::hir::Foreign::Type(ty) => return Some(ty),
                _ => return None,
            }
        }
        None
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
        let ty = self
            .base
            .named_property_type(type_name, property)
            .or_else(|| {
                let HirType::Object(object) = self.resolve_foreign_type(type_name)? else {
                    return None;
                };
                let prop = object.properties.get(property)?;
                Some(shapes::generics::optional_property(
                    prop.ty.clone(),
                    prop.optional,
                ))
            })?;
        Some(self.normalize_owned(ty))
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
    for statement in statements {
        match statement {
            Stmt::Let { id, ty, .. } if inferred.contains(id) || !spans.contains_key(id) => {
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
            } if (matches!(&*ty, HirType::Any | HirType::Unknown)
                || matches!(
                    &*ty,
                    HirType::Generic { base, type_args }
                        if matches!(base.as_str(), "Map" | "Set") && type_args.is_empty()
                ))
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
            Stmt::Let {
                init: Some(init), ..
            } => {
                let _ = expression(init, env);
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
            Stmt::Expr(expr) | Stmt::Throw(expr) => {
                let _ = expression(expr, env);
            }
            Stmt::Return(value) => {
                let ret_ty = value
                    .as_ref()
                    .map_or(HirType::Void, |value| expression(value, env));
                if let ScanMode::Returns(returned) = mode {
                    returned.push(ret_ty);
                }
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let _ = expression(condition, env);
                scan(then_branch, env, mode);
                if let Some(body) = else_branch {
                    scan(body, env, mode);
                }
            }
            Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                let _ = expression(condition, env);
                scan(body, env, mode);
            }
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => {
                if let Some(init) = init {
                    scan(std::slice::from_mut(init), env, mode);
                }
                if let Some(condition) = condition {
                    let _ = expression(condition, env);
                }
                if let Some(update) = update {
                    let _ = expression(update, env);
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
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                let _ = expression(discriminant, env);
                for case in cases {
                    if let Some(test) = &case.test {
                        let _ = expression(test, env);
                    }
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
        clear_inferred_type_vars(
            &mut function.body,
            inferred,
            &hir.local_source_spans,
            &integer_aliases,
        );
    }
    let mut locals = HashMap::new();
    let mut module_env = perry_hir::HirTypeEnv::from_module(hir);
    {
        let mut env = FunctionEnv {
            ctx,
            base: &module_env,
            interfaces: &hir.interfaces,
            type_aliases: &hir.type_aliases,
            locals: &mut locals,
            integer_aliases: &integer_aliases,
            foreign,
            imported_types: &imported_types,
            metadata,
        };
        scan(&mut hir.init, &mut env, &mut ScanMode::ResolveLocals);
    }
    module_env = perry_hir::HirTypeEnv::from_module(hir);
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
                ctx,
                base: &module_env,
                interfaces: &hir.interfaces,
                type_aliases: &hir.type_aliases,
                locals: &mut locals,
                integer_aliases: &integer_aliases,
                foreign,
                imported_types: &imported_types,
                metadata,
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
        ctx,
        base: &module_env,
        interfaces: &hir.interfaces,
        type_aliases: &hir.type_aliases,
        locals: &mut locals,
        integer_aliases: &integer_aliases,
        foreign,
        imported_types: &imported_types,
        metadata,
    };
    scan(&mut hir.init, &mut env, &mut ScanMode::ResolveLocals);
    module_env = perry_hir::HirTypeEnv::from_module(hir);
    for function in &mut hir.functions {
        locals.clear();
        for param in &function.params {
            locals.insert(param.id, param.ty.clone());
        }
        let mut env = FunctionEnv {
            ctx,
            base: &module_env,
            interfaces: &hir.interfaces,
            type_aliases: &hir.type_aliases,
            locals: &mut locals,
            integer_aliases: &integer_aliases,
            foreign,
            imported_types: &imported_types,
            metadata,
        };
        scan(&mut function.body, &mut env, &mut ScanMode::ResolveLocals);
    }
}
