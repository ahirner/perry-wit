use super::*;

pub(super) fn is_resolvable_type(ty: &HirType) -> bool {
    match ty {
        HirType::Any
        | HirType::Unknown
        | HirType::Void
        | HirType::Null
        | HirType::Never
        | HirType::TypeVar(_) => false,
        HirType::Named(name) => !hir_compat::is_object_literal(name),
        HirType::Array(inner) | HirType::Promise(inner) => is_resolvable_type(inner),
        HirType::Tuple(items) => items.iter().all(is_resolvable_type),
        HirType::Union(items) => {
            let mut has_defined = false;
            for item in items {
                if !matches!(item, HirType::Void | HirType::Null) {
                    if !is_resolvable_type(item) {
                        return false;
                    }
                    has_defined = true;
                }
            }
            has_defined
        }
        HirType::Generic { base, type_args } => {
            !matches!(base.as_str(), "Map" | "Set" | "Record" if type_args.is_empty())
                && type_args.iter().all(is_resolvable_type)
        }
        HirType::Object(object) => {
            object
                .index_signature
                .as_ref()
                .is_none_or(|v| is_resolvable_type(v))
                && object
                    .properties
                    .values()
                    .all(|p| is_resolvable_type(&p.ty))
        }
        _ => true,
    }
}

pub(super) fn strip_nullish(ty: HirType) -> (HirType, Option<Absence>) {
    match ty {
        HirType::Null => (HirType::Void, Some(Absence::Null)),
        HirType::Void => (HirType::Void, Some(Absence::Undefined)),
        HirType::Union(mut items) => {
            let mut absences = None;
            items.retain(|item| {
                let state = match item {
                    HirType::Null => Some(Absence::Null),
                    HirType::Void => Some(Absence::Undefined),
                    _ => None,
                };
                absences = merge_absence(absences, state);
                state.is_none()
            });
            let inner = match items.len() {
                0 => HirType::Void,
                1 => items.swap_remove(0),
                _ => HirType::Union(items),
            };
            (inner, absences)
        }
        other => (other, None),
    }
}

pub(super) fn without_undefined(states: Option<Absence>) -> Option<Absence> {
    states
        .filter(|state| state.admits_null())
        .map(|_| Absence::Null)
}

pub(super) fn with_absences(ty: HirType, states: impl Into<Option<Absence>>) -> HirType {
    let Some(states) = states.into() else {
        return ty;
    };
    let (inner, existing) = strip_nullish(ty);
    if matches!(inner, HirType::Any | HirType::Unknown | HirType::Void) {
        return inner;
    }
    let states = merge_absence(existing, Some(states)).unwrap_or(states);
    let mut items = match inner {
        HirType::Union(members) => members,
        other => vec![other],
    };
    if states.admits_null() {
        items.push(HirType::Null);
    }
    if states.admits_undefined() {
        items.push(HirType::Void);
    }
    HirType::Union(items)
}
