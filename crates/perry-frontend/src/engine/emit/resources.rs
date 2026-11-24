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
        let host_call = |args| {
            external(
                &member.binding,
                args,
                member.raw_result.as_ref().unwrap_or(&member.result).clone(),
                start,
                end,
            )
        };
        if member.raw_result.is_none() && member.result_inputs.is_empty() {
            return Ok(host_call(inputs));
        }
        let fid = if let Some(fid) = self.resource_functions.get(id).copied().flatten() {
            fid
        } else {
            let fid = self.fresh_func_id()?;
            let mut params = Vec::with_capacity(member.params.len());
            let mut raw_inputs = Vec::with_capacity(member.params.len());
            for (i, ty) in member.params.iter().enumerate() {
                let id = self.fresh_synthetic_local()?;
                let tagged = member.result_inputs.iter().find(|input| input.index == i);
                let parameter = tagged.map_or(ty, |input| &input.tagged);
                params.push(synth_param(id, format!("arg{i}"), parameter.clone()));
                let Some(input) = tagged else {
                    raw_inputs.push(Expr::LocalGet(id));
                    continue;
                };
                let field = |name| Self::prop_get(Expr::LocalGet(id), name);
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
                raw_inputs.push(Expr::Conditional {
                    condition: Box::new(Expr::Compare {
                        op: CompareOp::Eq,
                        left: Box::new(field("tag")),
                        right: Box::new(Expr::String("ok".into())),
                    }),
                    then_expr: Box::new(case(true, input.has_ok)),
                    else_expr: Box::new(case(false, input.has_err)),
                });
            }
            let mut body = Vec::new();
            let init = host_call(raw_inputs);
            if let Some(raw) = &member.raw_result {
                let r = self.fresh_synthetic_local()?;

                let field = |name| Self::prop_get(Expr::LocalGet(r), name);
                let mut failure = Vec::new();
                let error = match &member.error {
                    Some(crate::wit::ResourceError::Tagged { ty, owned }) => {
                        let id = self.fresh_synthetic_local()?;
                        failure.push(local(
                            id,
                            "error",
                            ty.clone(),
                            crate::hir_compat::mark_non_null(field("error")),
                            false,
                        ));
                        let prop = |name| Self::prop_get(Expr::LocalGet(id), name);
                        for (tag, owner) in owned {
                            let drop = self
                                .resource_members
                                .iter()
                                .find(|m| m.owner == *owner && m.kind == MemberKind::Dispose)
                                .ok_or_else(|| {
                                    Diagnostic::internal(
                                        "owned WIT error has no resource destructor",
                                    )
                                })?;
                            failure.push(Stmt::If {
                                condition: Expr::Compare {
                                    op: CompareOp::Eq,
                                    left: Box::new(prop("tag")),
                                    right: Box::new(Expr::String(tag.clone())),
                                },
                                then_branch: vec![Stmt::Expr(external(
                                    &drop.binding,
                                    vec![prop("val")],
                                    HirType::Void,
                                    start,
                                    end,
                                ))],
                                else_branch: None,
                            });
                        }
                        Expr::ErrorNew(Some(Box::new(prop("tag"))))
                    }
                    Some(crate::wit::ResourceError::Value) => field("error"),
                    None => Expr::ErrorNew(None),
                };
                failure.push(Stmt::Throw(error));
                body.extend([
                    local(r, "result", raw.clone(), init, false),
                    Stmt::If {
                        condition: field("ok"),
                        then_branch: vec![Stmt::Return(
                            (member.result != HirType::Void).then(|| field("value")),
                        )],
                        else_branch: Some(failure),
                    },
                ]);
            } else if member.result == HirType::Void {
                body.push(Stmt::Expr(init));
            } else {
                body.push(Stmt::Return(Some(init)));
            }
            self.synth_functions.push(synth_function(
                fid,
                format!("__resource_call_{fid}"),
                params,
                member.result.clone(),
                body,
                false,
            ));
            self.resource_functions[id] = Some(fid);
            fid
        };
        Ok(synth_call(fid, inputs, start, end))
    }
}
