//! Nullable WIT values keep a finite declared payload type in a tagged box.

use perry_hir::types::Type;

pub(super) fn inner(ty: &Type) -> Option<&Type> {
    let Type::Union(types) = ty else {
        return None;
    };
    if types.len() != 3 || !types.contains(&Type::Null) || !types.contains(&Type::Void) {
        return None;
    }
    types
        .iter()
        .find(|ty| !matches!(ty, Type::Null | Type::Void))
}
