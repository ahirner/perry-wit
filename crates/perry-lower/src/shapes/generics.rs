//! Instantiation of generic interfaces, type aliases, and classes.
use super::*;
use perry_hir::ir::{Class, Interface, TypeAlias};

mod resolution;
use resolution::Query;
pub(super) use resolution::Resolver;

const RESOLUTION_DEPTH: usize = 32;
const ARGUMENT_LIMIT: usize = 4096;

pub(super) trait DeclarationLookup<'h> {
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

impl<'h> DeclarationLookup<'h> for Shapes<'h> {
    fn interface(&self, name: &str) -> Option<(usize, &'h Interface)> {
        self.decls.interface(name)
    }
    fn type_alias(&self, name: &str) -> Option<(usize, &'h TypeAlias)> {
        self.decls.type_alias(name)
    }
    fn class(&self, name: &str) -> Option<(usize, &'h Class)> {
        self.classes.get(name).copied()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Utility {
    Partial,
    Required,
    Pick,
    Omit,
}

impl Utility {
    fn from_name(name: &str) -> Option<Self> {
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
pub(super) enum DeclarationId {
    Interface(usize),
    Alias(usize),
    Class(usize),
    Utility(Utility),
}

/// The generic declaration an instantiation expands.
#[derive(Clone, Copy)]
enum Declaration<'a> {
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
    ensure!(*remaining > 1, "generic type arguments expand indefinitely");
    *remaining -= 1;
    match ty {
        HirType::Named(name) | HirType::TypeVar(name) => match bindings.get(name.as_str()) {
            Some(&bound) => {
                charge_budget(bound.hir(ctx), remaining)?;
                Ok(true)
            }
            None => {
                if matches!(ty, HirType::TypeVar(_)) {
                    bail!("unresolved type argument {name}");
                }
                Ok(false)
            }
        },
        HirType::Array(inner) | HirType::Promise(inner) => {
            check_subst(ctx, inner, bindings, remaining)
        }
        HirType::Tuple(items) | HirType::Union(items) => {
            let mut subst = false;
            for item in items {
                subst |= check_subst(ctx, item, bindings, remaining)?;
            }
            Ok(subst)
        }
        HirType::Object(obj) => {
            let mut subst = false;
            for prop in obj.properties.values() {
                subst |= check_subst(ctx, &prop.ty, bindings, remaining)?;
            }
            if let Some(idx) = &obj.index_signature {
                subst |= check_subst(ctx, idx, bindings, remaining)?;
            }
            Ok(subst)
        }
        HirType::Function(func) => {
            let mut subst = false;
            for (_, param_ty, _) in &func.params {
                subst |= check_subst(ctx, param_ty, bindings, remaining)?;
            }
            subst |= check_subst(ctx, &func.return_type, bindings, remaining)?;
            Ok(subst)
        }
        HirType::Generic { type_args, .. } => {
            let mut subst = false;
            for arg in type_args {
                subst |= check_subst(ctx, arg, bindings, remaining)?;
            }
            Ok(subst)
        }
        _ => Ok(false),
    }
}

fn validate_id(ctx: &Context, id: TypeId, remaining: &mut usize) -> Result<TypeId> {
    check_subst(ctx, id.hir(ctx), &HashMap::new(), remaining)?;
    Ok(id)
}

pub(super) fn substitute_id(
    ctx: &Context,
    ty: &HirType,
    bindings: &HashMap<&str, TypeId>,
    remaining: &mut usize,
) -> Result<TypeId> {
    if let HirType::Named(name) | HirType::TypeVar(name) = ty
        && let Some(&bound) = bindings.get(name.as_str())
    {
        ensure!(*remaining > 1, "generic type arguments expand indefinitely");
        *remaining -= 1;
        charge_budget(bound.hir(ctx), remaining)?;
        return Ok(bound);
    }
    let needs_subst = check_subst(ctx, ty, bindings, remaining)?;
    if !needs_subst {
        return Ok(TypeId::intern(ctx, ty));
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
    fn find<L: DeclarationLookup<'a>>(lookup: &L, base: &str) -> Option<(DeclarationId, Self)> {
        if let Some((index, interface)) = lookup.interface(base) {
            return Some((DeclarationId::Interface(index), Self::Interface(interface)));
        }
        if let Some((index, class)) = lookup.class(base) {
            return Some((DeclarationId::Class(index), Self::Class(class)));
        }
        let (index, alias) = lookup.type_alias(base)?;
        Some((DeclarationId::Alias(index), Self::Alias(alias)))
    }

    fn parameters(self) -> &'a [perry_hir::types::TypeParam] {
        match self {
            Self::Interface(interface) => &interface.type_params,
            Self::Alias(alias) => &alias.type_params,
            Self::Class(class) => &class.type_params,
        }
    }

    fn bind(
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

impl<'s> Shapes<'s> {
    pub(crate) fn class_value_type(
        &self,
        ctx: &Context,
        class_name: &str,
        type_args: &[TypeId],
        ty: &HirType,
    ) -> Result<TypeHandle> {
        let Some((_, class)) = self.classes.get(class_name) else {
            return self.value_type(ctx, ty);
        };
        if class.type_params.is_empty() {
            return self.value_type(ctx, ty);
        }
        let bindings = Declaration::Class(class).bind(ctx, class_name, type_args)?;
        let mut remaining = ARGUMENT_LIMIT;
        let arg = substitute_id(ctx, ty, &bindings, &mut remaining)?;
        self.value_type(ctx, arg.hir(ctx))
    }

    /// The MIR type of `base<arguments>`: missing arguments take their declared defaults,
    /// and each distinct instantiation is one named record (or the aliased type).
    pub(crate) fn generic(
        &self,
        ctx: &Context,
        base: &str,
        arguments: &[HirType],
    ) -> Result<TypeHandle> {
        if !self.is_user_type(base)
            && base == "Readonly"
            && let [inner] = arguments
        {
            return self.value_type(ctx, inner);
        }
        let resolver = Resolver::new(ctx, self);
        let arg_ids = TypeId::intern_slice(ctx, arguments);
        if !self.is_user_type(base) {
            if matches!(base, "Exclude" | "Extract" | "NonNullable") {
                let expanded = resolver.utility_type(base, &arg_ids, 0).with_context(|| {
                    format!("MIR backend could not resolve utility type {base}")
                })?;
                return self.value_type(ctx, expanded.hir(ctx));
            }
            if let Some(utility) = Utility::from_name(base) {
                let key = (DeclarationId::Utility(utility), arg_ids);
                if let Some(ty) = self.named.borrow().get(&key) {
                    return Ok(*ty);
                }
                let properties = resolver
                    .named(base, &key.1, false, Query::Record, 0)
                    .and_then(|(view, _)| view.properties(ctx))
                    .with_context(|| {
                        format!("MIR backend could not resolve utility type {base}")
                    })?;
                let ty: TypeHandle = self
                    .source_record(
                        ctx,
                        Some(&key),
                        false,
                        properties
                            .iter()
                            .map(|(n, ty, opt)| (n.as_str(), ty.hir(ctx), *opt))
                            .collect(),
                    )?
                    .into();
                if !self.pending.borrow().contains_key(&key) {
                    self.named.borrow_mut().insert(key, ty);
                }
                return Ok(ty);
            }
        }
        let (identity, declaration) = Declaration::find(self, base).with_context(|| {
            format!("MIR backend does not support unresolved generic type {base}")
        })?;
        let mut key = (identity, arg_ids);
        if key.1.len() == declaration.parameters().len()
            && let Some(ty) = self.named.borrow().get(&key)
        {
            return Ok(*ty);
        }
        let bindings = declaration.bind(ctx, base, &key.1)?;
        if key.1.len() < declaration.parameters().len() {
            key.1.extend(
                declaration.parameters()[key.1.len()..]
                    .iter()
                    .map(|parameter| bindings[parameter.name.as_str()]),
            );
            if let Some(ty) = self.named.borrow().get(&key) {
                return Ok(*ty);
            }
        }
        ensure!(
            self.pending.borrow().len() < 128,
            "generic type instantiation expands indefinitely: {base}"
        );
        let mut remaining = usize::MAX;
        let ty = match declaration {
            Declaration::Interface(_) | Declaration::Class(_) => {
                if let Declaration::Interface(interface) = declaration {
                    ensure!(
                        interface.methods.is_empty(),
                        "interface {base}: methods are not supported yet"
                    );
                }
                let nominal = matches!(declaration, Declaration::Class(_));
                let properties = resolver
                    .fields(declaration, &bindings, 0)
                    .with_context(|| {
                        let subject = if nominal {
                            "fields of class"
                        } else {
                            "inherited properties of interface"
                        };
                        format!("failed to resolve {subject} {base}")
                    })?;
                let ty: TypeHandle = self
                    .source_record(
                        ctx,
                        Some(&key),
                        nominal,
                        properties
                            .iter()
                            .map(|(name, ty, optional)| (name.as_str(), ty.hir(ctx), *optional))
                            .collect(),
                    )?
                    .into();
                if let Declaration::Class(class) = declaration {
                    self.class_records
                        .borrow_mut()
                        .insert(ty, (class.name.as_str(), key.1.clone()));
                }
                ty
            }
            Declaration::Alias(alias) => {
                let value = substitute_id(ctx, &alias.ty, &bindings, &mut remaining)?;
                let value_hir = value.hir(ctx);
                if let HirType::Object(obj) = value_hir
                    && obj.index_signature.is_none()
                {
                    self.source_record(
                        ctx,
                        Some(&key),
                        false,
                        obj.properties
                            .iter()
                            .map(|(name, p)| (name.as_str(), &p.ty, p.optional))
                            .collect(),
                    )?
                    .into()
                } else {
                    ensure!(
                        !self.pending.borrow().contains_key(&key),
                        "nonproductive recursive type alias {base}"
                    );
                    self.pending
                        .borrow_mut()
                        .insert(key.clone(), PendingShape::Resolving);
                    let value = self.value_type(ctx, value_hir);
                    self.pending.borrow_mut().remove(&key);
                    value?
                }
            }
        };
        if !self.pending.borrow().contains_key(&key) {
            self.named.borrow_mut().insert(key, ty);
        }
        Ok(ty)
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

pub(crate) fn optional_property(ty: HirType, optional: bool) -> HirType {
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

/// What iterating `ty` yields: an array element, a `Set` value, a live view's item, or a
/// `Map` entry pair. Any other type is handed back unchanged.
pub(crate) fn iterated_element(ty: HirType) -> Result<HirType, HirType> {
    let HirType::Generic { base, type_args } = ty else {
        return match ty {
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
    pub(crate) fn property_type(
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
    pub(crate) fn dict_entry_types(
        &self,
        ctx: &Context,
        ty: &HirType,
    ) -> Option<(HirType, HirType)> {
        if ty.any(&mut |n| matches!(n, HirType::TypeVar(_))) {
            return None;
        }
        let id = TypeId::intern(ctx, ty);
        Resolver::new(ctx, self).dictionary(id)
    }

    pub(crate) fn object_properties(
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
