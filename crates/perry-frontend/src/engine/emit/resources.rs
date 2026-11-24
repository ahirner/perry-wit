use super::*;
use crate::wit::MemberKind;

impl<'h> Emitter<'h, '_> {
    pub(super) fn resource_call(
        &mut self,
        rctx: &RewriteCtx<'_>,
        expr: &'h Expr,
        id: usize,
    ) -> Result<Expr> {
        let member = self
            .resource_members
            .get(id)
            .ok_or_else(|| Diagnostic::internal("unknown resource operation"))?;
        let (args, receiver, start, end) = match expr {
            Expr::Call {
                callee,
                args,
                byte_offset,
                byte_end,
                ..
            } => {
                let receiver = match &**callee {
                    Expr::PropertyGet { object, .. } if member.kind == MemberKind::Instance => {
                        Some(&**object)
                    }
                    Expr::IndexGet { object, .. } if member.kind == MemberKind::Dispose => {
                        Some(&**object)
                    }
                    _ => None,
                };
                (args, receiver, *byte_offset, *byte_end)
            }
            Expr::New {
                args, byte_offset, ..
            } => (args, None, *byte_offset, 0),
            Expr::StaticMethodCall { args, .. } => (args, None, 0, 0),
            _ => return Err(Diagnostic::internal("invalid resource operation")),
        };
        let mut inputs = Vec::with_capacity(member.params.len());
        if let Some(receiver) = receiver {
            inputs.push(self.rewrite_expr(rctx, receiver)?);
        }
        for arg in args {
            let parameter = member.params.get(inputs.len());
            let value = self.rewrite_expr(rctx, arg)?;
            inputs.push(if matches!(value,Expr::Undefined) && matches!(parameter,Some(HirType::Union(parts)) if parts.contains(&HirType::Null)) {
                Expr::Null
            } else {value});
        }
        let host_call = |args| Expr::Call {
            callee: Box::new(Expr::ExternFuncRef {
                name: member.binding.clone(),
                param_types: Vec::new(),
                return_type: member.raw_result.as_ref().unwrap_or(&member.result).clone(),
            }),
            args,
            type_args: Vec::new(),
            byte_offset: start,
            byte_end: end,
        };
        if member.raw_result.is_none() && member.result_inputs.is_empty() {
            return Ok(host_call(inputs));
        }
        let fid = if let Some(fid) = self.resource_functions.get(id).copied().flatten() {
            fid
        } else {
            let fid = self.fresh_func_id()?;
            let params: Vec<_> = member
                .params
                .iter()
                .enumerate()
                .map(|(i, ty)| {
                    Ok(crate::hir_compat::resource_classes::synth_param(
                        self.fresh_synthetic_local()?,
                        format!("arg{i}"),
                        member
                            .result_inputs
                            .iter()
                            .find(|r| r.index == i)
                            .map_or(ty, |r| &r.tagged)
                            .clone(),
                    ))
                })
                .collect::<Result<_>>()?;
            let mut body = Vec::new();
            let mut raw_inputs: Vec<_> = params.iter().map(|p| Expr::LocalGet(p.id)).collect();
            for input in &member.result_inputs {
                let p = params
                    .get(input.index)
                    .ok_or_else(|| Diagnostic::internal("missing tagged result parameter"))?;
                let field = |name| Self::prop_get(Expr::LocalGet(p.id), name);
                let case = |ok, payload| {
                    let mut fields = vec![("ok".into(), Expr::Bool(ok))];
                    if payload {
                        fields.push((
                            if ok { "value".into() } else { "error".into() },
                            field("val"),
                        ));
                    }
                    Expr::Object(fields)
                };
                let result = self.fresh_synthetic_local()?;
                body.push(Stmt::Let {
                    id: result,
                    name: "result".into(),
                    ty: member
                        .params
                        .get(input.index)
                        .ok_or_else(|| Diagnostic::internal("missing canonical result type"))?
                        .clone(),
                    mutable: true,
                    init: None,
                });
                body.push(Stmt::If {
                    condition: Expr::Compare {
                        op: CompareOp::Eq,
                        left: Box::new(field("tag")),
                        right: Box::new(Expr::String("ok".into())),
                    },
                    then_branch: vec![Stmt::Expr(Expr::LocalSet(
                        result,
                        Box::new(case(true, input.has_ok)),
                    ))],
                    else_branch: Some(vec![Stmt::Expr(Expr::LocalSet(
                        result,
                        Box::new(case(false, input.has_err)),
                    ))]),
                });
                *raw_inputs
                    .get_mut(input.index)
                    .ok_or_else(|| Diagnostic::internal("missing raw argument"))? =
                    Expr::LocalGet(result);
            }
            let init = host_call(raw_inputs);
            if let Some(raw) = &member.raw_result {
                let r = self.fresh_synthetic_local()?;

                let field = |name| Self::prop_get(Expr::LocalGet(r), name);
                let error = match member.error {
                    Some(crate::wit::ResourceError::Tagged) => Expr::ErrorNew(Some(Box::new(
                        Self::prop_get(crate::hir_compat::mark_non_null(field("error")), "tag"),
                    ))),
                    Some(crate::wit::ResourceError::Value) => field("error"),
                    None => Expr::ErrorNew(None),
                };
                body.extend([
                    Stmt::Let {
                        id: r,
                        name: "result".into(),
                        ty: raw.clone(),
                        mutable: false,
                        init: Some(init),
                    },
                    Stmt::If {
                        condition: field("ok"),
                        then_branch: vec![Stmt::Return(
                            (member.result != HirType::Void).then(|| field("value")),
                        )],
                        else_branch: Some(vec![Stmt::Throw(error)]),
                    },
                ]);
            } else if member.result == HirType::Void {
                body.push(Stmt::Expr(init));
            } else {
                body.push(Stmt::Return(Some(init)));
            }
            self.synth_functions
                .push(crate::hir_compat::resource_classes::synth_function(
                    fid,
                    format!("__resource_call_{fid}"),
                    params,
                    member.result.clone(),
                    body,
                    false,
                ));
            *self
                .resource_functions
                .get_mut(id)
                .ok_or_else(|| Diagnostic::internal("missing resource function slot"))? = Some(fid);
            fid
        };
        Ok(crate::hir_compat::resource_classes::synth_call(
            fid, inputs, start, end,
        ))
    }
}
