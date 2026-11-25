//! MIR representation of instantiated frontend declarations.
use super::*;
use crate::shapes::generics::{Declaration, DeclarationId, Query, Resolver, Utility, substitute};

impl<'s> Shapes<'s> {
    /// The MIR type of `base<arguments>`: missing arguments take their declared defaults,
    /// and each distinct instantiation is one named record (or the aliased type).
    pub(crate) fn generic(
        &self,
        ctx: &Context,
        base: &str,
        arguments: &[Ty],
    ) -> Result<TypeHandle> {
        if !self.is_user_type(base) {
            if matches!(base, "Exclude" | "Extract" | "NonNullable") {
                let expanded = Resolver::new(
                    ctx,
                    &self.decls,
                    &mut self.specialization.types.borrow_mut(),
                )
                .utility_type(base, arguments, 0)
                .ok_or_else(|| {
                    diag!(
                        codes::UNSUPPORTED_TYPE,
                        "could not resolve utility type {base}"
                    )
                })?;
                return Ok(self.resolved_binding(ctx, expanded)?.carrier);
            }
            if let Some(utility) = Utility::from_name(base) {
                let key = (DeclarationId::Utility(utility), arguments.to_vec());
                if let Some(ty) = self.named.borrow().get(&key) {
                    return Ok(*ty);
                }
                let properties = {
                    let mut types = self.specialization.types.borrow_mut();
                    Resolver::new(ctx, &self.decls, &mut types)
                        .named(base, &key.1, false, Query::Record, 0)
                        .and_then(|(view, _)| view.properties(&types))
                }
                .ok_or_else(|| {
                    diag!(
                        codes::UNSUPPORTED_TYPE,
                        "could not resolve utility type {base}"
                    )
                })?;
                let ty: TypeHandle = self.source_record(ctx, Some(&key), properties)?.into();
                if !self.pending.borrow().contains_key(&key) {
                    self.named.borrow_mut().insert(key, ty);
                }
                return Ok(ty);
            }
        }
        let (identity, declaration) = Declaration::find(&self.decls, base).ok_or_else(|| {
            diag!(
                codes::UNSUPPORTED_TYPE,
                "unresolved generic type {base} is not supported yet"
            )
        })?;
        let mut key = (identity, arguments.to_vec());
        if key.1.len() == declaration.parameters().len()
            && let Some(ty) = self.named.borrow().get(&key)
        {
            return Ok(*ty);
        }
        let bindings = declaration.bind(
            ctx,
            base,
            &mut self.specialization.types.borrow_mut(),
            &key.1,
        )?;
        if key.1.len() < declaration.parameters().len() {
            key.1
                .extend(bindings[key.1.len()..].iter().map(|&(_, ty)| ty));
            if let Some(ty) = self.named.borrow().get(&key) {
                return Ok(*ty);
            }
        }
        ensure_diag!(
            self.pending.borrow().len() < 128,
            codes::UNSUPPORTED_TYPE,
            "generic type instantiation expands indefinitely: {base}"
        );
        let mut remaining = usize::MAX;
        let ty = match declaration {
            Declaration::Interface(interface) => {
                ensure_unsupported!(
                    interface.methods.is_empty(),
                    "interface {base}: methods are not supported yet"
                );
                let properties = Resolver::new(
                    ctx,
                    &self.decls,
                    &mut self.specialization.types.borrow_mut(),
                )
                .fields(declaration, &bindings, 0)
                .ok_or_else(|| {
                    diag!(
                        codes::UNSUPPORTED_TYPE,
                        "failed to resolve inherited properties of interface {base}"
                    )
                })?;
                self.source_record(ctx, Some(&key), properties)?.into()
            }
            Declaration::Class(class) => {
                if let Some(PendingShape::Recursive(record)) = self.pending.borrow().get(&key) {
                    return Ok((*record).into());
                }
                let record = fresh_record(ctx, RecordKind::Object, None)?;
                self.pending
                    .borrow_mut()
                    .insert(key.clone(), PendingShape::Recursive(record));
                let layout = self
                    .specialization
                    .instances
                    .class_fields
                    .get(class.name.as_str());
                let fields = class
                    .fields
                    .iter()
                    .map(|field| {
                        let ty = match layout.and_then(|layout| layout.instance.get(&field.name)) {
                            Some(&ty) => self.resolved_binding(ctx, ty)?.expected_type(ctx),
                            None => {
                                let ty = substitute(
                                    ctx,
                                    &mut self.specialization.types.borrow_mut(),
                                    &field.ty,
                                    &bindings,
                                    &mut remaining,
                                )?;
                                self.resolved_binding(ctx, ty)?.expected_type(ctx)
                            }
                        };
                        Ok((field.name.clone(), ty))
                    })
                    .collect::<Result<Vec<_>>>();
                self.pending.borrow_mut().remove(&key);
                let mut fields = fields?;
                fields.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                let name = record.deref(ctx).name().clone();
                let ty = RecordType::get(ctx, name, Some(fields))
                    .to_diag(ctx)?
                    .into();
                self.class_records
                    .borrow_mut()
                    .insert(ty, class.name.as_str());
                ty
            }
            Declaration::Alias(alias) => {
                let value = substitute(
                    ctx,
                    &mut self.specialization.types.borrow_mut(),
                    &alias.ty,
                    &bindings,
                    &mut remaining,
                )?;
                let kind = self.specialization.types.borrow().kind(value);
                if let TyKind::Object { props, index: None } = kind {
                    let fields = object_properties(&self.specialization.types.borrow(), props);
                    self.source_record(ctx, Some(&key), fields)?.into()
                } else {
                    ensure_diag!(
                        !self.pending.borrow().contains_key(&key),
                        codes::TYPE_MISMATCH,
                        "nonproductive recursive type alias {base}"
                    );
                    self.pending
                        .borrow_mut()
                        .insert(key.clone(), PendingShape::Resolving);
                    let value = self.resolved_binding(ctx, value).map(|b| b.carrier);
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
