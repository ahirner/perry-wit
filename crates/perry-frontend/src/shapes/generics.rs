//! Instantiation of generic interfaces, type aliases, and classes.
use super::*;
use crate::engine::types::{Ty, TyKind, TypeListId, TypeTable};
use perry_hir::ir::{Interface, TypeAlias};
use perry_wit_diagnostics::{codes, diag, ensure_diag, ensure_unsupported};
use pliron::utils::vec_exns::VecExtns;

mod resolution;
pub(crate) use resolution::{Query, Resolver};

const RESOLUTION_DEPTH: usize = 32;
pub(crate) const ARGUMENT_LIMIT: usize = 4096;

pub(crate) type InstanceKey = (TypeName, TypeListId);

#[derive(Clone, Copy)]
pub(crate) struct Application {
    pub(crate) key: InstanceKey,
    pub(crate) ty: Ty,
    pub(crate) reads_class_fields: bool,
}

/// The generic declaration an instantiation expands.
#[derive(Clone, Copy)]
pub(crate) enum Declaration<'a> {
    Interface(&'a Interface),
    Alias(&'a TypeAlias),
    Class(crate::shapes::ClassShape<'a>),
}

fn validate(types: &TypeTable, ctx: &Context, ty: Ty, remaining: &mut usize) -> Result<Ty> {
    ensure_unsupported!(*remaining > 1, "generic type arguments expand indefinitely");
    *remaining -= 1;
    let kind = types.kind(ty);
    if let TyKind::TypeVar(name) = kind {
        return Err(diag!(
            codes::TYPE_MISMATCH,
            "unresolved type argument {}",
            name.get(ctx)
        ));
    }
    if !matches!(kind, TyKind::Variant(_)) {
        for child in types.children(kind) {
            validate(types, ctx, child, remaining)?;
        }
    }
    Ok(ty)
}

pub(crate) fn substitute(
    ctx: &Context,
    types: &mut TypeTable,
    source: &HirType,
    bindings: &[(TypeName, Ty)],
    remaining: &mut usize,
) -> Result<Ty> {
    let declared = types.intern_hir(ctx, source, true);
    let bound = types.substitute(declared, bindings);
    validate(types, ctx, bound, remaining)
}

impl<'a> Symbol<'a> {
    pub(crate) fn declaration(&self) -> Option<Declaration<'a>> {
        self.interface
            .map(Declaration::Interface)
            .or_else(|| self.class.map(Declaration::Class))
            .or_else(|| self.alias.map(Declaration::Alias))
    }
}

impl<'a> Declaration<'a> {
    pub(crate) fn parameters(self) -> &'a [perry_hir::types::TypeParam] {
        match self {
            Self::Interface(interface) => &interface.type_params,
            Self::Alias(alias) => &alias.type_params,
            Self::Class(class) => class.parameters(),
        }
    }

    fn bind(
        self,
        ctx: &Context,
        base: &str,
        types: &mut TypeTable,
        arguments: TypeListId,
    ) -> Result<Vec<(TypeName, Ty)>> {
        let parameters = self.parameters();
        ensure_diag!(
            types.ty_slice(arguments).len() <= parameters.len(),
            codes::TYPE_MISMATCH,
            "too many type arguments for {base}"
        );
        let mut bindings = Vec::with_capacity(parameters.len());
        let mut remaining = ARGUMENT_LIMIT;
        for (index, parameter) in parameters.iter().enumerate() {
            let argument = match types.ty_slice(arguments).get(index) {
                Some(&argument) => validate(types, ctx, argument, &mut remaining)?,
                None => substitute(
                    ctx,
                    types,
                    parameter
                        .default
                        .as_deref()
                        .or(parameter.constraint.as_deref())
                        .ok_or_else(|| {
                            diag!(
                                codes::MISSING_TYPE_ANNOTATION,
                                "missing type argument {} of {base}",
                                parameter.name
                            )
                        })?,
                    &bindings,
                    &mut remaining,
                )?,
            };
            bindings.push((TypeName::new(ctx, parameter.name.clone()), argument));
        }
        Ok(bindings)
    }
}
