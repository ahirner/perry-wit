//! MIR representation of instantiated frontend declarations.
use super::*;
use crate::shapes::generics::{Declaration, Query, Resolver};

impl Shapes<'_, Preparing<'_>> {
    /// The MIR type of `base<arguments>`: missing arguments take their declared defaults,
    /// and each distinct instantiation is one named record (or the aliased type).
    pub(super) fn generic(
        &mut self,
        ctx: &Context,
        base: &str,
        arguments: &[Ty],
    ) -> Result<TypeHandle> {
        let mut resolver = Resolver::new(ctx, self.decls, self.types.source);
        let declaration = Declaration::find(self.decls, base);
        let (key, value) = if let Some(declaration) = declaration {
            if let Declaration::Interface(interface) = declaration {
                ensure_unsupported!(
                    interface.methods.is_empty(),
                    "interface {base}: methods are not supported yet"
                );
            }
            let app = resolver.application(declaration, base, arguments, 0)?;
            (app.key, app.ty)
        } else {
            let key = resolver.types.application_key(base, arguments);
            if let Some(Some(ty)) = self.types.projections.get(&ShapeKey::Application(key)) {
                return Ok(*ty);
            }
            let value = match base {
                "Exclude" | "Extract" | "NonNullable" => resolver.utility_type(base, arguments, 0),
                "Partial" | "Required" | "Pick" | "Omit" => resolver
                    .named(base, arguments, false, Query::Record, 0)
                    .map(|(ty, _)| ty),
                _ => bail_diag!(
                    codes::UNSUPPORTED_TYPE,
                    "unresolved generic type {base} is not supported yet"
                ),
            }
            .ok_or_else(|| {
                diag!(
                    codes::UNSUPPORTED_TYPE,
                    "could not resolve utility type {base}"
                )
            })?;
            (key, value)
        };
        let application = ShapeKey::Application(key);
        if let Some(&Some(ty)) = self.types.projections.get(&application) {
            return Ok(ty);
        }
        ensure_diag!(
            self.types.depth < 128,
            codes::UNSUPPORTED_TYPE,
            "generic type instantiation expands indefinitely: {base}"
        );
        self.types.depth += 1;
        let result = if let Some(props) = self.types.source.closed_record(value) {
            let class = match declaration {
                Some(Declaration::Class(class)) => Some(class),
                _ => None,
            };
            self.declared_record(ctx, key, props, class)
        } else if self.types.projections.insert(application, None).is_none() {
            self.resolved_binding(ctx, value).map(|b| b.carrier)
        } else {
            Err(diag!(
                codes::TYPE_MISMATCH,
                "nonproductive recursive type alias {base}"
            ))
        };
        self.types.depth -= 1;
        match &result {
            Ok(ty) => {
                self.types.projections.insert(application, Some(*ty));
            }
            Err(_) => {
                self.types.projections.remove(&application);
            }
        }
        result
    }
}
