//! Project specialized storage types into MIR carriers.
use super::*;
use crate::engine::types::{CellKind, Idx, LocId, Ty, TyKind};

impl Shapes<'_> {
    pub(in crate::lower) fn resolved_binding(&self, ctx: &Context, ty: Ty) -> Result<BindingType> {
        if let Some(&binding) = self.resolved.borrow().get(&ty) {
            return Ok(binding);
        }
        let binding = self.project_type(ctx, ty, &mut Vec::new())?;
        self.resolved.borrow_mut().insert(ty, binding);
        Ok(binding)
    }

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
        let types = self.specialization.types.borrow();
        let TyKind::Union(span) = types.kind(ty) else {
            return false;
        };
        types
            .ty_slice(span)
            .iter()
            .filter(|&&m| !matches!(m, Ty::NULL | Ty::VOID))
            .count()
            >= 2
            && encodable(self.specialization, &types, ty, &mut Vec::new())
    }

    pub(in crate::lower) fn closure_result(
        &self,
        ctx: &Context,
        id: perry_hir::types::FuncId,
        source: &HirType,
    ) -> Result<Option<TypeHandle>> {
        if let Some(ty) = self
            .specialization
            .instances
            .closures
            .get(&id)
            .and_then(|s| s.result)
        {
            return Ok(self.solved_result(ctx, ty, false)?.map(|b| b.carrier));
        }
        (source != &HirType::Void)
            .then(|| self.value_type(ctx, source))
            .transpose()
    }

    pub(in crate::lower) fn closure_param(
        &self,
        ctx: &Context,
        id: perry_hir::types::FuncId,
        index: usize,
        param: &perry_hir::ir::Param,
    ) -> Result<(BindingType, bool)> {
        self.param_type(
            ctx,
            self.specialization
                .instances
                .closures
                .get(&id)
                .and_then(|s| s.params.get(index).copied().flatten()),
            &param.ty,
        )
    }

    pub(in crate::lower) fn parameter_binding(
        &self,
        ctx: &Context,
        function: &perry_hir::ir::Function,
        index: usize,
    ) -> Result<(BindingType, bool)> {
        self.param_type(
            ctx,
            self.specialization
                .instances
                .signatures
                .get(&function.id)
                .and_then(|s| s.params[index]),
            &function.params[index].ty,
        )
    }

    fn param_type(
        &self,
        ctx: &Context,
        solved: Option<Ty>,
        ty: &HirType,
    ) -> Result<(BindingType, bool)> {
        if let Some(ty) = solved {
            return Ok((self.resolved_binding(ctx, ty)?, ty == Ty::NEVER));
        }
        Ok((
            BindingType {
                carrier: self.value_type(ctx, ty)?,
                integer: self.integer_type(ctx, ty),
            },
            matches!(ty, HirType::Never),
        ))
    }

    pub(in crate::lower) fn function_result(
        &self,
        ctx: &Context,
        function: &perry_hir::ir::Function,
    ) -> Result<(Option<BindingType>, bool)> {
        if let Some(ty) = self
            .specialization
            .instances
            .signatures
            .get(&function.id)
            .and_then(|signature| signature.result)
        {
            return Ok((
                self.solved_result(ctx, ty, function.is_async)?,
                ty == Ty::NEVER,
            ));
        }
        let ty = &function.return_type;
        let result = match (ty, function.is_async) {
            (HirType::Void, false) => None,
            (HirType::Promise(inner), true) => {
                Some(BindingType::plain(self.settled_type(ctx, inner)?))
            }
            (ty, true) => Some(BindingType::plain(self.settled_type(ctx, ty)?)),
            (ty, false) => Some(BindingType {
                carrier: self.value_type(ctx, ty)?,
                integer: self.integer_type(ctx, ty),
            }),
        };
        Ok((result, matches!(ty, HirType::Never)))
    }

    pub(in crate::lower) fn solved_result(
        &self,
        ctx: &Context,
        ty: Ty,
        is_async: bool,
    ) -> Result<Option<BindingType>> {
        if !is_async {
            return (ty != Ty::VOID)
                .then(|| self.resolved_binding(ctx, ty))
                .transpose();
        }
        let inner = match self.specialization.types.borrow().kind(ty) {
            TyKind::Promise(inner) => inner,
            TyKind::Cell {
                loc,
                kind: CellKind::Promise,
            } => self.specialization.locations[loc],
            _ => ty,
        };
        let settled = self
            .resolved_binding(ctx, if inner == Ty::BOTTOM { Ty::VOID } else { inner })?
            .carrier;
        Ok(Some(BindingType::plain(settled)))
    }

    pub(super) fn project_type(
        &self,
        ctx: &Context,
        ty: Ty,
        visiting: &mut Vec<LocId>,
    ) -> Result<BindingType> {
        let facts = self.specialization;
        let types = &facts.types;
        let slot = |ty, visiting: &mut Vec<LocId>| {
            if types.borrow().is_zero_sized_func(ty, &facts.capture_free) {
                Ok(BindingType::plain(FP64Type::get(ctx).into()))
            } else {
                self.project_type(ctx, ty, visiting)
            }
        };
        let promise = |inner, visiting: &mut Vec<LocId>| -> Result<TypeHandle> {
            let inner = if inner == Ty::BOTTOM { Ty::VOID } else { inner };
            Ok(PromiseType::get(ctx, self.project_type(ctx, inner, visiting)?.carrier).into())
        };
        let dict = |inner, visiting: &mut Vec<LocId>| -> Result<TypeHandle> {
            let value = self.project_type(ctx, inner, visiting)?.carrier;
            let entry = self.dict_tuple_type(ctx, StringType::get(ctx).into(), value)?;
            Ok(ArrayType::get(ctx, entry.into()).into())
        };
        let kind = types.borrow().kind(ty);
        let carrier = match kind {
            TyKind::Open | TyKind::Bottom => return Err(missing_type_annotation()),
            TyKind::Void | TyKind::Never => crate::lower::coerce::void_type(ctx),
            TyKind::Null => {
                OptionalType::get_with(ctx, FP64Type::get(ctx).into(), perry_mir::Absence::Null)
                    .into()
            }
            TyKind::Number | TyKind::Int32 => FP64Type::get(ctx).into(),
            TyKind::BigInt => perry_mir::BigIntType::get(ctx).into(),
            TyKind::String => StringType::get(ctx).into(),
            TyKind::Boolean => IntegerType::get(ctx, 1, Signedness::Signless).into(),
            TyKind::Dynamic => perry_mir::JsonType::get(ctx).into(),
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
            TyKind::ClassInst(id) => self.named(ctx, &facts.class_names[id])?,
            TyKind::Array(elem) => ArrayType::get(ctx, slot(elem, visiting)?.carrier).into(),
            TyKind::Promise(inner) => promise(inner, visiting)?,
            TyKind::Tuple(span) => {
                let elements = types.borrow().ty_slice(span).to_vec();
                let fields = elements
                    .iter()
                    .enumerate()
                    .map(|(i, &elem)| {
                        Ok((format!("_{i}"), slot(elem, visiting)?.expected_type(ctx)))
                    })
                    .collect::<Result<_>>()?;
                self.mir_tuple(ctx, fields)?.into()
            }
            TyKind::Object { props, index } => {
                let fields: Vec<_> = {
                    let types = types.borrow();
                    types
                        .object_slice(props)
                        .iter()
                        .filter(|p| !types.is_zero_sized_func(p.ty, &facts.capture_free))
                        .map(|p| (types.name_str(p.name).to_owned(), p.ty, p.optional))
                        .collect()
                };
                if let Some(index) = index {
                    ensure_diag!(
                        fields.is_empty(),
                        codes::UNSUPPORTED_TYPE,
                        "index signatures are outside the closed-record profile when combined with fixed properties"
                    );
                    dict(index, visiting)?
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
                        ArrayType::get(ctx, slot(elem, visiting)?.carrier).into()
                    }
                    CellKind::Promise => promise(elem, visiting)?,
                    CellKind::Dict
                        if types
                            .borrow()
                            .is_valid_dict_value(elem, facts.dict_reads.contains(loc.index())) =>
                    {
                        dict(elem, visiting)?
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
                let args = types.borrow().ty_slice(args).to_vec();
                if let Some(carrier) = self.builtin_generic(ctx, name, &args, |&arg| {
                    self.project_type(ctx, arg, visiting).map(|b| b.carrier)
                })? {
                    carrier
                } else if name == "ReadableStream"
                    && !self.is_user_type(name)
                    && let [arg] = args.as_slice()
                    && let TyKind::Named(id) = types.borrow().kind(*arg)
                    && id.get(ctx) == "Uint8Array"
                {
                    perry_mir::ReadableType::get(ctx).into()
                } else {
                    self.generic(ctx, name, &args)?
                }
            }
            TyKind::Union(span) => {
                let members = types.borrow().ty_slice(span).to_vec();
                self.solved_union(ctx, &members, visiting)?
            }
            TyKind::Func(_) => {
                perry_wit_diagnostics::bail_internal!("missing function value layout")
            }
        };
        Ok(BindingType::plain(carrier))
    }
}
