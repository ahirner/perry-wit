//! Project specialized storage types into MIR carriers.
use super::*;
use crate::engine::types::{CellKind, Idx, LocId, Ty, TyKind};

impl Shapes<'_, Preparing<'_>> {
    pub(in crate::lower) fn resolved_binding(
        &mut self,
        ctx: &Context,
        ty: Ty,
    ) -> Result<BindingType> {
        let view = usize::from(self.types.wit_input);
        if let Some(&Some(binding)) = self.types.resolved[view].get(ty.index()) {
            return Ok(binding);
        }
        let binding = self.project_type(ctx, ty, &mut Vec::new())?;
        let column = &mut self.types.resolved[view];
        if column.len() <= ty.index() {
            column.resize(ty.index() + 1, None);
        }
        column[ty.index()] = Some(binding);
        Ok(binding)
    }
}

impl Shapes<'_> {
    pub(in crate::lower) fn native_param(
        &self,
        param: crate::engine::NativeParam,
    ) -> Result<(BindingType, bool)> {
        Ok(match param {
            crate::engine::NativeParam::Value(ty)
            | crate::engine::NativeParam::Argument { ty, .. } => {
                (self.resolved_binding(ty)?, ty == Ty::NEVER)
            }
            crate::engine::NativeParam::Environment(id) => (
                BindingType::plain(self.environment(id)?.record.into()),
                false,
            ),
        })
    }

    pub(in crate::lower) fn function_result(
        &self,
        function: &crate::source::InstanceFunction<'_>,
    ) -> Result<(Option<BindingType>, bool)> {
        let declaration = &self.specialization.templates[function.template];
        let ty = match function.result {
            crate::engine::InstanceResult::Value(ty) => ty,
            crate::engine::InstanceResult::ErasedFunction => Ty::VOID,
            crate::engine::InstanceResult::Source => declaration.result,
        };
        Ok((
            self.solved_result(ty, declaration.is_async)?,
            ty == Ty::NEVER,
        ))
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
            Ok(PromiseType::get(ctx, this.project_view(ctx, inner, visiting, false)?).into())
        };
        let dict = |this: &mut Self, inner, visiting: &mut Vec<LocId>| -> Result<TypeHandle> {
            let value = this.project_type(ctx, inner, visiting)?.carrier;
            let entry = this.positional_record(
                ctx,
                RecordKind::DictionaryEntry,
                [StringType::get(ctx).into(), value],
            )?;
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
            TyKind::Integer(domain) => return Ok(BindingType::integer(ctx, domain)),
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
            TyKind::Regex(_) => perry_mir::RegexType::get(ctx).into(),
            TyKind::WitCarrier(carrier) => carrier,
            TyKind::Variant(span) => {
                let cases = self.types.source.variant_cases(span).to_vec();
                self.variant_layout(ctx, cases, visiting)?
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
                if let Some(domain) = self.types.source.integer_domain(ctx, self.decls, ty) {
                    return Ok(BindingType::integer(ctx, domain));
                }
                self.named(ctx, id.get(ctx))?
            }
            TyKind::ClassInst(id) => self.named(ctx, &facts.bodies.classes[id].name)?,
            TyKind::Array(elem) => ArrayType::get(ctx, slot(self, elem, visiting)?.carrier).into(),
            TyKind::Promise(inner) => promise(self, inner, visiting)?,
            TyKind::Tuple(span) => {
                let elements = self.types.source.ty_slice(span).to_vec();
                let fields = elements
                    .into_iter()
                    .map(|elem| Ok(slot(self, elem, visiting)?.expected_type(ctx)))
                    .collect::<Result<Vec<_>>>()?;
                self.positional_record(ctx, RecordKind::Tuple, fields)?
                    .into()
            }
            TyKind::Object { props, index } => {
                let mut fields: Vec<_> = {
                    let types = &self.types.source;
                    types
                        .object_slice(props)
                        .iter()
                        .filter(|p| !types.is_zero_sized_func(p.ty, &facts.capture_free))
                        .copied()
                        .collect()
                };
                if self.types.source.is_open(ty) {
                    let types = &self.types.source;
                    fields.sort_unstable_by_key(|p| types.name_str(p.name));
                }
                if let Some(index) = index {
                    ensure_diag!(
                        fields.is_empty(),
                        codes::UNSUPPORTED_TYPE,
                        "index signatures are outside the closed-record profile when combined with fixed properties"
                    );
                    dict(self, index, visiting)?
                } else {
                    let fields = self.record_fields(ctx, &fields, visiting)?;
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
        let carrier = if self.types.wit_input && carrier.deref(ctx).is::<perry_mir::BytesType>() {
            perry_mir::ByteInputType::get(ctx).into()
        } else {
            carrier
        };
        Ok(BindingType::plain(carrier))
    }

    pub(super) fn project_view(
        &mut self,
        ctx: &Context,
        ty: Ty,
        visiting: &mut Vec<LocId>,
        wit_input: bool,
    ) -> Result<TypeHandle> {
        let previous = std::mem::replace(&mut self.types.wit_input, wit_input);
        let binding = self.project_type(ctx, ty, visiting);
        self.types.wit_input = previous;
        binding.map(|binding| binding.carrier)
    }
}
