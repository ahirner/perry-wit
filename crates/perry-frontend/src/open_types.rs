//! Context needed to finish source binding types during elaboration.
use perry_hir::types::Type;

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

pub fn binding_type(ty: &Type, annotated: bool) -> TypeState<'_> {
    if requires_context(ty) || (!annotated && crate::infer::contains_type_var(ty)) {
        if annotated {
            TypeState::Dynamic
        } else {
            TypeState::Open
        }
    } else {
        TypeState::Resolved(ty)
    }
}

pub fn contextual_type(ty: &Type) -> Option<&Type> {
    (!matches!(ty, Type::Any | Type::Unknown)).then_some(ty)
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
