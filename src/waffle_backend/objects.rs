//! Plain data objects retain property order, identity, and tagged field values.

use super::{allocation::AllocationFuncs, runtime};
use anyhow::{Result, ensure};
use perry_hir::{
    ir::{Module as HirModule, Stmt},
    types::{ObjectType, PropertyInfo, Type as HirType},
};
use std::collections::{BTreeMap, BTreeSet};
use waffle::{Func, Memory, Module};

#[derive(Clone, Copy)]
pub(crate) struct ObjectHelpers {
    pub(crate) new: Func,
    pub(crate) set: Func,
    pub(crate) get: Func,
    pub(crate) delete: Func,
    pub(crate) value: Func,
    pub(crate) dynamic: Func,
}

pub(crate) fn is_object(ty: &HirType) -> bool {
    matches!(ty, HirType::Object(_))
}

pub(crate) fn contains_object(ty: &HirType) -> bool {
    match ty {
        HirType::Object(_) => true,
        HirType::Promise(inner) | HirType::Array(inner) => contains_object(inner),
        HirType::Union(types)
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

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    compare: Func,
    boxed_value: Func,
) -> Result<ObjectHelpers> {
    let functions = runtime::emit_functions(
        module,
        memory,
        include_str!("objects/runtime.wat"),
        &BTreeMap::from([
            ("realloc", allocator.realloc),
            ("compare", compare),
            ("box", boxed_value),
        ]),
    )?;
    Ok(ObjectHelpers {
        new: functions["object.new"],
        set: functions["object.set"],
        get: functions["object.get"],
        delete: functions["object.delete"],
        value: functions["object.value"],
        dynamic: functions["object.dynamic"],
    })
}
