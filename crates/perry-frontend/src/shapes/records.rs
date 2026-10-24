use super::{TypeId, generics::InstanceKey};
use perry_hir::ir::Class;
use std::collections::HashMap;

pub struct Classes<'h> {
    pub named: HashMap<&'h str, (usize, &'h Class)>,
    literal_fields: HashMap<&'h str, Vec<&'h str>>,
}

impl<'h> Classes<'h> {
    pub fn new(classes: &'h [Class]) -> Self {
        let mut named = HashMap::new();
        let mut literal_fields = HashMap::new();
        for (index, class) in classes.iter().enumerate() {
            if crate::hir_compat::is_object_literal(&class.name) {
                literal_fields.insert(
                    class.name.as_str(),
                    class
                        .fields
                        .iter()
                        .map(|field| field.name.as_str())
                        .collect(),
                );
            } else {
                named.insert(class.name.as_str(), (index, class));
            }
        }
        Self {
            named,
            literal_fields,
        }
    }

    pub fn literal_fields(&self, name: &str) -> Option<&[&'h str]> {
        self.literal_fields.get(name).map(Vec::as_slice)
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Identity {
    Structural(Vec<(String, TypeId, bool)>),
    Nominal(InstanceKey),
}

/// Record identity within one source declaration set, independent of its MIR carrier.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordId {
    Structural(usize),
    Nominal(usize),
}

#[derive(Default)]
pub(super) struct Records(HashMap<Identity, RecordId>);

impl Records {
    pub(super) fn intern(
        &mut self,
        instance: Option<&InstanceKey>,
        fields: &mut [(String, TypeId, bool)],
    ) -> RecordId {
        fields.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        let identity = match instance {
            Some(key @ (super::generics::DeclarationId::Class(_), _)) => {
                Identity::Nominal(key.clone())
            }
            _ => Identity::Structural(fields.to_vec()),
        };
        let next = match identity {
            Identity::Structural(_) => RecordId::Structural(self.0.len()),
            Identity::Nominal(_) => RecordId::Nominal(self.0.len()),
        };
        *self.0.entry(identity).or_insert(next)
    }
}

impl RecordId {
    pub fn is_nominal(self) -> bool {
        matches!(self, Self::Nominal(_))
    }
}
