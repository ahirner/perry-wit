//! MIR representation of instantiated frontend declarations.
use super::*;
use perry_frontend::hir::ir::{Class, Interface, TypeAlias};
use perry_frontend::shapes::generics::{
    ARGUMENT_LIMIT, Declaration, DeclarationId, DeclarationLookup, Query, Resolver, Utility,
    substitute_id,
};
impl<'h> DeclarationLookup<'h> for Shapes<'h> {
    fn interface(&self, name: &str) -> Option<(usize, &'h Interface)> {
        self.decls.interface(name)
    }
    fn type_alias(&self, name: &str) -> Option<(usize, &'h TypeAlias)> {
        self.decls.type_alias(name)
    }
    fn class(&self, name: &str) -> Option<(usize, &'h Class)> {
        self.classes.named.get(name).copied()
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
        let Some((_, class)) = self.classes.named.get(class_name) else {
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
