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
    pub(crate) fn fields(self) -> impl Iterator<Item = &'s ClassField> {
        let (source, selected): (&[ClassField], &[&ClassField]) = match self {
            Self::Source(c) => (&c.fields, &[]),
            Self::Instance(c) => (&[], &c.fields),
        };
        source.iter().chain(selected.iter().copied())
    }
}

use std::collections::HashMap;

pub(crate) struct Classes<'h> {
    pub(crate) named: HashMap<&'h str, (usize, ClassShape<'h>)>,
    literal_fields: HashMap<&'h str, Vec<&'h str>>,
}

impl<'h> Classes<'h> {
    pub(crate) fn new(classes: impl Iterator<Item = ClassShape<'h>>) -> Self {
        let mut named = HashMap::new();
        let mut literal_fields = HashMap::new();
        for (index, class) in classes.enumerate() {
            if crate::hir_compat::is_object_literal(class.name()) {
                literal_fields.insert(
                    class.name(),
                    class.fields().map(|field| field.name.as_str()).collect(),
                );
            } else {
                named.insert(class.name(), (index, class));
            }
        }
        Self {
            named,
            literal_fields,
        }
    }

    pub(crate) fn literal_fields(&self, name: &str) -> Option<&[&'h str]> {
        self.literal_fields.get(name).map(Vec::as_slice)
    }
}
