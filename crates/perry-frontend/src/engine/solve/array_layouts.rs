//! Callable array identity belongs to runtime storage. Specializations share its element layout.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct ArgumentKey(Ty);

#[derive(Default)]
pub(super) struct ArrayLayouts {
    literals: HashMap<Ty, (LocId, Ty)>,
    arguments: HashMap<Ty, (ArgumentKey, Box<[LocId]>)>,
}

impl ArrayLayouts {
    pub(super) fn contains(&self, types: &TypeTable, ty: Ty) -> bool {
        !self.literals.is_empty()
            && (self.literals.contains_key(&ty)
                || types.any_child(ty, &mut |child| self.contains(types, child)))
    }

    pub(super) fn argument_key(&mut self, types: &mut TypeTable, inputs: Ty) -> ArgumentKey {
        if let Some(&(key, _)) = self.arguments.get(&inputs) {
            return key;
        }
        if !self.contains(types, inputs) {
            return ArgumentKey(inputs);
        }
        let mut locations = Vec::new();
        let mut parameters = HashMap::new();
        let key = ArgumentKey(types.rewrite(inputs, &mut |types, ty| {
            let &(loc, element) = self.literals.get(&ty)?;
            Some(*parameters.entry(loc).or_insert_with(|| {
                let parameter = types.array_parameter(element, locations.len());
                locations.push(loc);
                parameter
            }))
        }));
        self.arguments
            .insert(inputs, (key, locations.into_boxed_slice()));
        key
    }

    fn bindings(&self, ty: Ty) -> &[LocId] {
        self.arguments.get(&ty).map_or(&[], |(_, locs)| locs)
    }
}

impl Solver<'_, '_> {
    pub(super) fn callable_array(
        &mut self,
        inst: InstanceId,
        expr: &Expr,
        element: Ty,
    ) -> Result<Ty> {
        let (site, loc, array) = self.storage_site(inst, expr, CellKind::Array)?;
        site.resolution = Some(SiteResolution::TypedArray(array));
        self.write_loc(loc, element);
        self.arrays.literals.entry(array).or_insert((loc, element));
        Ok(array)
    }

    pub(super) fn share_array_layouts(&mut self, actual: Ty, original: Ty) {
        if actual != original {
            for (&left, &right) in self
                .arrays
                .bindings(actual)
                .iter()
                .zip(self.arrays.bindings(original))
            {
                self.facts.share_elements(self.types, left, right);
            }
        }
    }

    pub(super) fn share_element_layouts(&mut self, arrays: &[Ty]) {
        let members = arrays.iter().flat_map(|ty| self.types.alternatives(ty));
        let Some(&(first, _)) = members.clone().find_map(|ty| self.arrays.literals.get(ty)) else {
            return;
        };
        let locations: Vec<_> = members
            .filter_map(|&ty| match self.types.kind(ty) {
                TyKind::Cell { loc, kind } if kind.is_array() && loc != first => Some(loc),
                _ => None,
            })
            .collect();
        for other in locations {
            self.facts.share_elements(self.types, first, other);
        }
    }
}
