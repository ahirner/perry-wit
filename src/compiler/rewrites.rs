//! Perry HIR rewrites for WebAssembly component compatibility.

pub(crate) fn rewrite_program(program: &mut perry_hir::ir::Module) {
    // Perry emits this dispatcher call but omits its name from its runtime string pool.
    program
        .init
        .push(perry_hir::ir::Stmt::Expr(perry_hir::ir::Expr::String(
            "js_loose_eq".into(),
        )));
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
    use perry_hir::ir::Stmt;
    match stmt {
        Stmt::Expr(expr)
        | Stmt::Return(Some(expr))
        | Stmt::Throw(expr)
        | Stmt::Let {
            init: Some(expr), ..
        } => rewrite_expr(expr),
        Stmt::If {
            condition,
            then_branch,
            else_branch,
        } => {
            rewrite_expr(condition);
            for stmt in then_branch
                .iter_mut()
                .chain(else_branch.iter_mut().flatten())
            {
                rewrite_stmt(stmt);
            }
        }
        Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
            rewrite_expr(condition);
            for stmt in body {
                rewrite_stmt(stmt);
            }
        }
        Stmt::For {
            init,
            condition,
            update,
            body,
        } => {
            if let Some(init) = init {
                rewrite_stmt(init);
            }
            for expr in condition.iter_mut().chain(update.iter_mut()) {
                rewrite_expr(expr);
            }
            for stmt in body {
                rewrite_stmt(stmt);
            }
        }
        Stmt::Labeled { body, .. } => rewrite_stmt(body),
        Stmt::Try {
            body,
            catch,
            finally,
        } => {
            for stmt in body
                .iter_mut()
                .chain(catch.iter_mut().flat_map(|catch| catch.body.iter_mut()))
                .chain(finally.iter_mut().flatten())
            {
                rewrite_stmt(stmt);
            }
        }
        Stmt::Switch {
            discriminant,
            cases,
        } => {
            rewrite_expr(discriminant);
            for case in cases {
                if let Some(test) = &mut case.test {
                    rewrite_expr(test);
                }
                for stmt in &mut case.body {
                    rewrite_stmt(stmt);
                }
            }
        }
        Stmt::Let { init: None, .. }
        | Stmt::Return(None)
        | Stmt::Break
        | Stmt::Continue
        | Stmt::LabeledBreak(_)
        | Stmt::LabeledContinue(_)
        | Stmt::PreallocateBoxes(_)
        | Stmt::PreallocateTdzBoxes(_)
        | Stmt::ReleaseBoxes(_) => {}
    }
}

fn rewrite_current_expr(expr: &mut perry_hir::ir::Expr) {
    if let perry_hir::ir::Expr::ProcessExit(code) = expr {
        let code = code
            .take()
            .map(|code| *code)
            .unwrap_or(perry_hir::ir::Expr::Integer(0));
        *expr = perry_hir::ir::Expr::Call {
            callee: Box::new(perry_hir::ir::Expr::PropertyGet {
                object: Box::new(perry_hir::ir::Expr::Undefined),
                property: "process_exit".into(),
                byte_offset: 0,
            }),
            args: vec![code],
            type_args: Vec::new(),
            byte_offset: 0,
        };
    }
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
}

fn rewrite_expr(expr: &mut perry_hir::ir::Expr) {
    rewrite_current_expr(expr);
    if let perry_hir::ir::Expr::Closure { body, .. } = expr {
        for stmt in body {
            rewrite_stmt(stmt);
        }
    }
    perry_hir::walker::walk_expr_children_mut(expr, &mut rewrite_expr);
}
