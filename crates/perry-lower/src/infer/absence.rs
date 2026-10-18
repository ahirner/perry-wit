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
            items
                .iter()
                .any(|item| !matches!(item, HirType::Void | HirType::Null))
                && items.iter().all(|item| {
                    matches!(item, HirType::Void | HirType::Null) || is_resolvable_type(item)
                })
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

pub(super) fn strip_nullish(ty: HirType) -> (HirType, Vec<HirType>) {
    match ty {
        HirType::Null => (HirType::Void, vec![HirType::Null]),
        HirType::Void => (HirType::Void, vec![HirType::Void]),
        HirType::Union(items) => {
            let mut defined = Vec::new();
            let mut absences = Vec::new();
            for item in items {
                if matches!(item, HirType::Null | HirType::Void) {
                    if !absences.contains(&item) {
                        absences.push(item);
                    }
                } else {
                    defined.push(item);
                }
            }
            let inner = match defined.len() {
                0 => HirType::Void,
                1 => defined.swap_remove(0),
                _ => HirType::Union(defined),
            };
            (inner, absences)
        }
        other => (other, Vec::new()),
    }
}

pub(super) fn with_absences(ty: HirType, absences: Vec<HirType>) -> HirType {
    if absences.is_empty() {
        return ty;
    }
    let (inner, mut existing) = strip_nullish(ty);
    for a in absences {
        if !existing.contains(&a) {
            existing.push(a);
        }
    }
    if matches!(inner, HirType::Any | HirType::Unknown | HirType::Void) {
        return inner;
    }
    let mut items = match inner {
        HirType::Union(members) => members,
        other => vec![other],
    };
    items.extend(existing);
    HirType::Union(items)
}
