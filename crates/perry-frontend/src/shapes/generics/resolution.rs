use super::*;

pub(crate) type Properties = Vec<(String, Ty, bool)>;

pub(crate) fn object_properties(
    types: &TypeTable,
    props: crate::engine::types::PropSpan,
) -> Properties {
    let mut fields: Properties = types
        .object_slice(props)
        .iter()
        .map(|p| (types.name_str(p.name).to_owned(), p.ty, p.optional))
        .collect();
    fields.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    fields
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Query {
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

pub(crate) enum View {
    Type(Ty),
    Fields(Properties),
    Index(Ty),
}

impl View {
    pub(crate) fn properties(self, types: &TypeTable) -> Option<Properties> {
        match self {
            Self::Fields(properties) => Some(properties),
            Self::Type(ty) if let TyKind::Object { props, index: None } = types.kind(ty) => {
                Some(object_properties(types, props))
            }
            _ => None,
        }
    }
}

pub(crate) struct Resolver<'a, 'h> {
    ctx: &'a Context,
    decls: &'a Declarations<'h>,
    pub(crate) types: &'a mut TypeTable,
}

impl<'a, 'h> Resolver<'a, 'h> {
    pub(crate) fn new(
        ctx: &'a Context,
        decls: &'a Declarations<'h>,
        types: &'a mut TypeTable,
    ) -> Self {
        Self { ctx, decls, types }
    }

    pub(crate) fn fields(
        &mut self,
        declaration: Declaration<'_>,
        bindings: &[(TypeName, Ty)],
        depth: usize,
    ) -> Option<Properties> {
        let (interface, class) = match declaration {
            Declaration::Interface(interface) => (Some(interface), None),
            Declaration::Class(class) => (None, Some(class)),
            _ => return None,
        };
        let mut properties: Properties = Vec::new();
        let mut merge = |field: (String, Ty, bool)| {
            if let Some(existing) = properties.iter_mut().find(|p| p.0 == field.0) {
                *existing = field;
            } else {
                properties.push(field);
            }
        };
        let mut remaining = ARGUMENT_LIMIT;
        for parent in interface.into_iter().flat_map(|i| &i.extends) {
            let parent = substitute(self.ctx, self.types, parent, bindings, &mut remaining).ok()?;
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
            let ty = substitute(self.ctx, self.types, ty, bindings, &mut remaining).ok()?;
            merge((name.clone(), ty, optional));
        }
        Some(properties)
    }

    pub(crate) fn named(
        &mut self,
        base: &str,
        arguments: &[Ty],
        plain: bool,
        query: Query,
        depth: usize,
    ) -> Option<(View, usize)> {
        if depth >= RESOLUTION_DEPTH {
            return None;
        }
        let declaration = if plain && query.aliases_only() {
            let &(_, alias) = self.decls.type_aliases.get(base)?;
            if !alias.type_params.is_empty() {
                return None;
            }
            Declaration::Alias(alias)
        } else {
            if let Some(expanded) = self.utility(base, arguments, query, depth) {
                return Some(expanded);
            }
            Declaration::find(self.decls, base)?.1
        };
        if matches!(declaration, Declaration::Class(_)) && query == Query::Dictionary {
            return None;
        }
        let bindings = declaration
            .bind(self.ctx, base, self.types, arguments)
            .ok()?;
        if let Declaration::Alias(alias) = declaration {
            let mut remaining = ARGUMENT_LIMIT;
            let target =
                substitute(self.ctx, self.types, &alias.ty, &bindings, &mut remaining).ok()?;
            let union = matches!(self.types.kind(target), TyKind::Union(_));
            let resolved = self.resolve(target, query, depth + 1)?;
            // Flatten aliases only when they expose a union or a utility expansion.
            if query == Query::Union
                && !union
                && matches!(resolved.0, View::Type(ty) if ty == target)
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

    fn resolve(&mut self, ty: Ty, query: Query, depth: usize) -> Option<(View, usize)> {
        let kind = self.types.kind(ty);
        let named = match kind {
            TyKind::Generic { base, args } => {
                Some((base, self.types.ty_slice(args).to_vec(), false))
            }
            TyKind::Named(id) => Some((id, Vec::new(), true)),
            _ => None,
        };
        if let Some((id, args, plain)) = named {
            return self
                .named(id.get(self.ctx), &args, plain, query, depth)
                .or_else(|| (query == Query::Union).then_some((View::Type(ty), depth)));
        }
        if query == Query::Property
            && let TyKind::Union(span) = kind
        {
            let members = self.types.ty_slice(span).to_vec();
            let flattened = self.flatten(&members, depth);
            let mut defined = flattened
                .into_iter()
                .filter(|ty| !matches!(*ty, Ty::NULL | Ty::VOID));
            let one = defined.next()?;
            if defined.next().is_some() || matches!(self.types.kind(one), TyKind::Union(_)) {
                return None;
            }
            return self.resolve(one, query, depth);
        }
        Some((View::Type(ty), depth))
    }

    pub(crate) fn record(&mut self, ty: Ty, depth: usize) -> Option<Properties> {
        self.resolve(ty, Query::Record, depth)?
            .0
            .properties(self.types)
    }

    pub(crate) fn flatten(&mut self, members: &[Ty], depth: usize) -> Vec<Ty> {
        if depth >= RESOLUTION_DEPTH {
            return members.to_vec();
        }
        let mut out = Vec::new();
        self.members(members, Query::Union, depth, &mut out);
        out
    }

    fn members(
        &mut self,
        members: &[Ty],
        query: Query,
        depth: usize,
        out: &mut Vec<Ty>,
    ) -> Option<()> {
        for &member in members {
            let (View::Type(target), resolved_depth) = self.resolve(member, query, depth)? else {
                return None;
            };
            if resolved_depth < RESOLUTION_DEPTH
                && let TyKind::Union(nested) = self.types.kind(target)
            {
                let nested_ids = self.types.ty_slice(nested).to_vec();
                self.members(&nested_ids, query, resolved_depth + 1, out)?;
            } else if query == Query::Keys {
                self.literal(target)?;
                out.push(target);
            } else {
                out.push_unique(target);
            }
        }
        Some(())
    }

    fn literal(&self, ty: Ty) -> Option<&str> {
        if let TyKind::Literal(id) = self.types.kind(ty) {
            Some(id.get(self.ctx))
        } else {
            None
        }
    }

    fn matches(&mut self, member: Ty, pattern: Ty, depth: usize) -> bool {
        if depth >= RESOLUTION_DEPTH {
            return false;
        }
        if member == pattern || (self.literal(member).is_some() && pattern == Ty::STRING) {
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

    pub(crate) fn utility_type(
        &mut self,
        base: &str,
        arguments: &[Ty],
        depth: usize,
    ) -> Option<Ty> {
        if depth >= RESOLUTION_DEPTH
            || self.decls.interfaces.contains_key(base)
            || self.decls.type_aliases.contains_key(base)
        {
            return None;
        }
        let members: Vec<Ty> = match (base, arguments) {
            ("NonNullable", [target]) => self
                .flatten(std::slice::from_ref(target), depth + 1)
                .into_iter()
                .filter(|m| !matches!(*m, Ty::NULL | Ty::VOID))
                .collect(),
            ("Exclude" | "Extract", [target, selected]) => {
                let selected = self.flatten(std::slice::from_ref(selected), depth + 1);
                self.flatten(std::slice::from_ref(target), depth + 1)
                    .into_iter()
                    .filter(|&member| {
                        selected
                            .iter()
                            .any(|&candidate| self.matches(member, candidate, depth + 1))
                            == (base == "Extract")
                    })
                    .collect()
            }
            _ => return None,
        };
        Some(if members.is_empty() {
            Ty::NEVER
        } else {
            self.types.union(&members)
        })
    }

    fn utility(
        &mut self,
        base: &str,
        arguments: &[Ty],
        query: Query,
        depth: usize,
    ) -> Option<(View, usize)> {
        if depth >= RESOLUTION_DEPTH
            || self.decls.interfaces.contains_key(base)
            || self.decls.type_aliases.contains_key(base)
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
            && let &[_, value] = arguments
            && matches!(query, Query::Property | Query::Dictionary)
        {
            return Some((View::Index(value), depth));
        }
        let (&inner, keys) = match (base, arguments) {
            ("Readonly" | "Partial" | "Required", [inner]) => (inner, None),
            ("Pick" | "Omit", [inner, keys]) => (inner, Some(keys)),
            _ => return None,
        };
        let mut selected = Vec::new();
        if let Some(keys) = keys {
            self.members(
                std::slice::from_ref(keys),
                Query::Keys,
                depth + 1,
                &mut selected,
            )?;
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
                    if *ty != Ty::VOID {
                        *ty = self.types.exclude_absences(*ty, crate::Absence::Undefined);
                    }
                }
            }
            "Pick" | "Omit" => properties.retain(|(name, _, _)| {
                selected
                    .iter()
                    .any(|key| self.literal(*key) == Some(name.as_str()))
                    == (base == "Pick")
            }),
            _ => {}
        }
        Some((View::Fields(properties), depth))
    }

    pub(crate) fn property(
        &mut self,
        base: &str,
        arguments: &[Ty],
        property: &str,
    ) -> Option<(Ty, bool)> {
        match self.named(base, arguments, false, Query::Property, 0)?.0 {
            View::Fields(fields) => fields
                .into_iter()
                .find(|(name, _, _)| name == property)
                .map(|(_, ty, optional)| (ty, optional)),
            View::Index(value) => Some((value, true)),
            View::Type(ty) => match self.types.kind(ty) {
                TyKind::Object { props, index } => self
                    .types
                    .object_slice(props)
                    .iter()
                    .find(|p| self.types.name_str(p.name) == property)
                    .map(|p| (p.ty, p.optional))
                    .or_else(|| index.map(|ty| (ty, true))),
                _ => None,
            },
        }
    }

    pub(crate) fn dictionary(&mut self, ty: Ty) -> Option<(Ty, Ty)> {
        let view = self.resolve(ty, Query::Dictionary, 0)?.0;
        let fields = match view {
            View::Index(value) => return Some((Ty::STRING, value)),
            View::Type(ty)
                if let TyKind::Object {
                    index: Some(value), ..
                } = self.types.kind(ty) =>
            {
                return Some((Ty::STRING, value));
            }
            other => other.properties(self.types)?,
        };
        let (_, first, mut optional) = *fields.first()?;
        let defined = self
            .types
            .exclude_absences(first, crate::Absence::Undefined);
        let mut exact = true;
        for (_, ty, opt) in fields.into_iter().skip(1) {
            optional |= opt;
            if first != ty {
                if self.types.exclude_absences(ty, crate::Absence::Undefined) != defined {
                    return None;
                }
                exact = false;
            }
        }
        (exact || optional).then(|| {
            (
                Ty::STRING,
                if optional {
                    self.types.with_absences(first, crate::Absence::Undefined)
                } else {
                    first
                },
            )
        })
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
