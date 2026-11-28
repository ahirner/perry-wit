//! Optional, scalar, and discriminated union layouts shared by declarations and solver types.
use super::*;
use crate::engine::types::{LocId, Ty, TyKind, VariantCase};
use crate::shapes::generics::Resolver;

impl Shapes<'_, Preparing<'_>> {
    fn runtime_properties(&self, props: PropertiesId) -> impl Iterator<Item = (&str, Ty)> {
        let types = &self.types.source;
        types
            .object_slice(props)
            .iter()
            .filter(|p| !types.is_zero_sized_func(p.ty, &self.specialization.capture_free))
            .map(|p| (types.name_str(p.name), p.ty))
    }

    pub(super) fn solved_union(
        &mut self,
        ctx: &Context,
        types: &[Ty],
        visiting: &mut Vec<LocId>,
    ) -> Result<TypeHandle> {
        let mut members = Resolver::new(ctx, self.decls, self.types.source).flatten(types, 0);
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
            (members, _)
                if members
                    .iter()
                    .all(|ty| matches!(self.types.source.kind(*ty), TyKind::Literal(_))) =>
            {
                StringType::get(ctx).into()
            }
            (members, _) => self.union_layout(ctx, members, visiting)?,
        };
        Ok(absence.map_or(inner, |states| {
            OptionalType::get_with(ctx, inner, states).into()
        }))
    }

    fn union_layout(
        &mut self,
        ctx: &Context,
        members: &[Ty],
        visiting: &mut Vec<LocId>,
    ) -> Result<TypeHandle> {
        let types = &self.types.source;
        let scalar = members.iter().any(|&member| {
            matches!(
                types.kind(member),
                TyKind::Number | TyKind::String | TyKind::Boolean
            ) || types
                .integer_domain(ctx, self.decls, member)
                .is_some_and(|domain| domain.deref(ctx).width() <= 32)
        });
        if scalar {
            let mut cases = Vec::with_capacity(members.len());
            for member in members {
                let ty = self.project_view(ctx, *member, visiting, false)?;
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
                    self.types.source.kind(member),
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
                let ty = match self.types.source.kind(ty) {
                    TyKind::ClassInst(id) => self.specialization.bodies.classes[id].ty,
                    _ => ty,
                };
                Resolver::new(ctx, self.decls, self.types.source)
                    .record(ty, 0)
                    .ok_or_else(|| {
                        Diagnostic::new(
                            codes::UNSUPPORTED_TYPE,
                            "unions other than tagged object types are not supported yet",
                        )
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        if !records.is_empty()
            && records.iter().all(|&props| {
                self.runtime_properties(props)
                    .any(|(name, ty)| name == perry_mir::RESULT_TAG && ty == Ty::BOOLEAN)
            })
        {
            return self.result(ctx, &records, visiting);
        }
        let mut cases = indexmap::IndexMap::with_capacity(records.len());
        for props in records {
            let (tag, payload) = self.variant_case(props)?;
            ensure_diag!(
                cases.insert(tag, payload).is_none(),
                codes::TYPE_MISMATCH,
                "variant tags must be distinct"
            );
        }
        self.variant_layout(ctx, cases, visiting)
    }

    /// Build the record representation for an SDK `Result`, keeping disjoint payload slots that admit `undefined` when inactive.
    fn result(
        &mut self,
        ctx: &Context,
        alternatives: &[PropertiesId],
        visiting: &mut Vec<LocId>,
    ) -> Result<TypeHandle> {
        ensure_diag!(
            alternatives.len() == 2
                || matches!(alternatives, [props] if self.runtime_properties(*props).count() == 1),
            codes::UNSUPPORTED_TYPE,
            "a tagged result needs exactly two alternatives"
        );
        use perry_mir::{RESULT_PAYLOADS, RESULT_TAG};
        let mut payloads = [None; 2];
        for &props in alternatives {
            for (name, ty) in self.runtime_properties(props) {
                if name == RESULT_TAG {
                    continue;
                }
                let index = RESULT_PAYLOADS
                    .iter()
                    .position(|&field| field == name)
                    .ok_or_else(|| {
                        diag!(
                            codes::UNSUPPORTED_TYPE,
                            "result alternatives only contain ok, value and error"
                        )
                    })?;
                ensure_diag!(
                    payloads[index].replace(ty).is_none(),
                    codes::UNSUPPORTED_TYPE,
                    "result payload {name} appears in both alternatives"
                );
            }
        }
        let record = self.types.source.result_record(payloads);
        Ok(self.project_type(ctx, record, visiting)?.carrier)
    }

    fn variant_case(&self, props: PropertiesId) -> Result<VariantCase> {
        let mut tag = None;
        let mut payload = None;
        let mut extra = false;
        for (name, ty) in self.runtime_properties(props) {
            match name {
                "tag" => {
                    if let TyKind::Literal(name) = self.types.source.kind(ty) {
                        tag = Some(name);
                    }
                }
                "val" => payload = Some(ty),
                _ => extra = true,
            }
        }
        let tag = tag.ok_or_else(|| {
            diag!(
                codes::UNSUPPORTED_TYPE,
                "union members need a literal `tag`"
            )
        })?;
        ensure_diag!(
            !extra,
            codes::UNSUPPORTED_TYPE,
            "union members have `tag` and `val` only"
        );
        Ok((tag, payload))
    }

    pub(super) fn variant_layout(
        &mut self,
        ctx: &Context,
        cases: impl IntoIterator<Item = VariantCase>,
        visiting: &mut Vec<LocId>,
    ) -> Result<TypeHandle> {
        let cases = cases
            .into_iter()
            .map(|(tag, payload)| {
                let ty = payload
                    .map(|ty| self.project_type(ctx, ty, visiting).map(|b| b.carrier))
                    .transpose()?;
                Ok((tag.get(ctx).clone(), ty))
            })
            .collect::<Result<_>>()?;
        Ok(perry_mir::VariantType::get(ctx, cases).into())
    }
}
