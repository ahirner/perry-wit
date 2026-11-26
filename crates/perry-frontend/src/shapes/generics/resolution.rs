use super::*;

use crate::engine::types::{ObjectProp, PropSpan};

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

pub(crate) enum Expansion<'h> {
    Type(Ty),
    Class(crate::shapes::ClassShape<'h>, PropSpan),
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

    pub(crate) fn expand(
        &mut self,
        declaration: Declaration<'h>,
        bindings: &[(TypeName, Ty)],
        depth: usize,
    ) -> Result<Expansion<'h>> {
        let mut remaining = ARGUMENT_LIMIT;
        let interface = match declaration {
            Declaration::Alias(alias) => {
                return Ok(Expansion::Type(substitute(
                    self.ctx,
                    self.types,
                    &alias.ty,
                    bindings,
                    &mut remaining,
                )?));
            }
            Declaration::Class(class) => {
                let mut remaining = usize::MAX;
                let fields = class
                    .fields()
                    .map(|(field, solved)| {
                        let ty = match solved {
                            Some(ty) => ty,
                            None => substitute(
                                self.ctx,
                                self.types,
                                &field.ty,
                                bindings,
                                &mut remaining,
                            )?,
                        };
                        Ok(self.types.property(&field.name, ty, false))
                    })
                    .collect::<Result<Vec<_>>>()?;
                return Ok(Expansion::Class(
                    class,
                    self.types.intern_properties(&fields),
                ));
            }
            Declaration::Interface(interface) => interface,
        };
        let mut properties: Vec<ObjectProp> = Vec::new();
        let mut merge = |field: ObjectProp| {
            if let Some(existing) = properties.iter_mut().find(|p| p.name == field.name) {
                *existing = field;
            } else {
                properties.push(field);
            }
        };
        let unresolved = || {
            diag!(
                codes::UNSUPPORTED_TYPE,
                "failed to resolve inherited properties of interface {}",
                interface.name
            )
        };
        for parent in &interface.extends {
            let parent = substitute(self.ctx, self.types, parent, bindings, &mut remaining)
                .map_err(|_| unresolved())?;
            let fields = self.record(parent, depth).ok_or_else(unresolved)?;
            for &field in self.types.object_slice(fields) {
                merge(field);
            }
        }
        for field in &interface.properties {
            let ty = substitute(self.ctx, self.types, &field.ty, bindings, &mut remaining)
                .map_err(|_| unresolved())?;
            merge(self.types.property(&field.name, ty, field.optional));
        }
        Ok(Expansion::Type(self.types.object(&properties, None)))
    }

    pub(crate) fn named(
        &mut self,
        base: &str,
        arguments: &[Ty],
        plain: bool,
        query: Query,
        depth: usize,
    ) -> Option<(Ty, usize)> {
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
        if (query.aliases_only() && !matches!(declaration, Declaration::Alias(_)))
            || (matches!(declaration, Declaration::Class(_)) && query == Query::Dictionary)
        {
            return None;
        }
        let bindings = declaration
            .bind(self.ctx, base, self.types, arguments)
            .ok()?;
        match self.expand(declaration, &bindings, depth + 1).ok()? {
            Expansion::Type(target) => {
                let union = matches!(self.types.kind(target), TyKind::Union(_));
                let resolved = self.resolve(target, query, depth + 1)?;
                // Flatten aliases only when they expose a union or a utility expansion.
                if query == Query::Union && !union && resolved.0 == target {
                    return None;
                }
                Some(resolved)
            }
            Expansion::Class(_, props) => Some((self.types.object_type(props, None), depth)),
        }
    }

    fn resolve(&mut self, ty: Ty, query: Query, depth: usize) -> Option<(Ty, usize)> {
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
                .or_else(|| (query == Query::Union).then_some((ty, depth)));
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
        Some((ty, depth))
    }

    pub(crate) fn record(&mut self, ty: Ty, depth: usize) -> Option<PropSpan> {
        let (ty, _) = self.resolve(ty, Query::Record, depth)?;
        self.types.closed_record(ty)
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
            let (target, resolved_depth) = self.resolve(member, query, depth)?;
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
        let pattern = self.types.object_slice(pattern).to_vec();
        let member = self.types.object_slice(member).to_vec();
        !pattern.is_empty()
            && pattern.iter().all(|p| {
                member.iter().any(|m| {
                    p.name == m.name
                        && (!m.optional || p.optional)
                        && self.matches(m.ty, p.ty, depth + 1)
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
    ) -> Option<(Ty, usize)> {
        if depth >= RESOLUTION_DEPTH
            || self.decls.interfaces.contains_key(base)
            || self.decls.type_aliases.contains_key(base)
        {
            return None;
        }
        if matches!(base, "Exclude" | "Extract" | "NonNullable") {
            let target = self.utility_type(base, arguments, depth)?;
            return if query == Query::Dictionary {
                let resolved = self.resolve(target, Query::Record, depth + 1)?;
                self.types.closed_record(resolved.0)?;
                Some(resolved)
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
            return Some((self.types.object(&[], Some(value)), depth));
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
        let (inner, _) = self.resolve(inner, Query::Record, depth + 1)?;
        let fields = self.types.closed_record(inner)?;
        if base == "Readonly" {
            return Some((inner, depth));
        }
        let mut properties = self.types.object_slice(fields).to_vec();
        match base {
            "Partial" => {
                for field in &mut properties {
                    field.optional = true;
                }
            }
            "Required" => {
                for field in &mut properties {
                    field.optional = false;
                    if field.ty != Ty::VOID {
                        field.ty = self
                            .types
                            .exclude_absences(field.ty, crate::Absence::Undefined);
                    }
                }
            }
            "Pick" | "Omit" => properties.retain(|field| {
                selected
                    .iter()
                    .any(|key| self.literal(*key) == Some(self.types.name_str(field.name)))
                    == (base == "Pick")
            }),
            _ => {}
        }
        Some((self.types.object(&properties, None), depth))
    }

    pub(crate) fn property(
        &mut self,
        base: &str,
        arguments: &[Ty],
        property: &str,
    ) -> Option<(Ty, bool)> {
        let (ty, _) = self.named(base, arguments, false, Query::Property, 0)?;
        let TyKind::Object { props, index } = self.types.kind(ty) else {
            return None;
        };
        self.types
            .object_slice(props)
            .iter()
            .find(|p| self.types.name_str(p.name) == property)
            .map(|p| (p.ty, p.optional))
            .or_else(|| index.map(|ty| (ty, true)))
    }

    pub(crate) fn dictionary(&mut self, ty: Ty) -> Option<(Ty, Ty)> {
        let (ty, _) = self.resolve(ty, Query::Dictionary, 0)?;
        let TyKind::Object { props, index } = self.types.kind(ty) else {
            return None;
        };
        if let Some(value) = index {
            return Some((Ty::STRING, value));
        }
        let fields = self.types.object_slice(props).to_vec();
        let first = fields.first()?.ty;
        let mut optional = false;
        let defined = self
            .types
            .exclude_absences(first, crate::Absence::Undefined);
        let mut exact = true;
        for field in fields {
            let ty = field.ty;
            optional |= field.optional;
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
