//! Instantiation of generic interfaces, type aliases, and classes.
use super::*;
use perry_hir::ir::{Class, Interface, TypeAlias};

mod resolution;
pub use resolution::Query;
pub use resolution::Resolver;

const RESOLUTION_DEPTH: usize = 32;
pub const ARGUMENT_LIMIT: usize = 4096;

pub trait DeclarationLookup<'h> {
    fn interface(&self, name: &str) -> Option<(usize, &'h Interface)>;
    fn type_alias(&self, name: &str) -> Option<(usize, &'h TypeAlias)>;
    fn class(&self, _name: &str) -> Option<(usize, &'h Class)> {
        None
    }
}

impl<'h> DeclarationLookup<'h> for Declarations<'h> {
    fn interface(&self, name: &str) -> Option<(usize, &'h Interface)> {
        self.interfaces.get(name).copied()
    }
    fn type_alias(&self, name: &str) -> Option<(usize, &'h TypeAlias)> {
        self.type_aliases.get(name).copied()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Utility {
    Partial,
    Required,
    Pick,
    Omit,
}

impl Utility {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Partial" => Some(Self::Partial),
            "Required" => Some(Self::Required),
            "Pick" => Some(Self::Pick),
            "Omit" => Some(Self::Omit),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeclarationId {
    Interface(usize),
    Alias(usize),
    Class(usize),
    Utility(Utility),
}

pub type InstanceKey = (DeclarationId, Vec<TypeId>);

/// The generic declaration an instantiation expands.
#[derive(Clone, Copy)]
pub enum Declaration<'a> {
    Interface(&'a Interface),
    Alias(&'a TypeAlias),
    Class(&'a Class),
}

fn charge_budget(ty: &HirType, remaining: &mut usize) -> Result<()> {
    let mut exceeded = false;
    let _ = ty.any(&mut |_| {
        if *remaining <= 1 {
            exceeded = true;
            true
        } else {
            *remaining -= 1;
            false
        }
    });
    ensure!(!exceeded, "generic type arguments expand indefinitely");
    Ok(())
}

fn check_subst(
    ctx: &Context,
    ty: &HirType,
    bindings: &HashMap<&str, TypeId>,
    remaining: &mut usize,
) -> Result<bool> {
    let mut subst = false;
    let mut err = None;
    let _ = ty.any(&mut |node| {
        if *remaining <= 1 {
            err = Some(anyhow!("generic type arguments expand indefinitely"));
            return true;
        }
        *remaining -= 1;
        if let HirType::Named(name) | HirType::TypeVar(name) = node {
            match bindings.get(name.as_str()) {
                Some(&bound) => {
                    if let Err(e) = charge_budget(bound.hir(ctx), remaining) {
                        err = Some(e);
                        return true;
                    }
                    subst = true;
                }
                None if matches!(node, HirType::TypeVar(_)) => {
                    err = Some(anyhow!("unresolved type argument {name}"));
                    return true;
                }
                None => {}
            }
        }
        false
    });
    match err {
        Some(e) => Err(e),
        None => Ok(subst),
    }
}

fn validate_id(ctx: &Context, id: TypeId, remaining: &mut usize) -> Result<TypeId> {
    check_subst(ctx, id.hir(ctx), &HashMap::new(), remaining)?;
    Ok(id)
}

pub fn substitute_id(
    ctx: &Context,
    ty: &HirType,
    bindings: &HashMap<&str, TypeId>,
    remaining: &mut usize,
) -> Result<TypeId> {
    let needs_subst = check_subst(ctx, ty, bindings, remaining)?;
    if !needs_subst {
        return Ok(TypeId::intern(ctx, ty));
    }
    if let HirType::Named(name) | HirType::TypeVar(name) = ty
        && let Some(&bound) = bindings.get(name.as_str())
    {
        return Ok(bound);
    }
    let mut out = ty.clone();
    out.transform(&mut |node| {
        if let HirType::Named(name) | HirType::TypeVar(name) = node
            && let Some(&bound) = bindings.get(name.as_str())
        {
            *node = bound.hir(ctx).clone();
        }
    });
    Ok(TypeId::from_owned(ctx, out))
}

impl<'a> Declaration<'a> {
    pub fn find<L: DeclarationLookup<'a>>(lookup: &L, base: &str) -> Option<(DeclarationId, Self)> {
        if let Some((index, interface)) = lookup.interface(base) {
            return Some((DeclarationId::Interface(index), Self::Interface(interface)));
        }
        if let Some((index, class)) = lookup.class(base) {
            return Some((DeclarationId::Class(index), Self::Class(class)));
        }
        let (index, alias) = lookup.type_alias(base)?;
        Some((DeclarationId::Alias(index), Self::Alias(alias)))
    }

    pub fn parameters(self) -> &'a [perry_hir::types::TypeParam] {
        match self {
            Self::Interface(interface) => &interface.type_params,
            Self::Alias(alias) => &alias.type_params,
            Self::Class(class) => &class.type_params,
        }
    }

    pub fn bind(
        self,
        ctx: &Context,
        base: &str,
        arguments: &[TypeId],
    ) -> Result<HashMap<&'a str, TypeId>> {
        let parameters = self.parameters();
        ensure!(
            arguments.len() <= parameters.len(),
            "too many type arguments for {base}"
        );
        let mut bindings = HashMap::with_capacity(parameters.len());
        let mut remaining = ARGUMENT_LIMIT;
        for (index, parameter) in parameters.iter().enumerate() {
            let argument = match arguments.get(index) {
                Some(&argument) => validate_id(ctx, argument, &mut remaining)?,
                None => substitute_id(
                    ctx,
                    parameter
                        .default
                        .as_deref()
                        .or(parameter.constraint.as_deref())
                        .with_context(|| {
                            format!("missing type argument {} of {base}", parameter.name)
                        })?,
                    &bindings,
                    &mut remaining,
                )?,
            };
            bindings.insert(parameter.name.as_str(), argument);
        }
        Ok(bindings)
    }
}

fn generic_parts(ty: &HirType) -> Option<(&str, &[HirType])> {
    match ty {
        HirType::Named(name) => Some((name, &[])),
        HirType::Generic { base, type_args } => Some((base, type_args)),
        _ => None,
    }
}

fn union_of(ctx: &Context, members: Vec<TypeId>) -> TypeId {
    match members.len() {
        0 => TypeId::intern(ctx, &HirType::Never),
        1 => members[0],
        _ => TypeId::from_owned(
            ctx,
            HirType::Union(members.into_iter().map(|m| m.hir(ctx).clone()).collect()),
        ),
    }
}

fn strip_void(ctx: &Context, ty: TypeId) -> TypeId {
    let HirType::Union(items) = ty.hir(ctx) else {
        return ty;
    };
    if !items.contains(&HirType::Void) {
        return ty;
    }
    let filtered: Vec<TypeId> = items
        .iter()
        .filter(|item| !matches!(item, HirType::Void))
        .map(|item| TypeId::intern(ctx, item))
        .collect();
    if filtered.is_empty() {
        TypeId::intern(ctx, &HirType::Void)
    } else {
        union_of(ctx, filtered)
    }
}

pub fn optional_property(ty: HirType, optional: bool) -> HirType {
    if !optional {
        return ty;
    }
    match ty {
        HirType::Union(mut items) => {
            if !items.contains(&HirType::Void) {
                items.push(HirType::Void);
            }
            HirType::Union(items)
        }
        other => HirType::Union(vec![other, HirType::Void]),
    }
}

/// The element type yielded by iteration. Unsupported types are returned unchanged.
pub fn iterated_element(ty: HirType) -> Result<HirType, HirType> {
    let HirType::Generic { base, type_args } = ty else {
        return match ty {
            HirType::String => Ok(HirType::String),
            HirType::Array(element) => Ok(*element),
            other => Err(other),
        };
    };
    let picked = match base.as_str() {
        "Set" | "ReadonlySet" | "SetValues" => <[HirType; 1]>::try_from(type_args).map(|[e]| e),
        "MapKeys" => <[HirType; 2]>::try_from(type_args).map(|[key, _]| key),
        "MapValues" => <[HirType; 2]>::try_from(type_args).map(|[_, value]| value),
        "Map" | "ReadonlyMap" | "MapEntries" => {
            <[HirType; 2]>::try_from(type_args).map(|pair| HirType::Tuple(pair.into()))
        }
        "SetEntries" => <[HirType; 1]>::try_from(type_args)
            .map(|[element]| HirType::Tuple(vec![element.clone(), element])),
        _ => Err(type_args),
    };
    picked.map_err(|type_args| HirType::Generic { base, type_args })
}

impl Declarations<'_> {
    /// HIR compatibility result for a selected member.
    pub fn property_type(
        &self,
        ctx: &Context,
        base: &str,
        arguments: &[HirType],
        property: &str,
    ) -> Option<HirType> {
        if arguments
            .iter()
            .any(|ty| ty.any(&mut |n| matches!(n, HirType::TypeVar(_))))
        {
            return None;
        }
        let arg_ids = TypeId::intern_slice(ctx, arguments);
        let (ty, optional) = Resolver::new(ctx, self).property(base, &arg_ids, property)?;
        Some(optional_property(ty.hir(ctx).clone(), optional))
    }

    /// Extract `(key_ty, value_ty)` if `ty` resolves to `Record<K, V>`, `{ [key: string]: V }`, or a
    /// closed record/interface whose properties share a uniform value type.
    pub fn dict_entry_types(&self, ctx: &Context, ty: &HirType) -> Option<(HirType, HirType)> {
        if ty.any(&mut |n| matches!(n, HirType::TypeVar(_))) {
            return None;
        }
        let id = TypeId::intern(ctx, ty);
        Resolver::new(ctx, self).dictionary(id)
    }

    pub fn object_properties(
        &self,
        ctx: &Context,
        ty: &HirType,
    ) -> Option<Vec<(String, HirType, bool)>> {
        if ty.any(&mut |n| matches!(n, HirType::TypeVar(_))) {
            return None;
        }
        let id = TypeId::intern(ctx, ty);
        Some(
            Resolver::new(ctx, self)
                .record(id, 0)?
                .into_iter()
                .map(|(name, ty, optional)| (name, ty.hir(ctx).clone(), optional))
                .collect(),
        )
    }
}
