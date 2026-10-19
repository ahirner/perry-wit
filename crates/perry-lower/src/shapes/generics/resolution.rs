use super::*;

type Properties = Vec<(String, ArgumentId, bool)>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Query {
    Union,
    Keys,
    Record,
    Property,
    Dictionary,
}

impl Query {
    fn aliases_only(self) -> bool {
        matches!(self, Self::Union | Self::Keys)
    }
}

pub(super) enum View {
    Argument(ArgumentId),
    Fields(Properties),
    Index(ArgumentId),
}

impl View {
    pub(super) fn properties(self, ctx: &Context) -> Option<Properties> {
        match self {
            Self::Fields(properties) => Some(properties),
            Self::Argument(id) => match id.deref(ctx).kind() {
                Kind::Object {
                    properties,
                    index_signature: None,
                    ..
                } => Some(
                    properties
                        .iter()
                        .map(|p| (p.name.clone(), p.ty, p.optional))
                        .collect(),
                ),
                _ => None,
            },
            Self::Index(_) => None,
        }
    }
}

pub(in crate::shapes) struct Resolver<'a, L> {
    ctx: &'a Context,
    lookup: &'a L,
}

impl<'a, L: DeclarationLookup> Resolver<'a, L> {
    pub(in crate::shapes) fn new(ctx: &'a Context, lookup: &'a L) -> Self {
        Self { ctx, lookup }
    }

    pub(super) fn fields(
        &self,
        declaration: Declaration<'_>,
        bindings: &HashMap<&str, ArgumentId>,
        depth: usize,
    ) -> Option<Properties> {
        let (interface, class) = match declaration {
            Declaration::Interface(interface) => (Some(interface), None),
            Declaration::Class(class) => (None, Some(class)),
            _ => return None,
        };
        let mut properties: Properties = Vec::new();
        let mut merge = |field: (String, ArgumentId, bool)| {
            if let Some(existing) = properties.iter_mut().find(|p| p.0 == field.0) {
                *existing = field;
            } else {
                properties.push(field);
            }
        };
        let mut remaining = ARGUMENT_LIMIT;
        for parent in interface.into_iter().flat_map(|i| &i.extends) {
            let parent = Argument::from_hir(self.ctx, parent, bindings, &mut remaining).ok()?;
            for field in self.record(parent, depth)? {
                merge(field);
            }
        }
        let fields = interface
            .into_iter()
            .flat_map(|i| i.properties.iter().map(|p| (&p.name, &p.ty, p.optional)))
            .chain(
                class
                    .into_iter()
                    .flat_map(|c| c.fields.iter().map(|f| (&f.name, &f.ty, false))),
            );
        for (name, ty, optional) in fields {
            let ty = Argument::from_hir(self.ctx, ty, bindings, &mut remaining).ok()?;
            merge((name.clone(), ty, optional));
        }
        Some(properties)
    }

    pub(super) fn named(
        &self,
        base: &str,
        arguments: &[ArgumentId],
        plain: bool,
        query: Query,
        depth: usize,
    ) -> Option<(View, usize)> {
        if depth >= RESOLUTION_DEPTH {
            return None;
        }
        let declaration = if plain && query.aliases_only() {
            let (_, alias) = self.lookup.type_alias(base)?;
            if !alias.type_params.is_empty() {
                return None;
            }
            Declaration::Alias(alias)
        } else {
            if let Some(expanded) = self.utility(base, arguments, query, depth) {
                return Some(expanded);
            }
            Declaration::find(self.lookup, base)?.1
        };
        let bindings = declaration.bind(self.ctx, base, arguments).ok()?;
        if let Declaration::Alias(alias) = declaration {
            let mut remaining = ARGUMENT_LIMIT;
            let target = Argument::from_hir(self.ctx, &alias.ty, &bindings, &mut remaining).ok()?;
            let union = matches!(target.deref(self.ctx).kind(), Kind::Union(_));
            let resolved = self.resolve(target, query, depth + 1)?;
            // Flatten aliases only when they expose a union or a utility expansion.
            if query == Query::Union
                && !union
                && matches!(resolved.0, View::Argument(id) if id == target)
            {
                return None;
            }
            Some(resolved)
        } else if !query.aliases_only() {
            Some((
                View::Fields(self.fields(declaration, &bindings, depth + 1)?),
                depth,
            ))
        } else {
            None
        }
    }

    fn resolve(&self, ty: ArgumentId, query: Query, depth: usize) -> Option<(View, usize)> {
        let argument = ty.deref(self.ctx);
        if let Some((base, arguments)) = generic_parts(argument.kind()) {
            return self
                .named(
                    base,
                    arguments,
                    matches!(argument.kind(), Kind::Named(_)),
                    query,
                    depth,
                )
                .or_else(|| (query == Query::Union).then_some((View::Argument(ty), depth)));
        }
        if query == Query::Property
            && let Kind::Union(members) = argument.kind()
        {
            let flattened = self.flatten(members, depth);
            let mut defined = flattened
                .into_iter()
                .filter(|m| !matches!(m.deref(self.ctx).kind(), Kind::Null | Kind::Void));
            let single = defined.next()?;
            if defined.next().is_some() || matches!(single.deref(self.ctx).kind(), Kind::Union(_)) {
                return None;
            }
            return self.resolve(single, query, depth);
        }
        Some((View::Argument(ty), depth))
    }

    pub(in crate::shapes) fn record(&self, ty: ArgumentId, depth: usize) -> Option<Properties> {
        self.resolve(ty, Query::Record, depth)?
            .0
            .properties(self.ctx)
    }

    pub(in crate::shapes) fn flatten(
        &self,
        members: &[ArgumentId],
        depth: usize,
    ) -> Vec<ArgumentId> {
        if depth >= RESOLUTION_DEPTH {
            return members.to_vec();
        }
        let mut out = Vec::new();
        self.members(members, Query::Union, depth, &mut out);
        out
    }

    fn members(
        &self,
        members: &[ArgumentId],
        query: Query,
        depth: usize,
        out: &mut Vec<ArgumentId>,
    ) -> Option<()> {
        for &member in members {
            let (View::Argument(target), resolved_depth) = self.resolve(member, query, depth)?
            else {
                return None;
            };
            if resolved_depth < RESOLUTION_DEPTH
                && let Kind::Union(nested) = target.deref(self.ctx).kind()
            {
                self.members(nested, query, resolved_depth + 1, out)?;
            } else if query == Query::Keys {
                if !matches!(target.deref(self.ctx).kind(), Kind::StringLiteral(_)) {
                    return None;
                }
                out.push(target);
            } else if !out.contains(&target) {
                out.push(target);
            }
        }
        Some(())
    }

    fn matches(&self, member: ArgumentId, pattern: ArgumentId, depth: usize) -> bool {
        if depth >= RESOLUTION_DEPTH {
            return false;
        }
        if member == pattern
            || matches!(
                (
                    member.deref(self.ctx).kind(),
                    pattern.deref(self.ctx).kind()
                ),
                (Kind::StringLiteral(_), Kind::String)
            )
        {
            return true;
        }
        let (Some(member), Some(pattern)) = (
            self.record(member, depth + 1),
            self.record(pattern, depth + 1),
        ) else {
            return false;
        };
        !pattern.is_empty()
            && pattern.iter().all(|(name, ty, optional)| {
                member.iter().any(|(other_name, other_ty, other_optional)| {
                    name == other_name
                        && (!other_optional || *optional)
                        && self.matches(*other_ty, *ty, depth + 1)
                })
            })
    }

    pub(super) fn utility_type(
        &self,
        base: &str,
        arguments: &[ArgumentId],
        depth: usize,
    ) -> Option<ArgumentId> {
        if depth >= RESOLUTION_DEPTH
            || self.lookup.interface(base).is_some()
            || self.lookup.type_alias(base).is_some()
        {
            return None;
        }
        let members = match (base, arguments) {
            ("NonNullable", [target]) => self
                .flatten(&[*target], depth + 1)
                .into_iter()
                .filter(|m| !matches!(m.deref(self.ctx).kind(), Kind::Null | Kind::Void))
                .collect(),
            ("Exclude" | "Extract", [target, selected]) => {
                let selected = self.flatten(&[*selected], depth + 1);
                self.flatten(&[*target], depth + 1)
                    .into_iter()
                    .filter(|member| {
                        selected
                            .iter()
                            .any(|candidate| self.matches(*member, *candidate, depth + 1))
                            == (base == "Extract")
                    })
                    .collect()
            }
            _ => return None,
        };
        Some(union_of(self.ctx, members))
    }

    fn utility(
        &self,
        base: &str,
        arguments: &[ArgumentId],
        query: Query,
        depth: usize,
    ) -> Option<(View, usize)> {
        if depth >= RESOLUTION_DEPTH
            || self.lookup.interface(base).is_some()
            || self.lookup.type_alias(base).is_some()
        {
            return None;
        }
        if matches!(base, "Exclude" | "Extract" | "NonNullable") {
            let target = self.utility_type(base, arguments, depth)?;
            return if query == Query::Dictionary {
                Some((View::Fields(self.record(target, depth + 1)?), depth))
            } else {
                self.resolve(target, query, depth + 1)
            };
        }
        if query.aliases_only() {
            return None;
        }
        if base == "Record"
            && let [_, value] = arguments
            && matches!(query, Query::Property | Query::Dictionary)
        {
            return Some((View::Index(*value), depth));
        }
        let (inner, keys) = match (base, arguments) {
            ("Readonly" | "Partial" | "Required", [inner]) => (*inner, None),
            ("Pick" | "Omit", [inner, keys]) => (*inner, Some(*keys)),
            _ => return None,
        };
        let mut selected = Vec::new();
        if let Some(keys) = keys {
            self.members(&[keys], Query::Keys, depth + 1, &mut selected)?;
        }
        let mut properties = self.record(inner, depth + 1)?;
        match base {
            "Partial" => {
                for (_, _, optional) in &mut properties {
                    *optional = true;
                }
            }
            "Required" => {
                for (_, ty, optional) in &mut properties {
                    *optional = false;
                    *ty = strip_void_arg(self.ctx, *ty);
                }
            }
            "Pick" | "Omit" => properties.retain(|(name, _, _)| {
                selected.iter().any(|key| {
                    matches!(key.deref(self.ctx).kind(), Kind::StringLiteral(key) if key == name)
                }) == (base == "Pick")
            }),
            _ => {}
        }
        Some((View::Fields(properties), depth))
    }

    pub(super) fn property(
        &self,
        base: &str,
        arguments: &[ArgumentId],
        property: &str,
    ) -> Option<(ArgumentId, bool)> {
        match self.named(base, arguments, false, Query::Property, 0)?.0 {
            View::Fields(fields) => fields
                .into_iter()
                .find(|(name, _, _)| name == property)
                .map(|(_, ty, optional)| (ty, optional)),
            View::Index(value) => Some((value, true)),
            View::Argument(ty) => {
                let argument = ty.deref(self.ctx);
                let Kind::Object {
                    index_signature, ..
                } = argument.kind()
                else {
                    return None;
                };
                argument
                    .property(property)
                    .map(|p| (p.ty, p.optional))
                    .or_else(|| index_signature.map(|ty| (ty, true)))
            }
        }
    }

    pub(super) fn dictionary(&self, ty: ArgumentId) -> Option<(ArgumentId, ArgumentId)> {
        let value = match self.resolve(ty, Query::Dictionary, 0)?.0 {
            View::Index(value) => value,
            View::Fields(fields) => {
                self.uniform(fields.iter().map(|(_, ty, optional)| (*ty, *optional)))?
            }
            View::Argument(ty) => match ty.deref(self.ctx).kind() {
                Kind::Object {
                    index_signature: Some(value),
                    ..
                } => *value,
                Kind::Object { properties, .. } => {
                    self.uniform(properties.iter().map(|p| (p.ty, p.optional)))?
                }
                _ => return None,
            },
        };
        Some((Argument::from_kind(self.ctx, Kind::String), value))
    }

    fn uniform(
        &self,
        mut properties: impl Iterator<Item = (ArgumentId, bool)> + Clone,
    ) -> Option<ArgumentId> {
        let optional = properties.clone().any(|(_, optional)| optional);
        let (first, _) = properties.next()?;
        let first = optional_property_arg(self.ctx, first, optional);
        properties
            .all(|(ty, _)| optional_property_arg(self.ctx, ty, optional) == first)
            .then_some(first)
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
