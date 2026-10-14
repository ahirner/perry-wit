//! Instantiation of generic interfaces and type aliases by substitution.
use super::*;
use perry_hir::ir::{Interface, TypeAlias};

pub(crate) trait DeclarationLookup {
    fn interface(&self, name: &str) -> Option<&Interface>;
    fn type_alias(&self, name: &str) -> Option<&TypeAlias>;
}

impl DeclarationLookup for (&[Interface], &[TypeAlias]) {
    fn interface(&self, name: &str) -> Option<&Interface> {
        self.0.iter().find(|i| i.name == name)
    }
    fn type_alias(&self, name: &str) -> Option<&TypeAlias> {
        self.1.iter().find(|a| a.name == name)
    }
}

impl DeclarationLookup for (&HashMap<&str, &Interface>, &HashMap<&str, &TypeAlias>) {
    fn interface(&self, name: &str) -> Option<&Interface> {
        self.0.get(name).copied()
    }
    fn type_alias(&self, name: &str) -> Option<&TypeAlias> {
        self.1.get(name).copied()
    }
}

impl Shapes<'_> {
    /// The MIR type of `base<arguments>`: missing arguments take their declared defaults,
    /// and each distinct instantiation is one named record (or the aliased type).
    pub(super) fn generic(
        &self,
        ctx: &mut Context,
        base: &str,
        arguments: &[HirType],
    ) -> Result<TypeHandle> {
        if !self.is_user_type(base) {
            if base == "Readonly"
                && let [inner] = arguments
            {
                return self.value_type(ctx, inner);
            }
            if matches!(base, "Partial" | "Required" | "Pick" | "Omit") {
                let key = format!("{base}<{arguments:?}>");
                if let Some(ty) = self.named.borrow().get(&key) {
                    return Ok(*ty);
                }
                let lookup = (&self.interfaces, &self.type_aliases);
                let properties = resolve_utility_record_properties(&lookup, base, arguments, 0)
                    .with_context(|| {
                        format!("MIR backend could not resolve utility type {base}")
                    })?;
                let ty: TypeHandle = self
                    .hir_record(
                        ctx,
                        Some(&key),
                        properties
                            .iter()
                            .map(|(n, ty, opt)| (n.as_str(), ty, *opt))
                            .collect(),
                    )?
                    .into();
                if !self.pending.borrow().contains_key(&key) {
                    self.named.borrow_mut().insert(key, ty);
                }
                return Ok(ty);
            }
        }
        let declaration = match self.interfaces.get(base) {
            Some(interface) => Declaration::Interface(interface),
            None => {
                Declaration::Alias(self.type_aliases.get(base).copied().with_context(|| {
                    format!("MIR backend does not support unresolved generic type {base}")
                })?)
            }
        };
        let parameters = match declaration {
            Declaration::Interface(interface) => &interface.type_params,
            Declaration::Alias(alias) => &alias.type_params,
        };
        ensure!(
            arguments.len() <= parameters.len(),
            "too many type arguments for {base}"
        );
        let mut substitutions = HashMap::new();
        for (index, parameter) in parameters.iter().enumerate() {
            let argument = match arguments.get(index) {
                Some(argument) => argument.clone(),
                None => substitute(
                    parameter.default.as_deref().with_context(|| {
                        format!("missing type argument {} of {base}", parameter.name)
                    })?,
                    &substitutions,
                ),
            };
            substitutions.insert(parameter.name.as_str(), argument);
        }
        let actual: Vec<&HirType> = parameters
            .iter()
            .map(|parameter| &substitutions[parameter.name.as_str()])
            .collect();
        // Bound the size of the arguments, which a recursive default could grow without end.
        let mut work = actual.clone();
        let mut nodes = 0;
        while let Some(ty) = work.pop() {
            nodes += 1;
            ensure!(
                nodes < 4096,
                "generic type arguments expand indefinitely: {base}"
            );
            match ty {
                HirType::Array(inner) | HirType::Promise(inner) => work.push(inner),
                HirType::Tuple(items) | HirType::Union(items) => work.extend(items),
                HirType::Generic { type_args, .. } => work.extend(type_args),
                HirType::Object(object) => {
                    work.extend(object.properties.values().map(|p| &p.ty));
                    if let Some(index_ty) = &object.index_signature {
                        work.push(index_ty);
                    }
                }
                _ => {}
            }
        }
        let key = format!("{base}<{actual:?}>");
        if let Some(ty) = self.named.borrow().get(&key) {
            return Ok(*ty);
        }
        ensure!(
            self.pending.borrow().len() < 128,
            "generic type instantiation expands indefinitely: {base}"
        );
        let ty = match declaration {
            Declaration::Interface(interface) => {
                ensure!(
                    interface.methods.is_empty(),
                    "interface {base}: methods are not supported yet"
                );
                let lookup = (&self.interfaces, &self.type_aliases);
                let properties =
                    collect_interface_properties(&lookup, interface, &substitutions, 0)
                        .with_context(|| {
                            format!("failed to resolve inherited properties of interface {base}")
                        })?;
                self.hir_record(
                    ctx,
                    Some(&key),
                    properties
                        .iter()
                        .map(|(name, ty, optional)| (name.as_str(), ty, *optional))
                        .collect(),
                )?
                .into()
            }
            Declaration::Alias(alias) => {
                let value = substitute(&alias.ty, &substitutions);
                if let HirType::Object(object) = &value
                    && object.index_signature.is_none()
                {
                    self.hir_record(
                        ctx,
                        Some(&key),
                        object
                            .properties
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
                    let value = self.value_type(ctx, &value);
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

/// The generic declaration an instantiation expands.
#[derive(Clone, Copy)]
enum Declaration<'a> {
    Interface(&'a Interface),
    Alias(&'a TypeAlias),
}

pub(super) fn collect_interface_properties<L: DeclarationLookup>(
    lookup: &L,
    interface: &Interface,
    substitutions: &HashMap<&str, HirType>,
    depth: usize,
) -> Option<Vec<(String, HirType, bool)>> {
    if depth >= 32 {
        return None;
    }
    let mut merged: Vec<(String, HirType, bool)> = Vec::new();
    for ext in &interface.extends {
        let ext_ty = match ext {
            HirType::Named(name) | HirType::Generic { base: name, .. } => {
                crate::collections::decode_hir_type(name).unwrap_or_else(|| ext.clone())
            }
            other => other.clone(),
        };
        let ext_sub = substitute(&ext_ty, substitutions);
        let parent_props = resolve_record_properties(lookup, &ext_sub, depth + 1)?;
        for (name, ty, opt) in parent_props {
            if let Some(existing) = merged.iter_mut().find(|(n, _, _)| *n == name) {
                *existing = (name, ty, opt);
            } else {
                merged.push((name, ty, opt));
            }
        }
    }
    for p in &interface.properties {
        let prop_ty = substitute(&p.ty, substitutions);
        if let Some(existing) = merged.iter_mut().find(|(n, _, _)| *n == p.name) {
            *existing = (p.name.clone(), prop_ty, p.optional);
        } else {
            merged.push((p.name.clone(), prop_ty, p.optional));
        }
    }
    Some(merged)
}

fn extract_string_keys<L: DeclarationLookup>(
    lookup: &L,
    ty: &HirType,
    depth: usize,
) -> Option<Vec<String>> {
    if depth >= 32 {
        return None;
    }
    match ty {
        HirType::StringLiteral(s) => Some(vec![s.clone()]),
        HirType::Union(members) => {
            let mut keys = Vec::new();
            for m in members {
                keys.extend(extract_string_keys(lookup, m, depth + 1)?);
            }
            Some(keys)
        }
        HirType::Named(name) => {
            let alias = lookup.type_alias(name)?;
            if !alias.type_params.is_empty() {
                return None;
            }
            extract_string_keys(lookup, &alias.ty, depth + 1)
        }
        _ => None,
    }
}

fn strip_void(ty: HirType) -> HirType {
    match ty {
        HirType::Union(items) => {
            let filtered: Vec<HirType> = items
                .into_iter()
                .filter(|item| !matches!(item, HirType::Void))
                .collect();
            if filtered.len() <= 1 {
                filtered.into_iter().next().unwrap_or(HirType::Void)
            } else {
                HirType::Union(filtered)
            }
        }
        other => other,
    }
}

pub(super) fn resolve_utility_record_properties<L: DeclarationLookup>(
    lookup: &L,
    base: &str,
    arguments: &[HirType],
    depth: usize,
) -> Option<Vec<(String, HirType, bool)>> {
    if depth >= 32 || lookup.interface(base).is_some() || lookup.type_alias(base).is_some() {
        return None;
    }
    match (base, arguments) {
        ("Readonly", [inner]) => resolve_record_properties(lookup, inner, depth + 1),
        ("Partial", [inner]) => {
            let mut props = resolve_record_properties(lookup, inner, depth + 1)?;
            for (_, _, opt) in &mut props {
                *opt = true;
            }
            Some(props)
        }
        ("Required", [inner]) => {
            let mut props = resolve_record_properties(lookup, inner, depth + 1)?;
            for (_, ty, opt) in &mut props {
                *opt = false;
                *ty = strip_void(ty.clone());
            }
            Some(props)
        }
        ("Pick", [inner, keys_ty]) => {
            let keys = extract_string_keys(lookup, keys_ty, depth + 1)?;
            let props = resolve_record_properties(lookup, inner, depth + 1)?;
            Some(
                props
                    .into_iter()
                    .filter(|(name, _, _)| keys.iter().any(|k| k == name))
                    .collect(),
            )
        }
        ("Omit", [inner, keys_ty]) => {
            let keys = extract_string_keys(lookup, keys_ty, depth + 1)?;
            let props = resolve_record_properties(lookup, inner, depth + 1)?;
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
    lookup: &L,
    ty: &HirType,
    depth: usize,
) -> Option<Vec<(String, HirType, bool)>> {
    if depth >= 32 {
        return None;
    }
    match ty {
        HirType::Object(object) if object.index_signature.is_none() => Some(
            object
                .properties
                .iter()
                .map(|(k, p)| (k.clone(), p.ty.clone(), p.optional))
                .collect(),
        ),
        HirType::Named(name) => resolve_generic_properties(lookup, name, &[], depth + 1),
        HirType::Generic { base, type_args } => {
            resolve_generic_properties(lookup, base, type_args, depth + 1)
        }
        _ => None,
    }
}

pub(super) fn resolve_generic_properties<L: DeclarationLookup>(
    lookup: &L,
    base: &str,
    arguments: &[HirType],
    depth: usize,
) -> Option<Vec<(String, HirType, bool)>> {
    if depth >= 32 {
        return None;
    }
    if let Some(props) = resolve_utility_record_properties(lookup, base, arguments, depth) {
        return Some(props);
    }
    let declaration = match lookup.interface(base) {
        Some(interface) => Declaration::Interface(interface),
        None => Declaration::Alias(lookup.type_alias(base)?),
    };
    let parameters = match declaration {
        Declaration::Interface(interface) => &interface.type_params,
        Declaration::Alias(alias) => &alias.type_params,
    };
    if arguments.len() > parameters.len() {
        return None;
    }
    let mut substitutions = HashMap::new();
    for (index, parameter) in parameters.iter().enumerate() {
        let argument = match arguments.get(index) {
            Some(argument) => argument.clone(),
            None => substitute(parameter.default.as_deref()?, &substitutions),
        };
        substitutions.insert(parameter.name.as_str(), argument);
    }
    match declaration {
        Declaration::Interface(interface) => {
            collect_interface_properties(lookup, interface, &substitutions, depth + 1)
        }
        Declaration::Alias(alias) => {
            let target = substitute(&alias.ty, &substitutions);
            resolve_record_properties(lookup, &target, depth + 1)
        }
    }
}

/// The substituted HIR type of `property` on `base<arguments>`, if `base` resolves to a record shape.
pub(crate) fn property_type(
    interfaces: &[Interface],
    type_aliases: &[TypeAlias],
    base: &str,
    arguments: &[HirType],
    property: &str,
) -> Option<HirType> {
    property_type_depth(interfaces, type_aliases, base, arguments, property, 0)
}

fn property_type_depth(
    interfaces: &[Interface],
    type_aliases: &[TypeAlias],
    base: &str,
    arguments: &[HirType],
    property: &str,
    depth: usize,
) -> Option<HirType> {
    if depth >= 32 {
        return None;
    }
    if base == "Record"
        && let [_, val_ty] = arguments
        && !interfaces.iter().any(|i| i.name == base)
        && !type_aliases.iter().any(|a| a.name == base)
    {
        return Some(optional_property(val_ty.clone(), true));
    }
    let lookup = (interfaces, type_aliases);
    if let Some(props) = resolve_generic_properties(&lookup, base, arguments, depth) {
        let (_, ty, optional) = props.into_iter().find(|(name, _, _)| name == property)?;
        return Some(optional_property(ty, optional));
    }
    let alias = type_aliases.iter().find(|a| a.name == base)?;
    if arguments.len() > alias.type_params.len() {
        return None;
    }
    let mut substitutions = HashMap::new();
    for (index, parameter) in alias.type_params.iter().enumerate() {
        let argument = match arguments.get(index) {
            Some(argument) => argument.clone(),
            None => substitute(parameter.default.as_deref()?, &substitutions),
        };
        substitutions.insert(parameter.name.as_str(), argument);
    }
    let target = match substitute(&alias.ty, &substitutions) {
        HirType::Union(members) => {
            let mut defined = members
                .into_iter()
                .filter(|m| !matches!(m, HirType::Null | HirType::Void));
            let single = defined.next()?;
            if defined.next().is_some() {
                return None;
            }
            single
        }
        other => other,
    };
    match target {
        HirType::Object(object) => {
            if let Some(prop) = object.properties.get(property) {
                return Some(optional_property(prop.ty.clone(), prop.optional));
            }
            if let Some(index_ty) = object.index_signature {
                return Some(optional_property(*index_ty, true));
            }
            None
        }
        HirType::Generic { base, type_args } => property_type_depth(
            interfaces,
            type_aliases,
            &base,
            &type_args,
            property,
            depth + 1,
        ),
        HirType::Named(name) => {
            property_type_depth(interfaces, type_aliases, &name, &[], property, depth + 1)
        }
        _ => None,
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

/// Extract `(key_ty, value_ty)` if `ty` resolves to `Record<K, V>`, `{ [key: string]: V }`, or a
/// closed record/interface whose properties share a uniform value type.
pub(crate) fn dict_entry_types(
    interfaces: &[Interface],
    type_aliases: &[TypeAlias],
    ty: &HirType,
) -> Option<(HirType, HirType)> {
    dict_entry_types_depth(interfaces, type_aliases, ty, 0)
}

fn dict_entry_types_depth(
    interfaces: &[Interface],
    type_aliases: &[TypeAlias],
    ty: &HirType,
    depth: usize,
) -> Option<(HirType, HirType)> {
    if depth >= 32 {
        return None;
    }
    let lookup = (interfaces, type_aliases);
    if let Some(props) = resolve_record_properties(&lookup, ty, depth) {
        let mut iter = props.into_iter();
        let (_, first_ty, first_opt) = iter.next()?;
        let first_ty = optional_property(first_ty, first_opt);
        if iter.all(|(_, ty, opt)| optional_property(ty, opt) == first_ty) {
            return Some((HirType::String, first_ty));
        }
        return None;
    }
    match ty {
        HirType::Generic { base, type_args }
            if base == "Record"
                && type_args.len() == 2
                && !interfaces.iter().any(|i| i.name == *base)
                && !type_aliases.iter().any(|a| a.name == *base) =>
        {
            Some((HirType::String, type_args[1].clone()))
        }
        HirType::Object(object) => {
            if let Some(val_ty) = &object.index_signature {
                return Some((HirType::String, (**val_ty).clone()));
            }
            None
        }
        HirType::Named(base) => {
            let alias = type_aliases.iter().find(|a| a.name == *base)?;
            if !alias.type_params.is_empty() {
                return dict_entry_types_generic(interfaces, type_aliases, base, &[], depth + 1);
            }
            dict_entry_types_depth(interfaces, type_aliases, &alias.ty, depth + 1)
        }
        HirType::Generic { base, type_args } => {
            dict_entry_types_generic(interfaces, type_aliases, base, type_args, depth + 1)
        }
        _ => None,
    }
}

fn dict_entry_types_generic(
    interfaces: &[Interface],
    type_aliases: &[TypeAlias],
    base: &str,
    arguments: &[HirType],
    depth: usize,
) -> Option<(HirType, HirType)> {
    let alias = type_aliases.iter().find(|a| a.name == base)?;
    if arguments.len() > alias.type_params.len() {
        return None;
    }
    let mut substitutions = HashMap::new();
    for (index, parameter) in alias.type_params.iter().enumerate() {
        let argument = match arguments.get(index) {
            Some(argument) => argument.clone(),
            None => substitute(parameter.default.as_deref()?, &substitutions),
        };
        substitutions.insert(parameter.name.as_str(), argument);
    }
    dict_entry_types_depth(
        interfaces,
        type_aliases,
        &substitute(&alias.ty, &substitutions),
        depth,
    )
}

/// `ty` with its type variables (and same-named type references) replaced by `arguments`.
fn substitute(ty: &HirType, arguments: &HashMap<&str, HirType>) -> HirType {
    match ty {
        HirType::TypeVar(name) | HirType::Named(name) if arguments.contains_key(name.as_str()) => {
            arguments[name.as_str()].clone()
        }
        HirType::Array(element) => HirType::Array(Box::new(substitute(element, arguments))),
        HirType::Promise(element) => HirType::Promise(Box::new(substitute(element, arguments))),
        HirType::Tuple(elements) => {
            HirType::Tuple(elements.iter().map(|t| substitute(t, arguments)).collect())
        }
        HirType::Union(elements) => {
            HirType::Union(elements.iter().map(|t| substitute(t, arguments)).collect())
        }
        HirType::Generic { base, type_args } => HirType::Generic {
            base: base.clone(),
            type_args: type_args.iter().map(|t| substitute(t, arguments)).collect(),
        },
        HirType::Object(object) => {
            let mut object = object.clone();
            for property in object.properties.values_mut() {
                property.ty = substitute(&property.ty, arguments);
            }
            if let Some(index_ty) = &mut object.index_signature {
                **index_ty = substitute(index_ty, arguments);
            }
            HirType::Object(object)
        }
        _ => ty.clone(),
    }
}
