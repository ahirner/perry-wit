//! Optional, scalar, and discriminated union layouts shared by declarations and solver types.
use super::*;
use crate::engine::types::{LocId, Ty, TyKind};
use crate::shapes::generics::Resolver;

impl Shapes<'_> {
    fn literal<'a>(&self, ctx: &'a Context, ty: Ty) -> Option<&'a str> {
        match self.specialization.types.borrow().kind(ty) {
            TyKind::Literal(value) => Some(value.get(ctx).as_str()),
            _ => None,
        }
    }
    fn variant_properties(&self, ctx: &Context, ty: Ty) -> Option<Vec<(String, Ty)>> {
        let mut types = self.specialization.types.borrow_mut();
        let ty = match types.kind(ty) {
            TyKind::ClassInst(id) => types.named(ctx, &self.specialization.class_names[id]),
            _ => ty,
        };
        Some(
            Resolver::new(ctx, &self.decls, &mut types)
                .record(ty, 0)?
                .into_iter()
                .filter(|(_, ty, _)| {
                    !types.is_zero_sized_func(*ty, &self.specialization.capture_free)
                })
                .map(|(name, ty, _)| (name, ty))
                .collect(),
        )
    }
    pub(super) fn solved_union(
        &self,
        ctx: &Context,
        types: &[Ty],
        visiting: &mut Vec<LocId>,
    ) -> Result<TypeHandle> {
        let mut members = Resolver::new(
            ctx,
            &self.decls,
            &mut self.specialization.types.borrow_mut(),
        )
        .flatten(types, 0);
        let mut absence = None;
        members.retain(|ty| {
            let state = match *ty {
                Ty::VOID => perry_mir::Absence::Undefined,
                Ty::NULL => perry_mir::Absence::Null,
                _ => return true,
            };
            absence = perry_mir::Absence::join_optional(absence, Some(state));
            false
        });
        let inner = match (members.as_slice(), absence) {
            ([], Some(_)) => bail_diag!(
                codes::MISSING_TYPE_ANNOTATION,
                "an absence-only union needs a declared value type"
            ),
            ([one], Some(_)) => self.project_type(ctx, *one, visiting)?.carrier,
            (members, _) if members.iter().all(|ty| self.literal(ctx, *ty).is_some()) => {
                StringType::get(ctx).into()
            }
            (members, _) => self.union_layout(ctx, members, visiting)?,
        };
        Ok(absence.map_or(inner, |states| {
            OptionalType::get_with(ctx, inner, states).into()
        }))
    }

    fn union_layout(
        &self,
        ctx: &Context,
        members: &[Ty],
        visiting: &mut Vec<LocId>,
    ) -> Result<TypeHandle> {
        let types = self.specialization.types.borrow();
        let scalar = members.iter().any(|&member| {
            matches!(
                types.kind(member),
                TyKind::Number | TyKind::String | TyKind::Boolean
            ) || types
                .integer_domain(ctx, &self.decls, member)
                .is_some_and(|domain| domain.deref(ctx).width() <= 32)
        });
        drop(types);
        if scalar {
            let mut cases = Vec::with_capacity(members.len());
            for member in members {
                let ty = self.project_type(ctx, *member, visiting)?.carrier;
                ensure_diag!(
                    self.class_of_type(ctx, ty).is_none(),
                    codes::UNSUPPORTED_TYPE,
                    "finite value unions require plain record alternatives"
                );
                let tag = perry_mir::value_tag(ctx, ty).ok_or_else(|| Diagnostic::new(
                    codes::UNSUPPORTED_TYPE,
                    "finite value unions support numbers, strings, booleans, and one object alternative (a plain record or Uint8Array)",
                ))?;
                if let Some((_, payload)) = cases.iter().find(|(name, _)| name == tag) {
                    ensure_diag!(
                        *payload == Some(ty),
                        codes::UNSUPPORTED_TYPE,
                        "finite value unions require one shape per JavaScript type"
                    );
                } else {
                    cases.push((tag.to_owned(), Some(ty)));
                }
            }
            let variants = perry_mir::VariantType::get(ctx, cases).into();
            return Ok(perry_mir::ValueUnionType::get(ctx, variants).into());
        }
        // Distinct inference cells can settle on the same promise, array or dictionary type.
        if let Some((&first, rest)) = members.split_first()
            && members.iter().all(|&member| {
                matches!(
                    self.specialization.types.borrow().kind(member),
                    TyKind::Cell { .. } | TyKind::Array(_) | TyKind::Promise(_)
                )
            })
        {
            let carrier = self.project_type(ctx, first, visiting)?.carrier;
            let mut same = true;
            for &member in rest {
                same &= self.project_type(ctx, member, visiting)?.carrier == carrier;
            }
            if same {
                return Ok(carrier);
            }
        }
        let records = members
            .iter()
            .map(|&ty| {
                self.variant_properties(ctx, ty).ok_or_else(|| {
                    Diagnostic::new(
                        codes::UNSUPPORTED_TYPE,
                        "unions other than tagged object types are not supported yet",
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if !records.is_empty()
            && records.iter().all(|props| {
                props
                    .iter()
                    .any(|(name, prop_ty)| name == "ok" && *prop_ty == Ty::BOOLEAN)
            })
        {
            return self.result(ctx, &records, visiting);
        }
        self.variant(ctx, &records, visiting)
    }

    /// Build the record representation for an SDK `Result`, keeping disjoint payload slots that admit `undefined` when inactive.
    fn result(
        &self,
        ctx: &Context,
        alternatives: &[Vec<(String, Ty)>],
        visiting: &mut Vec<LocId>,
    ) -> Result<TypeHandle> {
        ensure_diag!(
            alternatives.len() == 2
                || (alternatives.len() == 1
                    && alternatives[0].len() == 1
                    && alternatives[0][0].0 == "ok"),
            codes::UNSUPPORTED_TYPE,
            "a tagged result needs exactly two alternatives"
        );
        let mut value_arg = None;
        let mut error_arg = None;
        for properties in alternatives {
            for (p_name, prop_ty) in properties {
                match p_name.as_str() {
                    "ok" => {}
                    "value" => {
                        ensure_diag!(
                            value_arg.replace(*prop_ty).is_none(),
                            codes::UNSUPPORTED_TYPE,
                            "result payload value appears in both alternatives"
                        );
                    }
                    "error" => {
                        ensure_diag!(
                            error_arg.replace(*prop_ty).is_none(),
                            codes::UNSUPPORTED_TYPE,
                            "result payload error appears in both alternatives"
                        );
                    }
                    _ => bail_diag!(
                        codes::UNSUPPORTED_TYPE,
                        "result alternatives only contain ok, value and error"
                    ),
                }
            }
        }
        let mut fields = vec![(
            "ok".to_string(),
            IntegerType::get(ctx, 1, Signedness::Signless).into(),
        )];
        for (name, arg) in [("value", value_arg), ("error", error_arg)] {
            if let Some(prop_ty) = arg {
                let ty = self.project_type(ctx, prop_ty, visiting)?.carrier;
                fields.push((name.to_string(), OptionalType::get(ctx, ty).into()));
            }
        }
        Ok(self.mir_record(ctx, fields)?.into())
    }

    fn variant(
        &self,
        ctx: &Context,
        members: &[Vec<(String, Ty)>],
        visiting: &mut Vec<LocId>,
    ) -> Result<TypeHandle> {
        let mut cases = Vec::with_capacity(members.len());
        for properties in members {
            let mut tag = None;
            let mut val_arg = None;
            let mut invalid = false;
            for (p_name, ty) in properties {
                match p_name.as_str() {
                    "tag" => {
                        let Some(lit) = self.literal(ctx, *ty) else {
                            bail_diag!(
                                codes::UNSUPPORTED_TYPE,
                                "union members need a literal `tag`"
                            );
                        };
                        tag = Some(lit.to_owned());
                    }
                    "val" => val_arg = Some(*ty),
                    _ => invalid = true,
                }
            }
            let tag = tag.ok_or_else(|| {
                Diagnostic::new(
                    codes::UNSUPPORTED_TYPE,
                    "union members need a literal `tag`",
                )
            })?;
            ensure_diag!(
                !invalid,
                codes::UNSUPPORTED_TYPE,
                "union members have `tag` and `val` only"
            );
            ensure_diag!(
                !cases.iter().any(|(name, _)| name == &tag),
                codes::TYPE_MISMATCH,
                "variant tags must be distinct"
            );
            let payload = val_arg
                .map(|prop_ty| {
                    self.project_type(ctx, prop_ty, visiting)
                        .map(|binding| binding.carrier)
                })
                .transpose()?;
            cases.push((tag, payload));
        }
        Ok(perry_mir::VariantType::get(ctx, cases).into())
    }
}
