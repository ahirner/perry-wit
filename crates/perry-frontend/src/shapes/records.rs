use crate::source::InstanceClass;
use perry_hir::ir::{Class, ClassField};

#[derive(Clone, Copy)]
pub(crate) enum ClassShape<'s> {
    Source(&'s Class),
    Instance(&'s InstanceClass<'s>),
}

impl<'s> ClassShape<'s> {
    pub(crate) fn name(self) -> &'s str {
        match self {
            Self::Source(c) => &c.name,
            Self::Instance(c) => &c.name,
        }
    }
    pub(crate) fn parameters(self) -> &'s [perry_hir::types::TypeParam] {
        match self {
            Self::Source(c) => &c.type_params,
            Self::Instance(_) => &[],
        }
    }
    pub(crate) fn fields(
        self,
    ) -> impl Iterator<Item = (&'s ClassField, Option<crate::engine::types::Ty>)> {
        use crate::source::FieldOverride;
        let (declaration, overrides) = match self {
            Self::Source(c) => (c, &[][..]),
            Self::Instance(c) => (c.declaration, c.field_overrides.as_slice()),
        };
        let mut overrides = overrides.iter().peekable();
        declaration
            .fields
            .iter()
            .enumerate()
            .filter_map(move |(index, field)| {
                match overrides.next_if(|(position, _)| *position == index) {
                    None => Some((field, None)),
                    Some((_, FieldOverride::Solved(ty))) => Some((field, Some(*ty))),
                    Some((_, FieldOverride::Erased)) => None,
                }
            })
    }
}
