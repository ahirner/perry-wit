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
        let mut inputs = Vec::with_capacity(args.len() + usize::from(receiver.is_some()));
        if let Some(receiver) = receiver {
            inputs.push(self.rewrite_expr(rctx, receiver)?);
        }
        for arg in args {
            inputs.push(self.rewrite_expr(rctx, arg)?);
        }
        let fid = match self.resource_functions[id] {
            Some(fid) => fid,
            None => {
                let fid = self.fresh_func_id()?;
                self.plans
                    .operations
                    .insert(fid, crate::engine::NativeOperation::Resource(id));
                self.resource_functions[id] = Some(fid);
                fid
            }
        };
        Ok(synth_call(fid, inputs, start, end))
    }
}
