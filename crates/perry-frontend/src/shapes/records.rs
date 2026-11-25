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
        let (source, selected) = match self {
            Self::Source(c) => (c.fields.as_slice(), &[][..]),
            Self::Instance(c) => (&[][..], c.fields.as_slice()),
        };
        source
            .iter()
            .map(|field| (field, None))
            .chain(selected.iter().copied())
    }
}
