use super::*;

pub(super) fn strip_body(body: &mut Vec<Stmt>, params: &[perry_hir::ir::Param]) {
    let mut drop_count = 0usize;
    while let Some(Stmt::If {
        condition:
            Expr::Compare {
                op: CompareOp::Eq,
                left,
                right,
            },
        then_branch,
        else_branch: None,
    }) = body.get(drop_count)
    {
        let param_id = match (&**left, &**right) {
            (Expr::LocalGet(id), Expr::Undefined) | (Expr::Undefined, Expr::LocalGet(id)) => {
                Some(*id)
            }
            _ => None,
        };
        if let Some(id) = param_id
            && params.iter().any(|p| p.id == id && p.default.is_some())
            && matches!(
                then_branch.as_slice(),
                [Stmt::Expr(Expr::LocalSet(set_id, _))] if *set_id == id
            )
        {
            drop_count += 1;
        } else {
            break;
        }
    }
    if drop_count > 0 {
        body.drain(..drop_count);
    }
}
