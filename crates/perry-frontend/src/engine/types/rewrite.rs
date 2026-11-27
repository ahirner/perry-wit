use super::{Ty, TyKind, TypeListId, TypeTable};

pub(in crate::engine) trait TypeContext {
    fn types(&mut self) -> &mut TypeTable;

    fn try_map_children<E>(
        &mut self,
        ty: Ty,
        mut map: impl FnMut(&mut Self, Ty) -> Result<Ty, E>,
    ) -> Result<Ty, E> {
        let original = self.types().kind(ty);
        let mut kind = original;
        if let TyKind::Object { props, .. } | TyKind::Signature { params: props, .. } = &mut kind {
            let mut fields = self.types().object_slice(*props).to_vec();
            for field in &mut fields {
                field.ty = map(self, field.ty)?;
            }
            *props = self.types().intern_properties(&fields);
        }
        match &mut kind {
            TyKind::Array(inner)
            | TyKind::Promise(inner)
            | TyKind::Signature { result: inner, .. } => *inner = map(self, *inner)?,
            TyKind::Object {
                index: Some(index), ..
            } => *index = map(self, *index)?,
            TyKind::Tuple(span) | TyKind::Union(span) | TyKind::Generic { args: span, .. } => {
                let mut items = self.types().ty_slice(*span).to_vec();
                for item in &mut items {
                    *item = map(self, *item)?;
                }
                let types = self.types();
                if items == types.ty_slice(*span) {
                    return Ok(ty);
                }
                if matches!(original, TyKind::Union(_)) {
                    return Ok(types.union(&items));
                }
                *span = TypeListId::intern(&mut types.members, &items);
            }
            _ => {}
        }
        Ok(if original == kind {
            ty
        } else {
            self.types().intern_kind(kind)
        })
    }
}

impl TypeContext for TypeTable {
    fn types(&mut self) -> &mut TypeTable {
        self
    }
}
