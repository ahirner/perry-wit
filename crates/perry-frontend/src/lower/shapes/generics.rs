//! MIR representation of instantiated frontend declarations.
use super::*;
use crate::shapes::generics::{Declaration, Query, Resolver};

impl<'h> Shapes<'h, Preparing<'_>> {
    /// The MIR type of `base<arguments>`: missing arguments take their declared defaults,
    /// and each distinct instantiation is one named record (or the aliased type).
    pub(super) fn generic(
        &mut self,
        ctx: &Context,
        written: InstanceKey,
        declaration: Option<Declaration<'h>>,
    ) -> Result<TypeHandle> {
        let mut resolver = Resolver::new(ctx, self.decls, self.types.source);
        let (base, args) = written;
        let base = base.get(ctx).as_str();
        let (key, value) = if let Some(declaration) = declaration {
            if let Declaration::Interface(interface) = declaration {
                ensure_unsupported!(
                    interface.methods.is_empty(),
                    "interface {base}: methods are not supported yet"
                );
            }
            let app = resolver.application(declaration, written, 0)?;
            (app.key, app.ty)
        } else {
            let arguments = resolver.types.ty_slice(args).to_vec();
            let value = match base {
                "Exclude" | "Extract" | "NonNullable" => resolver.utility_type(base, &arguments, 0),
                "Partial" | "Required" | "Pick" | "Omit" => resolver
                    .utility(base, &arguments, Query::Record, 0)
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
            (written, value)
        };
        let application = (ShapeKey::Application(key), self.types.wit_input);
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
