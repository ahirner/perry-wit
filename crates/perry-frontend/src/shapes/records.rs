use perry_hir::ir::Class;
use std::collections::HashMap;

pub(crate) struct Classes<'h> {
    pub(crate) named: HashMap<&'h str, (usize, &'h Class)>,
    literal_fields: HashMap<&'h str, Vec<&'h str>>,
}

impl<'h> Classes<'h> {
    pub(crate) fn new(classes: &'h [Class]) -> Self {
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

    pub(crate) fn literal_fields(&self, name: &str) -> Option<&[&'h str]> {
        self.literal_fields.get(name).map(Vec::as_slice)
    }
}
