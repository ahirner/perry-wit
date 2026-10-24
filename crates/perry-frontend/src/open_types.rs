//! Origins of unresolved source types at the frontend boundary.
use crate::{hir_compat::Metadata, infer::walk, shapes};
use perry_hir::{
    ir::{Expr, Function, Module},
    types::{FuncId, LocalId, Type},
};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TypeState<'t> {
    Open,
    Dynamic,
    Resolved(&'t Type),
}

impl<'t> TypeState<'t> {
    pub fn resolved(self) -> Option<&'t Type> {
        match self {
            Self::Open | Self::Dynamic => None,
            Self::Resolved(ty) => Some(ty),
        }
    }
}

#[derive(Default)]
pub(crate) struct OpenTypes {
    bindings: HashSet<LocalId>,
    returns: HashSet<FuncId>,
}

impl OpenTypes {
    pub(crate) fn binding<'t>(&self, id: LocalId, ty: &'t Type) -> TypeState<'t> {
        if self.bindings.contains(&id)
            && (requires_context(ty) || crate::infer::contains_type_var(ty))
        {
            TypeState::Open
        } else if requires_context(ty) {
            TypeState::Dynamic
        } else {
            TypeState::Resolved(ty)
        }
    }

    pub(crate) fn parameter<'t>(&self, id: LocalId, ty: &'t Type) -> TypeState<'t> {
        if self.bindings.contains(&id) && contextual_type(ty).resolved().is_none() {
            TypeState::Open
        } else {
            contextual_type(ty)
        }
    }

    pub(crate) fn result<'t>(&self, function: &Function, ty: &'t Type) -> TypeState<'t> {
        if self.returns.contains(&function.id) && contextual_type(ty).resolved().is_none() {
            TypeState::Open
        } else {
            contextual_type(ty)
        }
    }
}

pub fn contextual_type(ty: &Type) -> TypeState<'_> {
    if matches!(ty, Type::Any | Type::Unknown) {
        TypeState::Dynamic
    } else {
        TypeState::Resolved(ty)
    }
}

fn requires_context(ty: &Type) -> bool {
    match ty {
        Type::Any | Type::Unknown | Type::Never | Type::TypeVar(_) => true,
        Type::Array(element) => matches!(
            **element,
            Type::Any | Type::Unknown | Type::Never | Type::TypeVar(_)
        ),
        Type::Generic { base, type_args }
            if matches!(
                base.as_str(),
                "Map"
                    | "ReadonlyMap"
                    | "MapKeys"
                    | "MapValues"
                    | "MapEntries"
                    | "Set"
                    | "ReadonlySet"
                    | "SetValues"
                    | "SetEntries"
                    | "Array"
                    | "ReadonlyArray"
                    | "Record"
            ) =>
        {
            type_args.is_empty()
                || type_args
                    .iter()
                    .any(|ty| matches!(ty, Type::Any | Type::Unknown | Type::TypeVar(_)))
        }
        _ => false,
    }
}

pub(crate) fn mark(module: &mut Module, metadata: &mut Metadata<'_>) {
    let mut facts = OpenTypes::default();
    let mut visit = |node: walk::Node<'_>| {
        if let walk::Node::Binding(id, ty) = node
            && (metadata.unannotated_bindings.contains(id)
                || !module.local_source_spans.contains_key(id))
            && (requires_context(ty) || crate::infer::contains_type_var(ty))
        {
            facts.bindings.insert(*id);
        }
        Ok(())
    };
    let _ = walk::visit_body(&mut module.init, &mut visit);
    for function in &mut module.functions {
        let _ = walk::function(function, &mut visit);
        if !metadata.locations.has_explicit_return(&function.name)
            && requires_context(&function.return_type)
        {
            facts.returns.insert(function.id);
        }
    }
    for class in &mut module.classes {
        for function in shapes::class_functions_mut(class) {
            let _ = walk::function(function, &mut visit);
        }
    }
    for class in &module.classes {
        for function in shapes::class_functions(class) {
            if !metadata
                .locations
                .has_explicit_return(&format!("{}::{}", class.name, function.name))
                && requires_context(&function.return_type)
            {
                facts.returns.insert(function.id);
            }
        }
    }
    metadata.open_types = facts;
}

pub fn has_reader_overload(initializer: &Expr) -> bool {
    matches!(initializer, Expr::NativeMethodCall { method, args, .. }
        if method == "getReader" && !args.is_empty())
}
