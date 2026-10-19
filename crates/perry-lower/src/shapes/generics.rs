//! Instantiation of generic interfaces, type aliases, and classes using interned source types.
use super::*;
use perry_hir::ir::{Class, Interface, TypeAlias};

mod resolution;
use resolution::Query;
pub(super) use resolution::Resolver;

const RESOLUTION_DEPTH: usize = 32;
const ARGUMENT_LIMIT: usize = 4096;

pub(super) trait DeclarationLookup {
    fn interface(&self, name: &str) -> Option<(usize, &Interface)>;
    fn type_alias(&self, name: &str) -> Option<(usize, &TypeAlias)>;
    fn class(&self, _name: &str) -> Option<(usize, &Class)> {
        None
    }
}

impl DeclarationLookup for (&[Interface], &[TypeAlias]) {
    fn interface(&self, name: &str) -> Option<(usize, &Interface)> {
        self.0.iter().enumerate().find(|(_, i)| i.name == name)
    }
    fn type_alias(&self, name: &str) -> Option<(usize, &TypeAlias)> {
        self.1.iter().enumerate().find(|(_, a)| a.name == name)
    }
}

impl DeclarationLookup for (&[Interface], &[TypeAlias], &[Class]) {
    fn interface(&self, name: &str) -> Option<(usize, &Interface)> {
        self.0.iter().enumerate().find(|(_, i)| i.name == name)
    }
    fn type_alias(&self, name: &str) -> Option<(usize, &TypeAlias)> {
        self.1.iter().enumerate().find(|(_, a)| a.name == name)
    }
    fn class(&self, name: &str) -> Option<(usize, &Class)> {
        self.2.iter().enumerate().find(|(_, c)| c.name == name)
    }
}

impl DeclarationLookup
    for (
        &HashMap<&str, (usize, &Interface)>,
        &HashMap<&str, (usize, &TypeAlias)>,
    )
{
    fn interface(&self, name: &str) -> Option<(usize, &Interface)> {
        self.0.get(name).copied()
    }
    fn type_alias(&self, name: &str) -> Option<(usize, &TypeAlias)> {
        self.1.get(name).copied()
    }
}

impl DeclarationLookup
    for (
        &HashMap<&str, (usize, &Interface)>,
        &HashMap<&str, (usize, &TypeAlias)>,
        &HashMap<&str, (usize, &Class)>,
    )
{
    fn interface(&self, name: &str) -> Option<(usize, &Interface)> {
        self.0.get(name).copied()
    }
    fn type_alias(&self, name: &str) -> Option<(usize, &TypeAlias)> {
        self.1.get(name).copied()
    }
    fn class(&self, name: &str) -> Option<(usize, &Class)> {
        self.2.get(name).copied()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum DeclarationId {
    Interface(usize),
    Alias(usize),
    Class(usize),
}

/// The generic declaration an instantiation expands.
#[derive(Clone, Copy)]
enum Declaration<'a> {
    Interface(&'a Interface),
    Alias(&'a TypeAlias),
    Class(&'a Class),
}

impl<'a> Declaration<'a> {
    fn find<L: DeclarationLookup>(lookup: &'a L, base: &str) -> Option<(DeclarationId, Self)> {
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
        arguments: &[ArgumentId],
    ) -> Result<HashMap<&'a str, ArgumentId>> {
        let parameters = self.parameters();
        ensure!(
            arguments.len() <= parameters.len(),
            "too many type arguments for {base}"
        );
        let mut bindings = HashMap::with_capacity(parameters.len());
        let mut remaining = ARGUMENT_LIMIT;
        for (index, parameter) in parameters.iter().enumerate() {
            let argument = match arguments.get(index) {
                Some(argument) => *argument,
                None => Argument::from_hir(
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

fn generic_parts(kind: &Kind) -> Option<(&str, &[ArgumentId])> {
    match kind {
        Kind::Named(name) => Some((name, &[])),
        Kind::Generic { base, arguments } => Some((base, arguments)),
        _ => None,
    }
}

fn union_of(ctx: &Context, members: Vec<ArgumentId>) -> ArgumentId {
    match members.as_slice() {
        [] => Argument::from_kind(ctx, Kind::Never),
        [single] => *single,
        _ => Argument::from_kind(ctx, Kind::Union(members)),
    }
}

impl<'s> Shapes<'s> {
    pub(crate) fn class_value_type(
        &self,
        ctx: &Context,
        class_name: &str,
        type_args: &[ArgumentId],
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
        let arg = Argument::from_hir(ctx, ty, &bindings, &mut remaining)?;
        self.source_type(ctx, arg)
    }

    /// The MIR type of `base<arguments>`: missing arguments take their declared defaults,
    /// and each distinct instantiation is one named record (or the aliased type).
    pub(crate) fn generic(
        &self,
        ctx: &Context,
        base: &str,
        arguments: &[ArgumentId],
    ) -> Result<TypeHandle> {
        let lookup = (&self.interfaces, &self.type_aliases, &self.classes);
        let resolver = Resolver::new(ctx, &lookup);
        if !self.is_user_type(base) {
            if base == "Readonly"
                && let [inner] = arguments
            {
                return self.source_type(ctx, *inner);
            }
            if matches!(base, "Exclude" | "Extract" | "NonNullable") {
                let expanded = resolver.utility_type(base, arguments, 0).with_context(|| {
                    format!("MIR backend could not resolve utility type {base}")
                })?;
                return self.source_type(ctx, expanded);
            }
            if matches!(base, "Partial" | "Required" | "Pick" | "Omit") {
                let key = ShapeKey::Utility(base.to_owned(), arguments.to_vec());
                if let Some(ty) = self.named.borrow().get(&key) {
                    return Ok(*ty);
                }
                let properties = resolver
                    .named(base, arguments, false, Query::Record, 0)
                    .and_then(|(view, _)| view.properties(ctx))
                    .with_context(|| {
                        format!("MIR backend could not resolve utility type {base}")
                    })?;
                let ty: TypeHandle = self
                    .source_record(
                        ctx,
                        Some(&key),
                        properties
                            .iter()
                            .map(|(n, ty, opt)| (n.as_str(), *ty, *opt))
                            .collect(),
                    )?
                    .into();
                if !self.pending.borrow().contains_key(&key) {
                    self.named.borrow_mut().insert(key, ty);
                }
                return Ok(ty);
            }
        }
        let (identity, declaration) = Declaration::find(&lookup, base).with_context(|| {
            format!("MIR backend does not support unresolved generic type {base}")
        })?;
        let bindings = declaration.bind(ctx, base, arguments)?;
        let effective_args: Vec<ArgumentId> = declaration
            .parameters()
            .iter()
            .map(|parameter| bindings[parameter.name.as_str()])
            .collect();
        let key = ShapeKey::Generic(identity, effective_args);
        if let Some(ty) = self.named.borrow().get(&key) {
            return Ok(*ty);
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
                let prefix = match declaration {
                    Declaration::Class(class) => Some(format!("class_{}", class.name)),
                    _ => None,
                };
                let properties = resolver
                    .fields(declaration, &bindings, 0)
                    .with_context(|| {
                        let subject = if prefix.is_some() {
                            "fields of class"
                        } else {
                            "inherited properties of interface"
                        };
                        format!("failed to resolve {subject} {base}")
                    })?;
                let ty: TypeHandle = self
                    .source_record_prefix(
                        ctx,
                        Some(&key),
                        prefix.as_deref(),
                        properties
                            .iter()
                            .map(|(name, ty, optional)| (name.as_str(), *ty, *optional))
                            .collect(),
                    )?
                    .into();
                if let Declaration::Class(class) = declaration
                    && let ShapeKey::Generic(_, arguments) = &key
                {
                    self.class_records
                        .borrow_mut()
                        .insert(ty, (class.name.clone(), arguments.clone()));
                }
                ty
            }
            Declaration::Alias(alias) => {
                let value = Argument::from_hir(ctx, &alias.ty, &bindings, &mut remaining)?;
                let value_ref = value.deref(ctx);
                if let Kind::Object {
                    properties,
                    index_signature: None,
                    ..
                } = value_ref.kind()
                {
                    self.source_record(
                        ctx,
                        Some(&key),
                        properties
                            .iter()
                            .map(|p| (p.name.as_str(), p.ty, p.optional))
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
                    let value = self.source_type(ctx, value);
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

fn strip_void_arg(ctx: &Context, ty: ArgumentId) -> ArgumentId {
    let argument = ty.deref(ctx);
    let Kind::Union(items) = argument.kind() else {
        return ty;
    };
    let filtered: Vec<ArgumentId> = items
        .iter()
        .copied()
        .filter(|item| !matches!(item.deref(ctx).kind(), Kind::Void))
        .collect();
    if filtered.is_empty() {
        Argument::from_kind(ctx, Kind::Void)
    } else {
        union_of(ctx, filtered)
    }
}

/// HIR compatibility result for a selected member; intermediate alias expansions stay interned.
pub(crate) fn property_type(
    ctx: &Context,
    interfaces: &[Interface],
    type_aliases: &[TypeAlias],
    base: &str,
    arguments: &[HirType],
    property: &str,
) -> Option<HirType> {
    let mut remaining = ARGUMENT_LIMIT;
    let arguments = arguments
        .iter()
        .map(|ty| Argument::from_hir(ctx, ty, &HashMap::new(), &mut remaining))
        .collect::<Result<Vec<_>>>()
        .ok()?;
    let lookup = (interfaces, type_aliases);
    let (ty, optional) = Resolver::new(ctx, &lookup).property(base, &arguments, property)?;
    Some(optional_property(ty.deref(ctx).to_hir(ctx), optional))
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

fn optional_property_arg(ctx: &Context, ty: ArgumentId, optional: bool) -> ArgumentId {
    if !optional {
        return ty;
    }
    let void_arg = Argument::from_kind(ctx, Kind::Void);
    let items = match ty.deref(ctx).kind() {
        Kind::Union(items) => {
            if items.contains(&void_arg) {
                return ty;
            }
            let mut items = items.clone();
            items.push(void_arg);
            items
        }
        _ => vec![ty, void_arg],
    };
    Argument::from_kind(ctx, Kind::Union(items))
}

/// Extract `(key_ty, value_ty)` if `ty` resolves to `Record<K, V>`, `{ [key: string]: V }`, or a
/// closed record/interface whose properties share a uniform value type.
pub(crate) fn dict_entry_types(
    ctx: &Context,
    interfaces: &[Interface],
    type_aliases: &[TypeAlias],
    ty: &HirType,
) -> Option<(HirType, HirType)> {
    let mut remaining = ARGUMENT_LIMIT;
    let arg = Argument::from_hir(ctx, ty, &HashMap::new(), &mut remaining).ok()?;
    let lookup = (interfaces, type_aliases);
    let (k, v) = Resolver::new(ctx, &lookup).dictionary(arg)?;
    Some((k.deref(ctx).to_hir(ctx), v.deref(ctx).to_hir(ctx)))
}

pub(crate) fn object_properties(
    ctx: &Context,
    interfaces: &[Interface],
    type_aliases: &[TypeAlias],
    ty: &HirType,
) -> Option<Vec<(String, HirType, bool)>> {
    let mut remaining = ARGUMENT_LIMIT;
    let arg = Argument::from_hir(ctx, ty, &HashMap::new(), &mut remaining).ok()?;
    let lookup = (interfaces, type_aliases);
    let props = Resolver::new(ctx, &lookup).record(arg, 0)?;
    Some(
        props
            .into_iter()
            .map(|(name, prop_arg, opt)| (name, prop_arg.deref(ctx).to_hir(ctx), opt))
            .collect(),
    )
}
