//! Perry HIR rewrites for WebAssembly component compatibility.

use perry_hir::ir::{Expr, Stmt};

pub(crate) fn rewrite_program(program: &mut perry_hir::ir::Module) {
    // Perry emits these dispatcher calls but omits their names from its string pool.
    for name in ["js_loose_eq", "string_char_at", "string_char_code_at"] {
        program.init.push(Stmt::Expr(Expr::String(name.into())));
    }
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
        needs_random: false,
        needs_env: false,
        needs_fs: false,
    };
    for stmt in &mut program.init {
        rewriter.rewrite_stmt(stmt);
    }
    for func in &mut program.functions {
        rewriter.rewrite_function(func);
    }
    for global in &mut program.globals {
        if let Some(init) = &mut global.init {
            rewriter.rewrite_expr(init);
        }
    }
    for class in &mut program.classes {
        for function in class
            .constructor
            .iter_mut()
            .chain(&mut class.methods)
            .chain(&mut class.static_methods)
            .chain(class.getters.iter_mut().map(|(_, function)| function))
            .chain(class.setters.iter_mut().map(|(_, function)| function))
            .chain(
                class
                    .computed_members
                    .iter_mut()
                    .map(|member| &mut member.function),
            )
        {
            rewriter.rewrite_function(function);
        }
        for field in class.fields.iter_mut().chain(&mut class.static_fields) {
            for expr in field.init.iter_mut().chain(&mut field.key_expr) {
                rewriter.rewrite_expr(expr);
            }
        }
        for member in &mut class.computed_members {
            rewriter.rewrite_expr(&mut member.key_expr);
        }
        if let Some(parent) = &mut class.extends_expr {
            rewriter.rewrite_expr(parent);
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
    if rewriter.needs_random {
        program
            .init
            .push(perry_hir::ir::Stmt::Expr(perry_hir::ir::Expr::String(
                "__needs_random__".into(),
            )));
    }
    if rewriter.needs_env {
        program
            .init
            .push(perry_hir::ir::Stmt::Expr(perry_hir::ir::Expr::String(
                "__needs_env__".into(),
            )));
        for name in [
            "process_env",
            "process_env_get",
            "process_argv",
            "process_cwd",
            "object_keys",
            "object_values",
            "object_entries",
            "object_has_property",
            "object_delete",
            "object_delete_dynamic",
        ] {
            program
                .init
                .push(perry_hir::ir::Stmt::Expr(perry_hir::ir::Expr::String(
                    name.into(),
                )));
        }
    }
    if rewriter.needs_fs {
        program
            .init
            .push(perry_hir::ir::Stmt::Expr(perry_hir::ir::Expr::String(
                "__needs_fs__".into(),
            )));
        for name in ["fs_read_file_sync", "fs_write_file_sync"] {
            program
                .init
                .push(perry_hir::ir::Stmt::Expr(perry_hir::ir::Expr::String(
                    name.into(),
                )));
        }
    }
}

struct Rewriter {
    literal_shapes: std::collections::HashMap<String, Vec<String>>,
    needs_clocks: bool,
    needs_http: bool,
    needs_random: bool,
    needs_env: bool,
    needs_fs: bool,
}

impl Rewriter {
    /// Rewrites a callable's defaults and body before selecting its capabilities.
    fn rewrite_function(&mut self, function: &mut perry_hir::ir::Function) {
        for parameter in &mut function.params {
            if let Some(default) = &mut parameter.default {
                self.rewrite_expr(default);
            }
        }
        for statement in &mut function.body {
            self.rewrite_stmt(statement);
        }
    }

    fn rewrite_stmt(&mut self, stmt: &mut perry_hir::ir::Stmt) {
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
                if let Some(finally) = finally {
                    if let Some(catch) = catch {
                        catch
                            .body
                            .insert(0, exception_marker("__perry_catch_start"));
                        catch.body.push(exception_marker("__perry_catch_end"));
                    }
                    finally.insert(0, exception_marker("__perry_finally_start"));
                    finally.push(exception_marker("__perry_finally_end"));
                }
                let original = std::mem::replace(stmt, Stmt::Return(None));
                *stmt = Stmt::If {
                    condition: perry_hir::ir::Expr::Bool(true),
                    then_branch: vec![original, exception_marker("__perry_exception_resume")],
                    else_branch: None,
                };
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
        if let Expr::Delete(target) = expr {
            let key = match target.as_mut() {
                Expr::EnvGet(name) => Some(Expr::String(name.clone())),
                Expr::EnvGetDynamic(key) => Some(std::mem::replace(key.as_mut(), Expr::Undefined)),
                _ => None,
            };
            if let Some(key) = key {
                **target = Expr::IndexGet {
                    object: Box::new(Expr::ProcessEnv),
                    index: Box::new(key),
                };
            }
        }
        // Perry's specialized constructor omits its arguments from string collection.
        if let Expr::Uint8ArrayNew(size) = expr {
            *expr = Expr::Call {
                callee: Box::new(Expr::PropertyGet {
                    object: size.take().unwrap_or_else(|| Box::new(Expr::Undefined)),
                    property: "uint8array_new".into(),
                    byte_offset: 0,
                }),
                args: Vec::new(),
                type_args: Vec::new(),
                byte_offset: 0,
            };
            return;
        }
        // Perry otherwise lowers both an omitted argument and explicit undefined to the same call.
        if let perry_hir::ir::Expr::DateNew(args) = expr
            && args.is_empty()
        {
            args.push(perry_hir::ir::Expr::DateNow);
        }
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
            perry_hir::ir::Expr::MathRandom
            | perry_hir::ir::Expr::CryptoRandomUUID
            | perry_hir::ir::Expr::CryptoRandomUUIDv7
            | perry_hir::ir::Expr::CryptoRandomBytes(_) => {
                self.needs_random = true;
            }
            perry_hir::ir::Expr::ProcessEnv
            | perry_hir::ir::Expr::EnvGet(_)
            | perry_hir::ir::Expr::EnvGetDynamic(_)
            | perry_hir::ir::Expr::ProcessArgv
            | perry_hir::ir::Expr::ProcessCwd => {
                self.needs_env = true;
            }
            perry_hir::ir::Expr::FsReadFileSync(_)
            | perry_hir::ir::Expr::FsReadFileBinary(_)
            | perry_hir::ir::Expr::FsWriteFileSync(_, _) => {
                self.needs_fs = true;
            }
            _ => {}
        }
        if let perry_hir::ir::Expr::FsReadFileSync(path_expr) = expr {
            self.needs_fs = true;
            let path = std::mem::replace(path_expr.as_mut(), perry_hir::ir::Expr::Undefined);
            *expr = perry_hir::ir::Expr::Call {
                callee: Box::new(perry_hir::ir::Expr::PropertyGet {
                    object: Box::new(perry_hir::ir::Expr::Undefined),
                    property: "fs_read_file_sync".into(),
                    byte_offset: 0,
                }),
                args: vec![path, perry_hir::ir::Expr::String("utf8".into())],
                type_args: Vec::new(),
                byte_offset: 0,
            };
            return;
        }
        if let perry_hir::ir::Expr::FsReadFileBinary(path_expr) = expr {
            self.needs_fs = true;
            let path = std::mem::replace(path_expr.as_mut(), perry_hir::ir::Expr::Undefined);
            *expr = perry_hir::ir::Expr::Call {
                callee: Box::new(perry_hir::ir::Expr::PropertyGet {
                    object: Box::new(perry_hir::ir::Expr::Undefined),
                    property: "fs_read_file_binary".into(),
                    byte_offset: 0,
                }),
                args: vec![path],
                type_args: Vec::new(),
                byte_offset: 0,
            };
            return;
        }
        if let perry_hir::ir::Expr::FsWriteFileSync(path_expr, data_expr) = expr {
            self.needs_fs = true;
            let path = std::mem::replace(path_expr.as_mut(), perry_hir::ir::Expr::Undefined);
            let data = std::mem::replace(data_expr.as_mut(), perry_hir::ir::Expr::Undefined);
            *expr = perry_hir::ir::Expr::Call {
                callee: Box::new(perry_hir::ir::Expr::PropertyGet {
                    object: Box::new(perry_hir::ir::Expr::Undefined),
                    property: "fs_write_file_sync".into(),
                    byte_offset: 0,
                }),
                args: vec![path, data],
                type_args: Vec::new(),
                byte_offset: 0,
            };
            return;
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
        if let perry_hir::ir::Expr::NativeMethodCall {
            module,
            method,
            args,
            ..
        } = expr
        {
            if module == "fs" || module == "node:fs" {
                if method == "readFileSync" {
                    self.needs_fs = true;
                    let path = if !args.is_empty() {
                        std::mem::replace(&mut args[0], perry_hir::ir::Expr::Undefined)
                    } else {
                        perry_hir::ir::Expr::Undefined
                    };
                    let options = if args.len() >= 2 {
                        std::mem::replace(&mut args[1], perry_hir::ir::Expr::Undefined)
                    } else {
                        perry_hir::ir::Expr::Undefined
                    };
                    *expr = perry_hir::ir::Expr::Call {
                        callee: Box::new(perry_hir::ir::Expr::PropertyGet {
                            object: Box::new(perry_hir::ir::Expr::Undefined),
                            property: "fs_read_file_sync".into(),
                            byte_offset: 0,
                        }),
                        args: vec![path, options],
                        type_args: Vec::new(),
                        byte_offset: 0,
                    };
                    return;
                } else if method == "writeFileSync" {
                    self.needs_fs = true;
                    let path = if !args.is_empty() {
                        std::mem::replace(&mut args[0], perry_hir::ir::Expr::Undefined)
                    } else {
                        perry_hir::ir::Expr::Undefined
                    };
                    let content = if args.len() >= 2 {
                        std::mem::replace(&mut args[1], perry_hir::ir::Expr::Undefined)
                    } else {
                        perry_hir::ir::Expr::Undefined
                    };
                    let options = if args.len() >= 3 {
                        std::mem::replace(&mut args[2], Expr::Undefined)
                    } else {
                        Expr::Undefined
                    };
                    *expr = perry_hir::ir::Expr::Call {
                        callee: Box::new(perry_hir::ir::Expr::PropertyGet {
                            object: Box::new(perry_hir::ir::Expr::Undefined),
                            property: "fs_write_file_sync".into(),
                            byte_offset: 0,
                        }),
                        args: vec![path, content, options],
                        type_args: Vec::new(),
                        byte_offset: 0,
                    };
                    return;
                }
            }
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
        if matches!(expr, perry_hir::ir::Expr::ProcessEnv) {
            self.needs_env = true;
            *expr = perry_hir::ir::Expr::Call {
                callee: Box::new(perry_hir::ir::Expr::PropertyGet {
                    object: Box::new(perry_hir::ir::Expr::Undefined),
                    property: "process_env".into(),
                    byte_offset: 0,
                }),
                args: Vec::new(),
                type_args: Vec::new(),
                byte_offset: 0,
            };
            return;
        }
        if let perry_hir::ir::Expr::EnvGet(var_name) = expr {
            self.needs_env = true;
            *expr = perry_hir::ir::Expr::Call {
                callee: Box::new(perry_hir::ir::Expr::PropertyGet {
                    object: Box::new(perry_hir::ir::Expr::Undefined),
                    property: "process_env_get".into(),
                    byte_offset: 0,
                }),
                args: vec![perry_hir::ir::Expr::String(var_name.clone())],
                type_args: Vec::new(),
                byte_offset: 0,
            };
            return;
        }
        if let perry_hir::ir::Expr::EnvGetDynamic(key_expr) = expr {
            self.needs_env = true;
            let key = std::mem::replace(key_expr.as_mut(), perry_hir::ir::Expr::Undefined);
            *expr = perry_hir::ir::Expr::Call {
                callee: Box::new(perry_hir::ir::Expr::PropertyGet {
                    object: Box::new(perry_hir::ir::Expr::Undefined),
                    property: "process_env_get".into(),
                    byte_offset: 0,
                }),
                args: vec![key],
                type_args: Vec::new(),
                byte_offset: 0,
            };
            return;
        }
        if matches!(expr, perry_hir::ir::Expr::ProcessArgv) {
            self.needs_env = true;
            *expr = perry_hir::ir::Expr::Call {
                callee: Box::new(perry_hir::ir::Expr::PropertyGet {
                    object: Box::new(perry_hir::ir::Expr::Undefined),
                    property: "process_argv".into(),
                    byte_offset: 0,
                }),
                args: Vec::new(),
                type_args: Vec::new(),
                byte_offset: 0,
            };
            return;
        }
        if matches!(expr, perry_hir::ir::Expr::ProcessCwd) {
            self.needs_env = true;
            *expr = perry_hir::ir::Expr::Call {
                callee: Box::new(perry_hir::ir::Expr::PropertyGet {
                    object: Box::new(perry_hir::ir::Expr::Undefined),
                    property: "process_cwd".into(),
                    byte_offset: 0,
                }),
                args: Vec::new(),
                type_args: Vec::new(),
                byte_offset: 0,
            };
            return;
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
                if property == "subarray" {
                    let mut args_iter = args.clone().into_iter();
                    let start = args_iter.next();
                    let end = args_iter.next();
                    *expr = perry_hir::ir::Expr::BufferSlice {
                        buffer: object.clone(),
                        start: start.map(Box::new),
                        end: end.map(Box::new),
                    };
                    return;
                }
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
                if matches!(
                    property.as_str(),
                    "math_random"
                        | "randomUUID"
                        | "randomUUIDv7"
                        | "getRandomValues"
                        | "$$cryptoFillRandom"
                        | "randomBytes"
                ) {
                    self.needs_random = true;
                }
                if let perry_hir::ir::Expr::NativeModuleRef(module) = object.as_ref() {
                    if module == "fs" || module == "node:fs" {
                        if property == "readFileSync" {
                            self.needs_fs = true;
                            let path = if !args.is_empty() {
                                std::mem::replace(&mut args[0], perry_hir::ir::Expr::Undefined)
                            } else {
                                perry_hir::ir::Expr::Undefined
                            };
                            let options = if args.len() >= 2 {
                                std::mem::replace(&mut args[1], perry_hir::ir::Expr::Undefined)
                            } else {
                                perry_hir::ir::Expr::Undefined
                            };
                            *expr = perry_hir::ir::Expr::Call {
                                callee: Box::new(perry_hir::ir::Expr::PropertyGet {
                                    object: Box::new(perry_hir::ir::Expr::Undefined),
                                    property: "fs_read_file_sync".into(),
                                    byte_offset: 0,
                                }),
                                args: vec![path, options],
                                type_args: Vec::new(),
                                byte_offset: 0,
                            };
                            return;
                        } else if property == "writeFileSync" {
                            self.needs_fs = true;
                            let path = if !args.is_empty() {
                                std::mem::replace(&mut args[0], perry_hir::ir::Expr::Undefined)
                            } else {
                                perry_hir::ir::Expr::Undefined
                            };
                            let content = if args.len() >= 2 {
                                std::mem::replace(&mut args[1], perry_hir::ir::Expr::Undefined)
                            } else {
                                perry_hir::ir::Expr::Undefined
                            };
                            let options = if args.len() >= 3 {
                                std::mem::replace(&mut args[2], Expr::Undefined)
                            } else {
                                Expr::Undefined
                            };
                            *expr = perry_hir::ir::Expr::Call {
                                callee: Box::new(perry_hir::ir::Expr::PropertyGet {
                                    object: Box::new(perry_hir::ir::Expr::Undefined),
                                    property: "fs_write_file_sync".into(),
                                    byte_offset: 0,
                                }),
                                args: vec![path, content, options],
                                type_args: Vec::new(),
                                byte_offset: 0,
                            };
                            return;
                        }
                    }
                }
                if property == "fs_read_file_sync"
                    || property == "fs_read_file_binary"
                    || property == "fs_write_file_sync"
                {
                    self.needs_fs = true;
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
            if let perry_hir::ir::Expr::ExternFuncRef { name, .. } = callee.as_ref() {
                if name == "js_native_module_named_esm_export_value" {
                    if let Some(perry_hir::ir::Expr::String(mod_name)) = args.first() {
                        if mod_name == "fs" || mod_name == "node:fs" {
                            *expr = perry_hir::ir::Expr::Undefined;
                            return;
                        }
                    }
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

/// Preserves exception-region boundaries through Perry's memory-call emission.
fn exception_marker(name: &str) -> Stmt {
    Stmt::Expr(Expr::Call {
        callee: Box::new(Expr::PropertyGet {
            object: Box::new(Expr::Undefined),
            property: name.into(),
            byte_offset: 0,
        }),
        args: Vec::new(),
        type_args: Vec::new(),
        byte_offset: 0,
    })
}
