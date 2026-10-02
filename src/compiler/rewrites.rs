//! Perry HIR rewrites for WebAssembly component compatibility.

pub(crate) fn rewrite_program(program: &mut perry_hir::ir::Module) {
    // Perry emits this dispatcher call but omits its name from its runtime string pool.
    program
        .init
        .push(perry_hir::ir::Stmt::Expr(perry_hir::ir::Expr::String(
            "js_loose_eq".into(),
        )));
    let mut rewriter = Rewriter {
        literal_shapes: program
            .classes
            .iter()
            .filter(|class| class.name.starts_with("__AnonShape_"))
            .map(|class| {
                (
                    class.name.clone(),
                    class
                        .fields
                        .iter()
                        .map(|field| field.name.clone())
                        .collect(),
                )
            })
            .collect(),
        needs_clocks: false,
        needs_http: false,
    };
    for stmt in &mut program.init {
        rewriter.rewrite_stmt(stmt);
    }
    for func in &mut program.functions {
        for stmt in &mut func.body {
            rewriter.rewrite_stmt(stmt);
        }
    }
    if rewriter.needs_clocks {
        program
            .init
            .push(perry_hir::ir::Stmt::Expr(perry_hir::ir::Expr::String(
                "__needs_clocks__".into(),
            )));
        program
            .init
            .push(perry_hir::ir::Stmt::Expr(perry_hir::ir::Expr::String(
                "date_new".into(),
            )));
    }
    if rewriter.needs_http {
        program
            .init
            .push(perry_hir::ir::Stmt::Expr(perry_hir::ir::Expr::String(
                "__needs_http__".into(),
            )));
    }
}

struct Rewriter {
    literal_shapes: std::collections::HashMap<String, Vec<String>>,
    needs_clocks: bool,
    needs_http: bool,
}

impl Rewriter {
    fn rewrite_stmt(&mut self, stmt: &mut perry_hir::ir::Stmt) {
        use perry_hir::ir::Stmt;
        match stmt {
            Stmt::Expr(expr)
            | Stmt::Return(Some(expr))
            | Stmt::Throw(expr)
            | Stmt::Let {
                init: Some(expr), ..
            } => self.rewrite_expr(expr),
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.rewrite_expr(condition);
                for stmt in then_branch
                    .iter_mut()
                    .chain(else_branch.iter_mut().flatten())
                {
                    self.rewrite_stmt(stmt);
                }
            }
            Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                self.rewrite_expr(condition);
                for stmt in body {
                    self.rewrite_stmt(stmt);
                }
            }
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => {
                if let Some(init) = init {
                    self.rewrite_stmt(init);
                }
                for expr in condition.iter_mut().chain(update.iter_mut()) {
                    self.rewrite_expr(expr);
                }
                for stmt in body {
                    self.rewrite_stmt(stmt);
                }
            }
            Stmt::Labeled { body, .. } => self.rewrite_stmt(body),
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
                    self.rewrite_stmt(stmt);
                }
            }
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                self.rewrite_expr(discriminant);
                for case in cases {
                    if let Some(test) = &mut case.test {
                        self.rewrite_expr(test);
                    }
                    for stmt in &mut case.body {
                        self.rewrite_stmt(stmt);
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

    fn rewrite_current_expr(&mut self, expr: &mut perry_hir::ir::Expr) {
        match expr {
            perry_hir::ir::Expr::DateNow
            | perry_hir::ir::Expr::DateNew(_)
            | perry_hir::ir::Expr::DateGetTime(_)
            | perry_hir::ir::Expr::DateToISOString(_)
            | perry_hir::ir::Expr::DateGetFullYear(_)
            | perry_hir::ir::Expr::DateGetMonth(_)
            | perry_hir::ir::Expr::DateGetDate(_)
            | perry_hir::ir::Expr::DateGetDay(_)
            | perry_hir::ir::Expr::DateGetHours(_)
            | perry_hir::ir::Expr::DateGetMinutes(_)
            | perry_hir::ir::Expr::DateGetSeconds(_)
            | perry_hir::ir::Expr::DateGetMilliseconds(_)
            | perry_hir::ir::Expr::DateParse(_)
            | perry_hir::ir::Expr::DateUtc(_)
            | perry_hir::ir::Expr::DateValueOf(_)
            | perry_hir::ir::Expr::PerformanceNow => {
                self.needs_clocks = true;
            }
            perry_hir::ir::Expr::FetchWithOptions { .. }
            | perry_hir::ir::Expr::FetchGetWithAuth { .. }
            | perry_hir::ir::Expr::FetchPostWithAuth { .. } => {
                self.needs_http = true;
            }
            _ => {}
        }
        if let perry_hir::ir::Expr::NativeMethodCall {
            module,
            object: Some(object),
            method,
            args,
            ..
        } = expr
            && matches!(
                module.as_str(),
                "fetch" | "fetchWithAuth" | "fetchPostWithAuth"
            )
            && matches!(method.as_str(), "status" | "ok")
            && args.is_empty()
        {
            *expr = perry_hir::ir::Expr::PropertyGet {
                object: object.clone(),
                property: method.clone(),
                byte_offset: 0,
            };
        }
        if let perry_hir::ir::Expr::New {
            class_name, args, ..
        } = expr
            && let Some(fields) = self.literal_shapes.get(class_name)
            && fields.len() == args.len()
        {
            *expr = perry_hir::ir::Expr::Object(
                fields.iter().cloned().zip(std::mem::take(args)).collect(),
            );
        }
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
            {
                if property == "performance_now" || property.starts_with("date_") {
                    self.needs_clocks = true;
                }
                if matches!(
                    property.as_str(),
                    "fetch_request"
                        | "fetch_url"
                        | "fetch_with_options"
                        | "response_json"
                        | "response_text"
                        | "response_status"
                        | "response_ok"
                ) {
                    self.needs_http = true;
                }
                if property == "json" {
                    *expr = perry_hir::ir::Expr::NativeMethodCall {
                        module: "fetch".to_string(),
                        class_name: Some("Response".to_string()),
                        object: Some(object.clone()),
                        method: "json".to_string(),
                        args: args.clone(),
                    };
                    return;
                }
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

    fn rewrite_expr(&mut self, expr: &mut perry_hir::ir::Expr) {
        self.rewrite_current_expr(expr);
        if let perry_hir::ir::Expr::Closure { body, .. } = expr {
            for stmt in body {
                self.rewrite_stmt(stmt);
            }
        }
        perry_hir::walker::walk_expr_children_mut(expr, &mut |expr| self.rewrite_expr(expr));
    }
}
