//! Plain data objects retain property order, identity, and tagged field values.

use super::{allocation::AllocationFuncs, runtime};
use anyhow::{Result, ensure};
use perry_hir::{
    ir::{Expr, Module as HirModule, Stmt},
    types::{ObjectType, PropertyInfo, Type as HirType},
};
use std::collections::{BTreeMap, BTreeSet};
use waffle::{Func, Memory, Module};

pub(crate) const INFERRED_RECORD_TYPE: &str = "__perry_inferred_record";

#[derive(Clone, Copy)]
pub(crate) struct ObjectHelpers {
    pub(crate) new: Func,
    pub(crate) record: Func,
    pub(crate) set: Func,
    pub(crate) get: Func,
    pub(crate) value: Func,
    pub(crate) dynamic: Func,
    pub(crate) enumerate: Func,
    pub(crate) assign: Func,
    pub(crate) spread: Func,
}

/// Recover the frontend's ordered object-literal IIFE without allocating a closure.
pub(crate) fn spread_parts(expression: &Expr) -> Option<Vec<(Option<&str>, &Expr)>> {
    let Expr::Call { callee, args, .. } = expression else {
        return None;
    };
    let Expr::Closure { params, body, .. } = callee.as_ref() else {
        return None;
    };
    let [parameter] = params.as_slice() else {
        return None;
    };
    if parameter.name != "__perry_obj_iife"
        || !matches!(args.as_slice(), [Expr::Object(fields)] if fields.is_empty())
    {
        return None;
    }
    let (Stmt::Return(Some(Expr::LocalGet(id))), fields) = body.split_last()? else {
        return None;
    };
    if *id != parameter.id {
        return None;
    }
    fields.iter().map(|field| match field {
        Stmt::Expr(Expr::Call { callee, args, .. })
            if matches!(callee.as_ref(), Expr::ExternFuncRef {name, ..} if name == "js_object_assign_one") => {
                let [Expr::LocalGet(id), source] = args.as_slice() else { return None };
                (*id == parameter.id).then_some((None, source))
            }
        Stmt::Expr(Expr::IndexSet {object, index, value})
            if matches!(object.as_ref(), Expr::LocalGet(id) if *id == parameter.id) => {
                let Expr::String(name) = index.as_ref() else { return None };
                Some((Some(name.as_str()), value.as_ref()))
            }
        _ => None,
    }).collect()
}

pub(crate) fn property_type(field: &PropertyInfo) -> HirType {
    if !field.optional || super::values::is_dynamic(&field.ty) {
        return field.ty.clone();
    }
    let mut variants = match &field.ty {
        HirType::Union(variants) => variants.clone(),
        ty => vec![ty.clone()],
    };
    if !variants.contains(&HirType::Void) {
        variants.push(HirType::Void);
    }
    if variants.len() == 1 {
        variants.remove(0)
    } else {
        HirType::Union(variants)
    }
}

pub(crate) fn is_object(ty: &HirType) -> bool {
    matches!(ty, HirType::Object(_))
        || super::context::is_environment(ty)
        || matches!(ty, HirType::Union(types) if !types.is_empty() && types.iter().all(is_object))
}

pub(crate) fn contains_object(ty: &HirType) -> bool {
    match ty {
        ty if is_object(ty) => true,
        HirType::Promise(inner) | HirType::Array(inner) => contains_object(inner),
        HirType::Union(types)
        | HirType::Tuple(types)
        | HirType::Generic {
            type_args: types, ..
        } => types.iter().any(contains_object),
        _ => false,
    }
}

pub(crate) fn resolve_declared_types(hir: &mut HirModule) -> Result<()> {
    let mut definitions: BTreeMap<_, _> = hir
        .type_aliases
        .iter()
        .filter(|alias| alias.type_params.is_empty())
        .map(|alias| (alias.name.clone(), alias.ty.clone()))
        .collect();
    for interface in &hir.interfaces {
        if !interface.type_params.is_empty()
            || !interface.extends.is_empty()
            || !interface.methods.is_empty()
        {
            continue;
        }
        definitions.insert(
            interface.name.clone(),
            HirType::Object(ObjectType {
                name: Some(interface.name.clone()),
                properties: interface
                    .properties
                    .iter()
                    .map(|property| {
                        (
                            property.name.clone(),
                            PropertyInfo {
                                ty: property.ty.clone(),
                                optional: property.optional,
                                readonly: property.readonly,
                            },
                        )
                    })
                    .collect(),
                property_order: Some(
                    interface
                        .properties
                        .iter()
                        .map(|property| property.name.clone())
                        .collect(),
                ),
                index_signature: None,
            }),
        );
    }
    for function in &mut hir.functions {
        for parameter in &mut function.params {
            resolve_type(&mut parameter.ty, &definitions, &mut BTreeSet::new())?;
        }
        resolve_type(
            &mut function.return_type,
            &definitions,
            &mut BTreeSet::new(),
        )?;
        let mut result = Ok(());
        super::visit::visit_statement_nodes_mut(&mut function.body, &mut |statement| {
            if result.is_ok()
                && let Stmt::Let { ty, .. } = statement
            {
                result = resolve_type(ty, &definitions, &mut BTreeSet::new());
            }
        });
        result?;
    }
    Ok(())
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    compare: Func,
    boxed_value: Func,
) -> Result<ObjectHelpers> {
    let wat = format!(
        "{} {})",
        include_str!("objects/runtime.wat")
            .trim_end()
            .strip_suffix(')')
            .unwrap(),
        include_str!("objects/enumerate.wat")
    );
    let imports = BTreeMap::from([
        ("realloc", allocator.realloc),
        ("compare", compare),
        ("box", boxed_value),
    ]);
    let functions = runtime::emit_functions(module, memory, &wat, &imports)?;
    Ok(ObjectHelpers {
        new: functions["object.new"],
        record: functions["object.record"],
        set: functions["object.set"],
        get: functions["object.get"],
        value: functions["object.value"],
        dynamic: functions["object.dynamic"],
        enumerate: functions["object.enumerate"],
        assign: functions["object.assign"],
        spread: functions["object.spread"],
    })
}

fn resolve_type(
    ty: &mut HirType,
    definitions: &BTreeMap<String, HirType>,
    visiting: &mut BTreeSet<String>,
) -> Result<()> {
    match ty {
        HirType::Named(name) if definitions.contains_key(name) => {
            let name = name.clone();
            ensure!(
                visiting.insert(name.clone()),
                "Recursive object type '{name}' requires recursive type lowering"
            );
            *ty = definitions[&name].clone();
            resolve_type(ty, definitions, visiting)?;
            visiting.remove(&name);
        }
        HirType::Array(inner) | HirType::Promise(inner) => {
            resolve_type(inner, definitions, visiting)?
        }
        HirType::Union(types)
        | HirType::Tuple(types)
        | HirType::Generic {
            type_args: types, ..
        } => {
            for ty in types {
                resolve_type(ty, definitions, visiting)?;
            }
        }
        HirType::Object(object) => {
            for property in object.properties.values_mut() {
                resolve_type(&mut property.ty, definitions, visiting)?;
            }
            if let Some(index) = &mut object.index_signature {
                resolve_type(index, definitions, visiting)?;
            }
        }
        _ => {}
    }
    Ok(())
}
