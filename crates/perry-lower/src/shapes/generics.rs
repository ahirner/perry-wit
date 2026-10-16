//! Instantiation of generic interfaces and type aliases using interned source types.
use super::*;
use perry_hir::ir::{Interface, TypeAlias};

pub(crate) trait DeclarationLookup {
    fn interface(&self, name: &str) -> Option<(usize, &Interface)>;
    fn type_alias(&self, name: &str) -> Option<(usize, &TypeAlias)>;
}

impl DeclarationLookup for (&[Interface], &[TypeAlias]) {
    fn interface(&self, name: &str) -> Option<(usize, &Interface)> {
        self.0.iter().enumerate().find(|(_, i)| i.name == name)
    }
    fn type_alias(&self, name: &str) -> Option<(usize, &TypeAlias)> {
        self.1.iter().enumerate().find(|(_, a)| a.name == name)
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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum DeclarationId {
    Interface(usize),
    Alias(usize),
}

/// The generic declaration an instantiation expands.
#[derive(Clone, Copy)]
enum Declaration<'a> {
    Interface(&'a Interface),
    Alias(&'a TypeAlias),
}

impl<'a> Declaration<'a> {
    fn find<L: DeclarationLookup>(lookup: &'a L, base: &str) -> Option<(DeclarationId, Self)> {
        if let Some((index, interface)) = lookup.interface(base) {
            return Some((DeclarationId::Interface(index), Self::Interface(interface)));
        }
        let (index, alias) = lookup.type_alias(base)?;
        Some((DeclarationId::Alias(index), Self::Alias(alias)))
    }

    fn parameters(self) -> &'a [perry_hir::types::TypeParam] {
        match self {
            Self::Interface(interface) => &interface.type_params,
            Self::Alias(alias) => &alias.type_params,
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
        let mut remaining = 4096;
        for (index, parameter) in parameters.iter().enumerate() {
            let argument = match arguments.get(index) {
                Some(argument) => *argument,
                None => Argument::from_hir(
                    ctx,
                    parameter.default.as_deref().with_context(|| {
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

impl Shapes<'_> {
    /// The MIR type of `base<arguments>`: missing arguments take their declared defaults,
    /// and each distinct instantiation is one named record (or the aliased type).
    pub(super) fn generic(
        &self,
        ctx: &Context,
        base: &str,
        arguments: &[ArgumentId],
    ) -> Result<TypeHandle> {
        if !self.is_user_type(base) {
            if base == "Readonly"
                && let [inner] = arguments
            {
                return self.source_type(ctx, *inner);
            }
            if matches!(base, "Partial" | "Required" | "Pick" | "Omit") {
                let key = ShapeKey::Utility(base.to_owned(), arguments.to_vec());
                if let Some(ty) = self.named.borrow().get(&key) {
                    return Ok(*ty);
                }
                let lookup = (&self.interfaces, &self.type_aliases);
                let properties =
                    resolve_utility_record_properties(ctx, &lookup, base, arguments, 0)
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
        let lookup = (&self.interfaces, &self.type_aliases);
        let (identity, declaration) = Declaration::find(&lookup, base).with_context(|| {
            format!("MIR backend does not support unresolved generic type {base}")
        })?;
        let bindings = declaration.bind(ctx, base, arguments)?;
        let effective_args = declaration
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
            Declaration::Interface(interface) => {
                ensure!(
                    interface.methods.is_empty(),
                    "interface {base}: methods are not supported yet"
                );
                let properties =
                    collect_interface_properties(ctx, &lookup, interface, &bindings, 0)
                        .with_context(|| {
                            format!("failed to resolve inherited properties of interface {base}")
                        })?;
                self.source_record(
                    ctx,
                    Some(&key),
                    properties
                        .iter()
                        .map(|(name, ty, optional)| (name.as_str(), *ty, *optional))
                        .collect(),
                )?
                .into()
            }
            Declaration::Alias(alias) => {
                let value = Argument::from_hir(ctx, &alias.ty, &bindings, &mut remaining)?;
                let object_fields: Option<Vec<(String, ArgumentId, bool)>> = {
                    let value_ref = value.deref(ctx);
                    if let Kind::Object {
                        properties,
                        index_signature: None,
                        ..
                    } = value_ref.kind()
                    {
                        Some(
                            properties
                                .iter()
                                .map(|p| (p.name.clone(), p.ty, p.optional))
                                .collect(),
                        )
                    } else {
                        None
                    }
                };
                if let Some(properties) = object_fields {
                    self.source_record(
                        ctx,
                        Some(&key),
                        properties
                            .iter()
                            .map(|(name, ty, opt)| (name.as_str(), *ty, *opt))
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

pub(super) fn collect_interface_properties<L: DeclarationLookup>(
    ctx: &Context,
    lookup: &L,
    interface: &Interface,
    bindings: &HashMap<&str, ArgumentId>,
    depth: usize,
) -> Option<Vec<(String, ArgumentId, bool)>> {
    if depth >= 32 {
        return None;
    }
    let mut merged: Vec<(String, ArgumentId, bool)> = Vec::new();
    let mut remaining = 4096;
    for ext in &interface.extends {
        let ext_sub = Argument::from_hir(ctx, ext, bindings, &mut remaining).ok()?;
        let parent_props = resolve_record_properties(ctx, lookup, ext_sub, depth + 1)?;
        for (name, ty, opt) in parent_props {
            if let Some(existing) = merged.iter_mut().find(|(n, _, _)| *n == name) {
                *existing = (name, ty, opt);
            } else {
                merged.push((name, ty, opt));
            }
        }
    }
    for p in &interface.properties {
        let prop_ty = Argument::from_hir(ctx, &p.ty, bindings, &mut remaining).ok()?;
        if let Some(existing) = merged.iter_mut().find(|(n, _, _)| *n == p.name) {
            *existing = (p.name.clone(), prop_ty, p.optional);
        } else {
            merged.push((p.name.clone(), prop_ty, p.optional));
        }
    }
    Some(merged)
}

fn extract_string_keys<L: DeclarationLookup>(
    ctx: &Context,
    lookup: &L,
    ty: ArgumentId,
    depth: usize,
) -> Option<Vec<String>> {
    if depth >= 32 {
        return None;
    }
    match ty.deref(ctx).kind() {
        Kind::StringLiteral(s) => Some(vec![s.clone()]),
        Kind::Union(members) => {
            let members = members.clone();
            let mut keys = Vec::new();
            for m in members {
                keys.extend(extract_string_keys(ctx, lookup, m, depth + 1)?);
            }
            Some(keys)
        }
        Kind::Named(name) => {
            let (_, alias) = lookup.type_alias(name)?;
            if !alias.type_params.is_empty() {
                return None;
            }
            let mut remaining = 4096;
            let target =
                Argument::from_hir(ctx, &alias.ty, &HashMap::new(), &mut remaining).ok()?;
            extract_string_keys(ctx, lookup, target, depth + 1)
        }
        _ => None,
    }
}

fn strip_void_arg(ctx: &Context, ty: ArgumentId) -> ArgumentId {
    let items = match ty.deref(ctx).kind() {
        Kind::Union(items) => items.clone(),
        _ => return ty,
    };
    let filtered: Vec<ArgumentId> = items
        .into_iter()
        .filter(|item| !matches!(item.deref(ctx).kind(), Kind::Void))
        .collect();
    if filtered.is_empty() {
        Argument::from_kind(ctx, Kind::Void)
    } else if filtered.len() == 1 {
        filtered[0]
    } else {
        Argument::from_kind(ctx, Kind::Union(filtered))
    }
}

pub(super) fn resolve_utility_record_properties<L: DeclarationLookup>(
    ctx: &Context,
    lookup: &L,
    base: &str,
    arguments: &[ArgumentId],
    depth: usize,
) -> Option<Vec<(String, ArgumentId, bool)>> {
    if depth >= 32 || lookup.interface(base).is_some() || lookup.type_alias(base).is_some() {
        return None;
    }
    match (base, arguments) {
        ("Readonly", [inner]) => resolve_record_properties(ctx, lookup, *inner, depth + 1),
        ("Partial", [inner]) => {
            let mut props = resolve_record_properties(ctx, lookup, *inner, depth + 1)?;
            for (_, _, opt) in &mut props {
                *opt = true;
            }
            Some(props)
        }
        ("Required", [inner]) => {
            let mut props = resolve_record_properties(ctx, lookup, *inner, depth + 1)?;
            for (_, ty, opt) in &mut props {
                *opt = false;
                *ty = strip_void_arg(ctx, *ty);
            }
            Some(props)
        }
        ("Pick", [inner, keys_ty]) => {
            let keys = extract_string_keys(ctx, lookup, *keys_ty, depth + 1)?;
            let props = resolve_record_properties(ctx, lookup, *inner, depth + 1)?;
            Some(
                props
                    .into_iter()
                    .filter(|(name, _, _)| keys.iter().any(|k| k == name))
                    .collect(),
            )
        }
        ("Omit", [inner, keys_ty]) => {
            let keys = extract_string_keys(ctx, lookup, *keys_ty, depth + 1)?;
            let props = resolve_record_properties(ctx, lookup, *inner, depth + 1)?;
            Some(
                props
                    .into_iter()
                    .filter(|(name, _, _)| !keys.iter().any(|k| k == name))
                    .collect(),
            )
        }
        _ => None,
    }
}

pub(super) fn resolve_record_properties<L: DeclarationLookup>(
    ctx: &Context,
    lookup: &L,
    ty: ArgumentId,
    depth: usize,
) -> Option<Vec<(String, ArgumentId, bool)>> {
    if depth >= 32 {
        return None;
    }
    let kind_info = match ty.deref(ctx).kind() {
        Kind::Object {
            properties,
            index_signature: None,
            ..
        } => {
            return Some(
                properties
                    .iter()
                    .map(|p| (p.name.clone(), p.ty, p.optional))
                    .collect(),
            );
        }
        Kind::Named(name) => Some((name.clone(), Vec::new())),
        Kind::Generic { base, arguments } => Some((base.clone(), arguments.clone())),
        _ => None,
    }?;
    resolve_generic_properties(ctx, lookup, &kind_info.0, &kind_info.1, depth + 1)
}

pub(super) fn resolve_generic_properties<L: DeclarationLookup>(
    ctx: &Context,
    lookup: &L,
    base: &str,
    arguments: &[ArgumentId],
    depth: usize,
) -> Option<Vec<(String, ArgumentId, bool)>> {
    if depth >= 32 {
        return None;
    }
    if let Some(props) = resolve_utility_record_properties(ctx, lookup, base, arguments, depth) {
        return Some(props);
    }
    let (_, declaration) = Declaration::find(lookup, base)?;
    let bindings = declaration.bind(ctx, base, arguments).ok()?;
    match declaration {
        Declaration::Interface(interface) => {
            collect_interface_properties(ctx, lookup, interface, &bindings, depth + 1)
        }
        Declaration::Alias(alias) => {
            let mut remaining = 4096;
            let target = Argument::from_hir(ctx, &alias.ty, &bindings, &mut remaining).ok()?;
            resolve_record_properties(ctx, lookup, target, depth + 1)
        }
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
    let mut remaining = 4096;
    let arguments = arguments
        .iter()
        .map(|ty| Argument::from_hir(ctx, ty, &HashMap::new(), &mut remaining))
        .collect::<Result<Vec<_>>>()
        .ok()?;
    property_type_depth(ctx, interfaces, type_aliases, base, &arguments, property, 0)
}

fn property_type_depth(
    ctx: &Context,
    interfaces: &[Interface],
    type_aliases: &[TypeAlias],
    base: &str,
    arguments: &[ArgumentId],
    property: &str,
    depth: usize,
) -> Option<HirType> {
    if depth >= 32 {
        return None;
    }
    let lookup = (interfaces, type_aliases);
    if base == "Record"
        && let [_, val_ty] = arguments
        && lookup.interface(base).is_none()
        && lookup.type_alias(base).is_none()
    {
        return Some(optional_property(val_ty.deref(ctx).to_hir(ctx), true));
    }
    if let Some(props) = resolve_generic_properties(ctx, &lookup, base, arguments, depth) {
        let (_, ty, optional) = props.into_iter().find(|(name, _, _)| name == property)?;
        return Some(optional_property(ty.deref(ctx).to_hir(ctx), optional));
    }
    let (_, Declaration::Alias(alias)) = Declaration::find(&lookup, base)? else {
        return None;
    };
    let bindings = Declaration::Alias(alias).bind(ctx, base, arguments).ok()?;
    let mut remaining = 4096;
    let substituted = Argument::from_hir(ctx, &alias.ty, &bindings, &mut remaining).ok()?;
    let target = {
        let sub_ref = substituted.deref(ctx);
        if let Kind::Union(members) = sub_ref.kind() {
            let mut defined = members
                .iter()
                .copied()
                .filter(|m| !matches!(m.deref(ctx).kind(), Kind::Null | Kind::Void));
            let single = defined.next()?;
            if defined.next().is_some() {
                return None;
            }
            single
        } else {
            substituted
        }
    };
    enum TargetDispatch {
        Prop(ArgumentId, bool),
        Generic(String, Vec<ArgumentId>),
        Named(String),
    }
    let dispatch = {
        let target_ref = target.deref(ctx);
        match target_ref.kind() {
            Kind::Object {
                index_signature, ..
            } => {
                if let Some(prop) = target_ref.property(property) {
                    Some(TargetDispatch::Prop(prop.ty, prop.optional))
                } else {
                    index_signature.map(|idx_ty| TargetDispatch::Prop(idx_ty, true))
                }
            }
            Kind::Generic { base, arguments } => {
                Some(TargetDispatch::Generic(base.clone(), arguments.clone()))
            }
            Kind::Named(name) => Some(TargetDispatch::Named(name.clone())),
            _ => None,
        }
    }?;
    match dispatch {
        TargetDispatch::Prop(ty, optional) => {
            Some(optional_property(ty.deref(ctx).to_hir(ctx), optional))
        }
        TargetDispatch::Generic(base, type_args) => property_type_depth(
            ctx,
            interfaces,
            type_aliases,
            &base,
            &type_args,
            property,
            depth + 1,
        ),
        TargetDispatch::Named(name) => property_type_depth(
            ctx,
            interfaces,
            type_aliases,
            &name,
            &[],
            property,
            depth + 1,
        ),
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

fn optional_property_arg(ctx: &Context, ty: ArgumentId, optional: bool) -> ArgumentId {
    if !optional {
        return ty;
    }
    let void_arg = Argument::from_kind(ctx, Kind::Void);
    let items = match ty.deref(ctx).kind() {
        Kind::Union(items) => {
            let mut items = items.clone();
            if !items.contains(&void_arg) {
                items.push(void_arg);
            }
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
    let mut remaining = 4096;
    let arg = Argument::from_hir(ctx, ty, &HashMap::new(), &mut remaining).ok()?;
    let (k, v) = dict_entry_types_depth(ctx, interfaces, type_aliases, arg, 0)?;
    Some((k.deref(ctx).to_hir(ctx), v.deref(ctx).to_hir(ctx)))
}

fn dict_entry_types_depth(
    ctx: &Context,
    interfaces: &[Interface],
    type_aliases: &[TypeAlias],
    ty: ArgumentId,
    depth: usize,
) -> Option<(ArgumentId, ArgumentId)> {
    if depth >= 32 {
        return None;
    }
    let lookup = (interfaces, type_aliases);
    if let Some(props) = resolve_record_properties(ctx, &lookup, ty, depth) {
        let any_opt = props.iter().any(|(_, _, opt)| *opt);
        let mut iter = props.into_iter();
        let (_, first_ty, _) = iter.next()?;
        let first_ty = optional_property_arg(ctx, first_ty, any_opt);
        if iter.all(|(_, ty, _)| optional_property_arg(ctx, ty, any_opt) == first_ty) {
            return Some((Argument::from_kind(ctx, Kind::String), first_ty));
        }
        return None;
    }
    enum DictDispatch {
        IndexSig(ArgumentId),
        Named(String),
        Generic(String, Vec<ArgumentId>),
    }
    let dispatch = {
        let ty_ref = ty.deref(ctx);
        match ty_ref.kind() {
            Kind::Generic { base, arguments }
                if base == "Record"
                    && arguments.len() == 2
                    && lookup.interface(base).is_none()
                    && lookup.type_alias(base).is_none() =>
            {
                Some(DictDispatch::IndexSig(arguments[1]))
            }
            Kind::Object {
                index_signature: Some(val_ty),
                ..
            } => Some(DictDispatch::IndexSig(*val_ty)),
            Kind::Named(base) => Some(DictDispatch::Named(base.clone())),
            Kind::Generic { base, arguments } => {
                Some(DictDispatch::Generic(base.clone(), arguments.clone()))
            }
            _ => None,
        }
    }?;
    match dispatch {
        DictDispatch::IndexSig(val_ty) => Some((Argument::from_kind(ctx, Kind::String), val_ty)),
        DictDispatch::Named(base) => {
            let (_, alias) = lookup.type_alias(&base)?;
            if !alias.type_params.is_empty() {
                return dict_entry_types_generic(
                    ctx,
                    interfaces,
                    type_aliases,
                    &base,
                    &[],
                    depth + 1,
                );
            }
            let mut remaining = 4096;
            let target =
                Argument::from_hir(ctx, &alias.ty, &HashMap::new(), &mut remaining).ok()?;
            dict_entry_types_depth(ctx, interfaces, type_aliases, target, depth + 1)
        }
        DictDispatch::Generic(base, arguments) => {
            dict_entry_types_generic(ctx, interfaces, type_aliases, &base, &arguments, depth + 1)
        }
    }
}

fn dict_entry_types_generic(
    ctx: &Context,
    interfaces: &[Interface],
    type_aliases: &[TypeAlias],
    base: &str,
    arguments: &[ArgumentId],
    depth: usize,
) -> Option<(ArgumentId, ArgumentId)> {
    let lookup = (interfaces, type_aliases);
    let (_, Declaration::Alias(alias)) = Declaration::find(&lookup, base)? else {
        return None;
    };
    let bindings = Declaration::Alias(alias).bind(ctx, base, arguments).ok()?;
    let mut remaining = 4096;
    let target = Argument::from_hir(ctx, &alias.ty, &bindings, &mut remaining).ok()?;
    dict_entry_types_depth(ctx, interfaces, type_aliases, target, depth)
}
