//! Project specialized storage types into MIR carriers.
use super::*;
use crate::engine::types::{CellKind, Idx, LocId, Ty, TyKind};

impl Shapes<'_, Preparing<'_>> {
    pub(in crate::lower) fn resolved_binding(
        &mut self,
        ctx: &Context,
        ty: Ty,
    ) -> Result<BindingType> {
        if let Some(&binding) = self.types.resolved.get(&ty) {
            return Ok(binding);
        }
        let binding = self.project_type(ctx, ty, &mut Vec::new())?;
        self.types.resolved.insert(ty, binding);
        Ok(binding)
    }
}

impl Shapes<'_> {
    pub(in crate::lower) fn is_json_encodable_union(&self, ty: Ty) -> bool {
        fn encodable(
            facts: &crate::engine::Specialization,
            types: &crate::engine::types::TypeTable,
            ty: Ty,
            visiting: &mut Vec<LocId>,
        ) -> bool {
            match types.kind(ty) {
                TyKind::Null
                | TyKind::Void
                | TyKind::Boolean
                | TyKind::Number
                | TyKind::Int32
                | TyKind::String
                | TyKind::Dynamic
                | TyKind::Literal(_) => true,
                TyKind::Array(elem) => encodable(facts, types, elem, visiting),
                TyKind::Cell { kind, loc }
                    if kind != CellKind::Promise && !visiting.contains(&loc) =>
                {
                    let elem = facts.locations[loc];
                    visiting.push(loc);
                    let yes = (kind != CellKind::Dict
                        || types.is_valid_dict_value(elem, facts.dict_reads.contains(loc.index())))
                        && encodable(facts, types, elem, visiting);
                    visiting.pop();
                    yes
                }
                TyKind::Tuple(span) | TyKind::Union(span) => types
                    .ty_slice(span)
                    .iter()
                    .all(|&member| encodable(facts, types, member, visiting)),
                TyKind::Object { props, index } => {
                    let fields = types.object_slice(props);
                    (!fields.is_empty() || index.is_some())
                        && fields
                            .iter()
                            .all(|p| encodable(facts, types, p.ty, visiting))
                        && index.is_none_or(|v| encodable(facts, types, v, visiting))
                }
                _ => false,
            }
        }
        let types = &self.types.source;
        let TyKind::Union(span) = types.kind(ty) else {
            return false;
        };
        types
            .ty_slice(span)
            .iter()
            .filter(|&&m| !matches!(m, Ty::NULL | Ty::VOID))
            .count()
            >= 2
            && encodable(self.specialization, types, ty, &mut Vec::new())
    }

    pub(in crate::lower) fn source_closure_result(
        &self,
        source: &HirType,
    ) -> Result<Option<TypeHandle>> {
        (source != &HirType::Void)
            .then(|| self.source_binding(source).map(|binding| binding.carrier))
            .transpose()
    }

    pub(in crate::lower) fn param_type(
        &self,
        solved: Option<Ty>,
        ty: &HirType,
    ) -> Result<(BindingType, bool)> {
        let ty = match solved {
            Some(ty) => ty,
            None => self.specialization.annotations.get(ty)?,
        };
        self.native_param(crate::engine::NativeParam::Value(ty))
    }

    pub(in crate::lower) fn native_param(
        &self,
        param: crate::engine::NativeParam,
    ) -> Result<(BindingType, bool)> {
        Ok(match param {
            crate::engine::NativeParam::Value(ty) => (self.resolved_binding(ty)?, ty == Ty::NEVER),
            crate::engine::NativeParam::Environment(id) => (
                BindingType::plain(self.environment_record(id)?.into()),
                false,
            ),
        })
    }

    pub(in crate::lower) fn function_result(
        &self,
        function: &crate::source::InstanceFunction<'_>,
    ) -> Result<(Option<BindingType>, bool)> {
        let ty = match function.signature.as_ref().map(|sig| sig.result) {
            Some(crate::engine::InstanceResult::Value(ty)) => ty,
            Some(crate::engine::InstanceResult::ErasedFunction) => Ty::VOID,
            _ => self.specialization.annotations.get(function.return_type)?,
        };
        Ok((self.solved_result(ty, function.is_async)?, ty == Ty::NEVER))
    }

    pub(in crate::lower) fn solved_result(
        &self,
        ty: Ty,
        is_async: bool,
    ) -> Result<Option<BindingType>> {
        if !is_async {
            return (ty != Ty::VOID)
                .then(|| self.resolved_binding(ty))
                .transpose();
        }
        let inner = match self.types.source.kind(ty) {
            TyKind::Promise(inner) => inner,
            TyKind::Cell {
                loc,
                kind: CellKind::Promise,
            } => self.specialization.locations[loc],
            _ => ty,
        };
        let settled = self
            .resolved_binding(if inner == Ty::BOTTOM { Ty::VOID } else { inner })?
            .carrier;
        Ok(Some(BindingType::plain(settled)))
    }
}

impl Shapes<'_, Preparing<'_>> {
    pub(super) fn project_type(
        &mut self,
        ctx: &Context,
        ty: Ty,
        visiting: &mut Vec<LocId>,
    ) -> Result<BindingType> {
        let facts = self.specialization;
        let slot = |this: &mut Self, ty, visiting: &mut Vec<LocId>| {
            if this
                .types
                .source
                .is_zero_sized_func(ty, &facts.capture_free)
            {
                Ok(BindingType::plain(FP64Type::get(ctx).into()))
            } else {
                this.project_type(ctx, ty, visiting)
            }
        };
        let promise = |this: &mut Self, inner, visiting: &mut Vec<LocId>| -> Result<TypeHandle> {
            let inner = if inner == Ty::BOTTOM { Ty::VOID } else { inner };
            Ok(PromiseType::get(ctx, this.project_type(ctx, inner, visiting)?.carrier).into())
        };
        let dict = |this: &mut Self, inner, visiting: &mut Vec<LocId>| -> Result<TypeHandle> {
            let value = this.project_type(ctx, inner, visiting)?.carrier;
            let entry = this.dict_tuple_type(ctx, StringType::get(ctx).into(), value)?;
            Ok(ArrayType::get(ctx, entry.into()).into())
        };
        let kind = self.types.source.kind(ty);
        let carrier = match kind {
            TyKind::Open | TyKind::Bottom => return Err(missing_type_annotation()),
            TyKind::Void | TyKind::Never => crate::lower::coerce::void_type(ctx),
            TyKind::Null => {
                OptionalType::get_with(ctx, FP64Type::get(ctx).into(), perry_mir::Absence::Null)
                    .into()
            }
            TyKind::Number | TyKind::Int32 => FP64Type::get(ctx).into(),
            TyKind::Integer(domain) => {
                return Ok(BindingType {
                    carrier: integer_mir_carrier(ctx, domain.deref(ctx).width()),
                    integer: Some(domain),
                });
            }
            TyKind::BigInt => perry_mir::BigIntType::get(ctx).into(),
            TyKind::String => StringType::get(ctx).into(),
            TyKind::Boolean => IntegerType::get(ctx, 1, Signedness::Signless).into(),
            TyKind::Dynamic => perry_mir::JsonType::get(ctx).into(),
            TyKind::Timer => {
                if let Some(timer) = self.types.timer {
                    timer
                } else {
                    let fields = vec![
                        (
                            "cancelled".into(),
                            IntegerType::get(ctx, 1, Signedness::Signless).into(),
                        ),
                        ("ctrl".into(), perry_mir::AbortType::get(ctx).into()),
                    ];
                    let timer = self
                        .records
                        .fresh(ctx, RecordKind::Object, Some(fields))
                        .to_diag(ctx)?
                        .into();
                    self.types.timer = Some(timer);
                    timer
                }
            }
            TyKind::Symbol => {
                bail_diag!(codes::UNSUPPORTED_TYPE, "type Symbol is not supported yet")
            }
            TyKind::Literal(_) => StringType::get(ctx).into(),
            TyKind::Signature { .. } => bail_diag!(
                codes::UNSUPPORTED_TYPE,
                "unresolved function value type reached lowering"
            ),
            TyKind::TypeVar(id) => bail_diag!(
                codes::UNSUPPORTED_TYPE,
                "type TypeVar({:?}) is not supported yet",
                id.get(ctx)
            ),
            TyKind::Named(id) => {
                let name = id.get(ctx);
                return Ok(BindingType {
                    carrier: self.named(ctx, name)?,
                    integer: self
                        .decls
                        .integer_spec(name)
                        .map(|(w, s)| IntegerType::get(ctx, w, s)),
                });
            }
            TyKind::ClassInst(id) => self.named(ctx, &facts.bodies.classes[id].name)?,
            TyKind::Array(elem) => ArrayType::get(ctx, slot(self, elem, visiting)?.carrier).into(),
            TyKind::Promise(inner) => promise(self, inner, visiting)?,
            TyKind::Tuple(span) => {
                let elements = self.types.source.ty_slice(span).to_vec();
                let fields = elements
                    .iter()
                    .enumerate()
                    .map(|(i, &elem)| {
                        Ok((
                            format!("_{i}"),
                            slot(self, elem, visiting)?.expected_type(ctx),
                        ))
                    })
                    .collect::<Result<_>>()?;
                self.mir_tuple(ctx, fields)?.into()
            }
            TyKind::Object { props, index } => {
                let mut fields: Vec<_> = {
                    let types = &self.types.source;
                    types
                        .object_slice(props)
                        .iter()
                        .filter(|p| !types.is_zero_sized_func(p.ty, &facts.capture_free))
                        .map(|p| (types.name_str(p.name).to_owned(), p.ty, p.optional))
                        .collect()
                };
                if self.types.source.is_open(ty) {
                    fields.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                }
                if let Some(index) = index {
                    ensure_diag!(
                        fields.is_empty(),
                        codes::UNSUPPORTED_TYPE,
                        "index signatures are outside the closed-record profile when combined with fixed properties"
                    );
                    dict(self, index, visiting)?
                } else {
                    let fields = fields
                        .into_iter()
                        .map(|(name, ty, optional)| {
                            let field = self
                                .project_type(ctx, ty, visiting)
                                .map_err(|e| e.with_note(format!("in field '{name}'")))?;
                            let carrier = if optional {
                                OptionalType::get(ctx, field.carrier).into()
                            } else {
                                field.expected_type(ctx)
                            };
                            Ok((name, carrier))
                        })
                        .collect::<Result<_>>()?;
                    self.mir_record(ctx, fields)?.into()
                }
            }
            TyKind::Cell { loc, kind } => {
                if visiting.contains(&loc) {
                    return match kind {
                        CellKind::Dict => {
                            Ok(BindingType::plain(self.mir_record(ctx, Vec::new())?.into()))
                        }
                        _ => Err(missing_type_annotation()),
                    };
                }
                visiting.push(loc);
                let elem = facts.locations[loc];
                let carrier = match kind {
                    CellKind::Array | CellKind::LocalArray => {
                        ArrayType::get(ctx, slot(self, elem, visiting)?.carrier).into()
                    }
                    CellKind::Promise => promise(self, elem, visiting)?,
                    CellKind::Dict
                        if self
                            .types
                            .source
                            .is_valid_dict_value(elem, facts.dict_reads.contains(loc.index())) =>
                    {
                        dict(self, elem, visiting)?
                    }
                    CellKind::Dict => self.mir_record(ctx, Vec::new())?.into(),
                };
                visiting.pop();
                carrier
            }
            TyKind::Func(_) | TyKind::Union(_)
                if let Some(callable) = self.callable_type(ctx, ty)? =>
            {
                callable
            }
            TyKind::Generic { base, args } => {
                let name = base.get(ctx);
                let args = self.types.source.ty_slice(args).to_vec();
                if let Some(carrier) = self.builtin_generic(ctx, name, &args, visiting)? {
                    carrier
                } else if name == "ReadableStream"
                    && !self.is_user_type(name)
                    && let [arg] = args.as_slice()
                    && let TyKind::Named(id) = self.types.source.kind(*arg)
                    && id.get(ctx) == "Uint8Array"
                {
                    perry_mir::ReadableType::get(ctx).into()
                } else {
                    self.generic(ctx, name, &args)?
                }
            }
            TyKind::Union(span) => {
                let members = self.types.source.ty_slice(span).to_vec();
                self.solved_union(ctx, &members, visiting)?
            }
            TyKind::Func(_) => {
                perry_wit_diagnostics::bail_internal!("missing function value layout")
            }
        };
        Ok(BindingType::plain(carrier))
    }
}
