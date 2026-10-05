//! Nullable WIT values keep a finite declared payload type in a tagged box.

use perry_hir::types::Type;

pub(super) fn inner(ty: &Type) -> Option<&Type> {
    if super::values::sentinel_inner(ty).is_some() {
        return None;
    }
    let Type::Union(types) = ty else {
        return None;
    };
    if types.len() < 2 || types.len() > 3 {
        return None;
    }
    let non_null_count = types
        .iter()
        .filter(|t| !matches!(t, Type::Null | Type::Void))
        .count();
    if non_null_count != 1 {
        return None;
    }
    types.iter().find(|t| !matches!(t, Type::Null | Type::Void))
}
