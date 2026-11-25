//! Instantiation of generic interfaces, type aliases, and classes.
use super::*;
use crate::engine::types::{Ty, TyKind, TypeTable};
use perry_hir::ir::{Interface, TypeAlias};
use perry_wit_diagnostics::{codes, diag, ensure_diag, ensure_unsupported};
use pliron::utils::vec_exns::VecExtns;

mod resolution;
pub(crate) use resolution::{Properties, Query, Resolver, object_properties};

const RESOLUTION_DEPTH: usize = 32;
pub(crate) const ARGUMENT_LIMIT: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Utility {
    Partial,
    Required,
    Pick,
    Omit,
}

impl Utility {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
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
pub(crate) enum DeclarationId {
    Interface(usize),
    Alias(usize),
    Class(usize),
    Utility(Utility),
}

pub(crate) type InstanceKey = (DeclarationId, Vec<Ty>);

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
    match types.kind(ty) {
        TyKind::TypeVar(name) => {
            return Err(diag!(
                codes::TYPE_MISMATCH,
                "unresolved type argument {}",
                name.get(ctx)
            ));
        }
        TyKind::Signature { params, result, .. } => {
            for p in types.object_slice(params) {
                validate(types, ctx, p.ty, remaining)?;
            }
            validate(types, ctx, result, remaining)?;
        }
        TyKind::Promise(inner) | TyKind::Array(inner) => {
            validate(types, ctx, inner, remaining)?;
        }
        TyKind::Tuple(span) | TyKind::Union(span) | TyKind::Generic { args: span, .. } => {
            for &item in types.ty_slice(span) {
                validate(types, ctx, item, remaining)?;
            }
        }
        TyKind::Object { props, index } => {
            for prop in types.object_slice(props) {
                validate(types, ctx, prop.ty, remaining)?;
            }
            if let Some(value) = index {
                validate(types, ctx, value, remaining)?;
            }
        }
        _ => {}
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

impl<'a> Declaration<'a> {
    pub(crate) fn find(decls: &Declarations<'a>, base: &str) -> Option<(DeclarationId, Self)> {
        if let Some(&(index, interface)) = decls.interfaces.get(base) {
            return Some((DeclarationId::Interface(index), Self::Interface(interface)));
        }
        if let Some(&(index, class)) = decls.classes.named.get(base) {
            return Some((DeclarationId::Class(index), Self::Class(class)));
        }
        let &(index, alias) = decls.type_aliases.get(base)?;
        Some((DeclarationId::Alias(index), Self::Alias(alias)))
    }

    pub(crate) fn parameters(self) -> &'a [perry_hir::types::TypeParam] {
        match self {
            Self::Interface(interface) => &interface.type_params,
            Self::Alias(alias) => &alias.type_params,
            Self::Class(class) => class.parameters(),
        }
    }

    pub(crate) fn bind(
        self,
        ctx: &Context,
        base: &str,
        types: &mut TypeTable,
        arguments: &[Ty],
    ) -> Result<Vec<(TypeName, Ty)>> {
        let parameters = self.parameters();
        ensure_diag!(
            arguments.len() <= parameters.len(),
            codes::TYPE_MISMATCH,
            "too many type arguments for {base}"
        );
        let mut bindings = Vec::with_capacity(parameters.len());
        let mut remaining = ARGUMENT_LIMIT;
        for (index, parameter) in parameters.iter().enumerate() {
            let argument = match arguments.get(index) {
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
