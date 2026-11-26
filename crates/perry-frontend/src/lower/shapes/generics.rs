//! MIR representation of instantiated frontend declarations.
use super::*;
use crate::shapes::generics::{Declaration, DeclarationId, Expansion, Query, Resolver, Utility};

impl Shapes<'_, Preparing<'_>> {
    /// The MIR type of `base<arguments>`: missing arguments take their declared defaults,
    /// and each distinct instantiation is one named record (or the aliased type).
    pub(super) fn generic(
        &mut self,
        ctx: &Context,
        base: &str,
        arguments: &[Ty],
    ) -> Result<TypeHandle> {
        if !self.is_user_type(base) {
            if matches!(base, "Exclude" | "Extract" | "NonNullable") {
                let expanded = Resolver::new(ctx, self.decls, self.types.source)
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
                if let Some(ty) = self.types.named.get(&key) {
                    return Ok(*ty);
                }
                let properties = {
                    let types = &mut *self.types.source;
                    Resolver::new(ctx, self.decls, types)
                        .named(base, &key.1, false, Query::Record, 0)
                        .and_then(|(ty, _)| types.closed_record(ty))
                }
                .ok_or_else(|| {
                    diag!(
                        codes::UNSUPPORTED_TYPE,
                        "could not resolve utility type {base}"
                    )
                })?;
                let ty: TypeHandle = self.source_record(ctx, &key, properties)?.into();
                if !self.types.pending.contains_key(&key) {
                    self.types.named.insert(key, ty);
                }
                return Ok(ty);
            }
        }
        let (identity, declaration) = Declaration::find(self.decls, base).ok_or_else(|| {
            diag!(
                codes::UNSUPPORTED_TYPE,
                "unresolved generic type {base} is not supported yet"
            )
        })?;
        let mut key = (identity, arguments.to_vec());
        if key.1.len() == declaration.parameters().len()
            && let Some(ty) = self.types.named.get(&key)
        {
            return Ok(*ty);
        }
        let bindings = declaration.bind(ctx, base, self.types.source, &key.1)?;
        if key.1.len() < declaration.parameters().len() {
            key.1
                .extend(bindings[key.1.len()..].iter().map(|&(_, ty)| ty));
            if let Some(ty) = self.types.named.get(&key) {
                return Ok(*ty);
            }
        }
        ensure_diag!(
            self.types.pending.len() < 128,
            codes::UNSUPPORTED_TYPE,
            "generic type instantiation expands indefinitely: {base}"
        );
        if let Declaration::Interface(interface) = declaration {
            ensure_unsupported!(
                interface.methods.is_empty(),
                "interface {base}: methods are not supported yet"
            );
        }
        let expansion =
            Resolver::new(ctx, self.decls, self.types.source).expand(declaration, &bindings, 0)?;
        let ty = match expansion {
            Expansion::Class(class, props) => {
                if let Some(PendingShape::Recursive(record)) = self.types.pending.get(&key) {
                    return Ok((*record).into());
                }
                let record = self
                    .records
                    .fresh(ctx, RecordKind::Object, None)
                    .to_diag(ctx)?;
                self.types
                    .pending
                    .insert(key.clone(), PendingShape::Recursive(record));
                let fields = self.types.source.object_slice(props).to_vec();
                let fields = self.record_fields(ctx, &fields, &mut Vec::new());
                self.types.pending.remove(&key);
                let mut fields = fields?;
                fields.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                let name = record.deref(ctx).name().clone();
                let ty = RecordType::get(ctx, name, Some(fields))
                    .to_diag(ctx)?
                    .into();
                self.class_records.insert(ty, class);
                ty
            }
            Expansion::Type(value) => {
                if let Some(props) = self.types.source.closed_record(value) {
                    self.source_record(ctx, &key, props)?.into()
                } else {
                    ensure_diag!(
                        !self.types.pending.contains_key(&key),
                        codes::TYPE_MISMATCH,
                        "nonproductive recursive type alias {base}"
                    );
                    self.types
                        .pending
                        .insert(key.clone(), PendingShape::Resolving);
                    let value = self.resolved_binding(ctx, value).map(|b| b.carrier);
                    self.types.pending.remove(&key);
                    value?
                }
            }
        };
        if !self.types.pending.contains_key(&key) {
            self.types.named.insert(key, ty);
        }
        Ok(ty)
    }
}
