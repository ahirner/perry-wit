use super::*;

type Properties = Vec<(String, TypeId, bool)>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Query {
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

pub enum View {
    Type(TypeId),
    Fields(Properties),
    Index(TypeId),
}

impl View {
    pub fn properties(self, ctx: &Context) -> Option<Properties> {
        match self {
            Self::Fields(properties) => Some(properties),
            Self::Type(ty) => match ty.hir(ctx) {
                HirType::Object(obj) if obj.index_signature.is_none() => {
                    let mut props: Properties = obj
                        .properties
                        .iter()
                        .map(|(name, p)| (name.clone(), TypeId::intern(ctx, &p.ty), p.optional))
                        .collect();
                    props.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                    Some(props)
                }
                _ => None,
            },
            Self::Index(_) => None,
        }
    }
}

pub struct Resolver<'a, L> {
    ctx: &'a Context,
    lookup: &'a L,
}

impl<'a, 'h, L: DeclarationLookup<'h>> Resolver<'a, L> {
    pub fn new(ctx: &'a Context, lookup: &'a L) -> Self {
        Self { ctx, lookup }
    }

    pub fn fields(
        &self,
        declaration: Declaration<'_>,
        bindings: &HashMap<&str, TypeId>,
        depth: usize,
    ) -> Option<Properties> {
        let (interface, class) = match declaration {
            Declaration::Interface(interface) => (Some(interface), None),
            Declaration::Class(class) => (None, Some(class)),
            _ => return None,
        };
        let mut properties: Properties = Vec::new();
        let mut merge = |field: (String, TypeId, bool)| {
            if let Some(existing) = properties.iter_mut().find(|p| p.0 == field.0) {
                *existing = field;
            } else {
                properties.push(field);
            }
        };
        let mut remaining = ARGUMENT_LIMIT;
        for parent in interface.into_iter().flat_map(|i| &i.extends) {
            let parent = substitute_id(self.ctx, parent, bindings, &mut remaining).ok()?;
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
            let ty = substitute_id(self.ctx, ty, bindings, &mut remaining).ok()?;
            merge((name.clone(), ty, optional));
        }
        Some(properties)
    }

    pub fn named(
        &self,
        base: &str,
        arguments: &[TypeId],
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
            let target = substitute_id(self.ctx, &alias.ty, &bindings, &mut remaining).ok()?;
            let union = matches!(target.hir(self.ctx), HirType::Union(_));
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

    fn resolve(&self, ty: TypeId, query: Query, depth: usize) -> Option<(View, usize)> {
        let hir = ty.hir(self.ctx);
        if let Some((base, arguments)) = generic_parts(hir) {
            let arg_ids = TypeId::intern_slice(self.ctx, arguments);
            return self
                .named(
                    base,
                    &arg_ids,
                    matches!(hir, HirType::Named(_)),
                    query,
                    depth,
                )
                .or_else(|| (query == Query::Union).then_some((View::Type(ty), depth)));
        }
        if query == Query::Property
            && let HirType::Union(members) = hir
        {
            let member_ids = TypeId::intern_slice(self.ctx, members);
            let flattened = self.flatten(&member_ids, depth);
            let mut defined = flattened
                .into_iter()
                .filter(|m| !matches!(m.hir(self.ctx), HirType::Null | HirType::Void));
            let single = defined.next()?;
            if defined.next().is_some() || matches!(single.hir(self.ctx), HirType::Union(_)) {
                return None;
            }
            return self.resolve(single, query, depth);
        }
        Some((View::Type(ty), depth))
    }

    pub fn record(&self, ty: TypeId, depth: usize) -> Option<Properties> {
        self.resolve(ty, Query::Record, depth)?
            .0
            .properties(self.ctx)
    }

    pub fn flatten(&self, members: &[TypeId], depth: usize) -> Vec<TypeId> {
        if depth >= RESOLUTION_DEPTH {
            return members.to_vec();
        }
        let mut out = Vec::new();
        self.members(members, Query::Union, depth, &mut out);
        out
    }

    fn members(
        &self,
        members: &[TypeId],
        query: Query,
        depth: usize,
        out: &mut Vec<TypeId>,
    ) -> Option<()> {
        for &member in members {
            let (View::Type(target), resolved_depth) = self.resolve(member, query, depth)? else {
                return None;
            };
            if resolved_depth < RESOLUTION_DEPTH
                && let HirType::Union(nested) = target.hir(self.ctx)
            {
                let nested_ids = TypeId::intern_slice(self.ctx, nested);
                self.members(&nested_ids, query, resolved_depth + 1, out)?;
            } else if query == Query::Keys {
                if !matches!(target.hir(self.ctx), HirType::StringLiteral(_)) {
                    return None;
                }
                out.push(target);
            } else if !out.contains(&target) {
                out.push(target);
            }
        }
        Some(())
    }

    fn matches(&self, member: TypeId, pattern: TypeId, depth: usize) -> bool {
        if depth >= RESOLUTION_DEPTH {
            return false;
        }
        if member == pattern
            || matches!(
                (member.hir(self.ctx), pattern.hir(self.ctx)),
                (HirType::StringLiteral(_), HirType::String)
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

    pub fn utility_type(&self, base: &str, arguments: &[TypeId], depth: usize) -> Option<TypeId> {
        if depth >= RESOLUTION_DEPTH
            || self.lookup.interface(base).is_some()
            || self.lookup.type_alias(base).is_some()
        {
            return None;
        }
        let members = match (base, arguments) {
            ("NonNullable", [target]) => self
                .flatten(std::slice::from_ref(target), depth + 1)
                .into_iter()
                .filter(|m| !matches!(m.hir(self.ctx), HirType::Null | HirType::Void))
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
        Some(union_of(self.ctx, members))
    }

    fn utility(
        &self,
        base: &str,
        arguments: &[TypeId],
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
                    *ty = strip_void(self.ctx, *ty);
                }
            }
            "Pick" | "Omit" => properties.retain(|(name, _, _)| {
                selected
                    .iter()
                    .any(|key| matches!(key.hir(self.ctx), HirType::StringLiteral(k) if k == name))
                    == (base == "Pick")
            }),
            _ => {}
        }
        Some((View::Fields(properties), depth))
    }

    pub fn property(
        &self,
        base: &str,
        arguments: &[TypeId],
        property: &str,
    ) -> Option<(TypeId, bool)> {
        match self.named(base, arguments, false, Query::Property, 0)?.0 {
            View::Fields(fields) => fields
                .into_iter()
                .find(|(name, _, _)| name == property)
                .map(|(_, ty, optional)| (ty, optional)),
            View::Index(value) => Some((value, true)),
            View::Type(ty) => match ty.hir(self.ctx) {
                HirType::Object(obj) => obj
                    .properties
                    .get(property)
                    .map(|p| (TypeId::intern(self.ctx, &p.ty), p.optional))
                    .or_else(|| {
                        obj.index_signature
                            .as_deref()
                            .map(|idx| (TypeId::intern(self.ctx, idx), true))
                    }),
                _ => None,
            },
        }
    }

    pub fn dictionary(&self, ty: TypeId) -> Option<(HirType, HirType)> {
        let value = match self.resolve(ty, Query::Dictionary, 0)?.0 {
            View::Index(value) => value.hir(self.ctx).clone(),
            View::Fields(fields) => self.uniform(
                fields
                    .iter()
                    .map(|(_, ty, optional)| (ty.hir(self.ctx), *optional)),
            )?,
            View::Type(ty) => match ty.hir(self.ctx) {
                HirType::Object(obj) => match &obj.index_signature {
                    Some(value) => (**value).clone(),
                    None => self.uniform(obj.properties.values().map(|p| (&p.ty, p.optional)))?,
                },
                _ => return None,
            },
        };
        Some((HirType::String, value))
    }

    fn uniform<'b>(
        &self,
        mut properties: impl Iterator<Item = (&'b HirType, bool)>,
    ) -> Option<HirType> {
        let (first, mut optional) = properties.next()?;
        let mut exact = true;
        for (ty, opt) in properties {
            optional |= opt;
            if ty != first {
                if !optional_union_members(ty).eq(optional_union_members(first)) {
                    return None;
                }
                exact = false;
            }
        }
        (exact || optional).then(|| optional_property(first.clone(), optional))
    }
}

fn optional_union_members(ty: &HirType) -> impl Iterator<Item = &HirType> {
    let (items, add_void) = match ty {
        HirType::Union(items) => (items.as_slice(), !items.contains(&HirType::Void)),
        other => (std::slice::from_ref(other), true),
    };
    items.iter().chain(add_void.then_some(&HirType::Void))
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
