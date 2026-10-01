//! Perry HIR rewrites for WebAssembly component compatibility.

pub(crate) fn rewrite_program(program: &mut perry_hir::ir::Module) {
    for stmt in &mut program.init {
        rewrite_stmt(stmt);
    }
    for func in &mut program.functions {
        for stmt in &mut func.body {
            rewrite_stmt(stmt);
        }
    }
}

fn rewrite_stmt(stmt: &mut perry_hir::ir::Stmt) {
    match stmt {
        perry_hir::ir::Stmt::Expr(e)
        | perry_hir::ir::Stmt::Return(Some(e))
        | perry_hir::ir::Stmt::Throw(e) => {
            rewrite_expr(e);
        }
        perry_hir::ir::Stmt::Let { init: Some(e), .. } => {
            rewrite_expr(e);
        }
        _ => {}
    }
}

fn rewrite_expr(expr: &mut perry_hir::ir::Expr) {
    if let perry_hir::ir::Expr::JsonStringifyFull(val, _, _) = expr {
        *expr = perry_hir::ir::Expr::JsonStringify(val.clone());
        return;
    }
    if let perry_hir::ir::Expr::Call { callee, args, .. } = expr {
        if let perry_hir::ir::Expr::PropertyGet {
            object, property, ..
        } = callee.as_ref()
            && property == "json"
        {
            *expr = perry_hir::ir::Expr::NativeMethodCall {
                module: "fetch".to_string(),
                class_name: Some("Response".to_string()),
                object: Some(object.clone()),
                method: "json".to_string(),
                args: args.clone(),
            };
            return;
        }
        if let perry_hir::ir::Expr::Closure { body, .. } = callee.as_mut() {
            let mut sources = Vec::new();
            let mut is_spread_iife = !body.is_empty();
            for stmt in body.iter() {
                match stmt {
                    perry_hir::ir::Stmt::Expr(perry_hir::ir::Expr::Call {
                        callee: inner_callee,
                        args: inner_args,
                        ..
                    }) => {
                        if let perry_hir::ir::Expr::ExternFuncRef { name, .. } =
                            inner_callee.as_ref()
                            && name == "js_object_assign_one"
                            && inner_args.len() >= 2
                        {
                            sources.push(inner_args[1].clone());
                            continue;
                        }
                        is_spread_iife = false;
                        break;
                    }
                    perry_hir::ir::Stmt::Return(_) => {}
                    _ => {
                        is_spread_iife = false;
                        break;
                    }
                }
            }
            if is_spread_iife && !sources.is_empty() && !args.is_empty() {
                let target = args.remove(0);
                *expr = perry_hir::ir::Expr::ObjectAssign {
                    target: Box::new(target),
                    sources,
                };
                return;
            }
        }
    }
    perry_hir::walker::walk_expr_children_mut(expr, &mut rewrite_expr);
}
