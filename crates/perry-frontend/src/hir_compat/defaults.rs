use super::*;

pub fn strip_synthetic_default_guards<'a, 'p>(
    mut body: &'a [Stmt],
    params: impl Iterator<Item = &'p perry_hir::ir::Param> + Clone,
) -> &'a [Stmt] {
    while let Some(Stmt::If {
        condition:
            Expr::Compare {
                op: CompareOp::Eq,
                left,
                right,
            },
        then_branch,
        else_branch: None,
    }) = body.first()
    {
        let param_id = match (&**left, &**right) {
            (Expr::LocalGet(id), Expr::Undefined) | (Expr::Undefined, Expr::LocalGet(id)) => {
                Some(*id)
            }
            _ => None,
        };
        if let Some(id) = param_id
            && params.clone().any(|p| p.id == id && p.default.is_some())
            && matches!(
                then_branch.as_slice(),
                [Stmt::Expr(Expr::LocalSet(set_id, _))] if *set_id == id
            )
        {
            body = &body[1..];
        } else {
            break;
        }
    }
    body
}
