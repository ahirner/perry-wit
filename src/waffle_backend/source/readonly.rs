//! Infer context aliases before frontend rewrites can erase writes or call arguments.

use super::underlying_expression;
use anyhow::{Result, bail};
use perry_parser::swc_ecma_ast as ast;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use swc_common::SyntaxContext;
use swc_ecma_visit::{Visit, VisitWith};

pub(super) fn validate(module: &ast::Module, unresolved: SyntaxContext) -> Result<()> {
    let mut graph = Aliases::new(unresolved);
    module.visit_with(&mut graph);
    graph.solve();
    for &target in &graph.writes {
        for &object in &graph.values[target] {
            if let Some(name) = graph.objects[object].readonly {
                bail!("{name} is read-only; context mutation is unsupported");
            }
        }
    }
    for call in &graph.calls {
        if graph.values[call.callee].is_empty()
            || graph.values[call.callee]
                .iter()
                .any(|&object| graph.objects[object].function.is_none())
        {
            for &argument in &call.arguments {
                if let Some(name) = graph.readonly_reachable(argument, &mut BTreeSet::new()) {
                    bail!(
                        "{name} is read-only; passing context to an unresolved call is unsupported"
                    );
                }
            }
        }
    }
    Ok(())
}

type Node = usize;
type Key = Option<String>;

#[derive(Default)]
struct Object {
    fields: BTreeMap<Key, BTreeSet<usize>>,
    readonly: Option<&'static str>,
    function: Option<Function>,
}

struct Function {
    parameters: Vec<Node>,
    result: Node,
}

struct Call {
    callee: Node,
    arguments: Vec<Node>,
    result: Node,
}

struct Aliases {
    unresolved: SyntaxContext,
    process: Node,
    bindings: HashMap<ast::Id, Node>,
    values: Vec<BTreeSet<usize>>,
    objects: Vec<Object>,
    copies: Vec<(Node, Node)>,
    loads: Vec<(Node, Key, Node)>,
    stores: Vec<(Node, Key, Node)>,
    field_copies: Vec<(Node, Node)>,
    calls: Vec<Call>,
    writes: Vec<Node>,
    result: Option<Node>,
}

impl Aliases {
    fn new(unresolved: SyntaxContext) -> Self {
        let mut graph = Self {
            unresolved,
            process: 0,
            bindings: HashMap::new(),
            values: Vec::new(),
            objects: Vec::new(),
            copies: Vec::new(),
            loads: Vec::new(),
            stores: Vec::new(),
            field_copies: Vec::new(),
            calls: Vec::new(),
            writes: Vec::new(),
            result: None,
        };
        graph.process = graph.object(Object {
            readonly: Some("process context"),
            ..Object::default()
        });
        for (key, name) in [("env", "process.env"), ("argv", "process.argv")] {
            let value = graph.object(Object {
                readonly: Some(name),
                ..Object::default()
            });
            graph.stores.push((graph.process, Some(key.into()), value));
        }
        graph
    }

    fn node(&mut self) -> Node {
        let node = self.values.len();
        self.values.push(BTreeSet::new());
        node
    }

    fn object(&mut self, object: Object) -> Node {
        let node = self.node();
        self.values[node].insert(self.objects.len());
        self.objects.push(object);
        node
    }

    fn binding(&mut self, name: &ast::Ident) -> Node {
        if name.sym == "process" && name.ctxt == self.unresolved {
            return self.process;
        }
        if let Some(&node) = self.bindings.get(&name.to_id()) {
            return node;
        }
        let node = self.node();
        self.bindings.insert(name.to_id(), node);
        node
    }

    fn load(&mut self, object: Node, key: Key) -> Node {
        let result = self.node();
        self.loads.push((object, key, result));
        result
    }

    fn member_key(&mut self, property: &ast::MemberProp) -> Key {
        match property {
            ast::MemberProp::Ident(name) => Some(name.sym.to_string()),
            ast::MemberProp::Computed(key) => {
                self.expression(&key.expr);
                literal_key(&key.expr)
            }
            _ => None,
        }
    }

    fn bind_pattern(&mut self, pattern: &ast::Pat, value: Node) {
        match pattern {
            ast::Pat::Ident(name) => {
                let target = self.binding(&name.id);
                self.copies.push((value, target));
            }
            ast::Pat::Assign(assign) => {
                self.bind_pattern(&assign.left, value);
                let default = self.expression(&assign.right);
                self.bind_pattern(&assign.left, default);
            }
            ast::Pat::Object(object) => {
                for property in &object.props {
                    match property {
                        ast::ObjectPatProp::KeyValue(property) => {
                            let field = self.load(value, property_key(&property.key));
                            self.bind_pattern(&property.value, field);
                        }
                        ast::ObjectPatProp::Assign(property) => {
                            let field = self.load(value, Some(property.key.id.sym.to_string()));
                            let target = self.binding(&property.key.id);
                            self.copies.push((field, target));
                            if let Some(default) = &property.value {
                                let default = self.expression(default);
                                self.copies.push((default, target));
                            }
                        }
                        ast::ObjectPatProp::Rest(rest) => {
                            let copy = self.object(Object::default());
                            self.field_copies.push((value, copy));
                            self.bind_pattern(&rest.arg, copy);
                        }
                    }
                }
            }
            ast::Pat::Array(array) => {
                for (index, element) in array.elems.iter().enumerate() {
                    if let Some(element) = element {
                        let field = self.load(value, Some(index.to_string()));
                        self.bind_pattern(element, field);
                    }
                }
            }
            ast::Pat::Rest(rest) => self.bind_pattern(&rest.arg, value),
            ast::Pat::Expr(expression) => self.assign(expression, value),
            _ => {}
        }
    }

    fn assign(&mut self, target: &ast::Expr, value: Node) {
        match underlying_expression(target) {
            ast::Expr::Ident(name) => {
                let target = self.binding(name);
                self.copies.push((value, target));
            }
            ast::Expr::Member(member) => {
                let object = self.expression(&member.obj);
                let key = self.member_key(&member.prop);
                if super::context::process_property(member, self.unresolved) != Some("exitCode") {
                    self.writes.push(object);
                }
                self.stores.push((object, key, value));
            }
            _ => {
                self.expression(target);
            }
        }
    }

    fn function(
        &mut self,
        parameters: impl Iterator<Item = ast::Pat>,
        body: impl FnOnce(&mut Self),
    ) -> Node {
        let parameters = parameters
            .map(|pattern| {
                let node = self.node();
                self.bind_pattern(&pattern, node);
                node
            })
            .collect();
        let result = self.node();
        let previous = self.result.replace(result);
        body(self);
        self.result = previous;
        self.object(Object {
            function: Some(Function { parameters, result }),
            ..Object::default()
        })
    }

    fn expression(&mut self, expression: &ast::Expr) -> Node {
        let expression = underlying_expression(expression);
        match expression {
            ast::Expr::Ident(name) => self.binding(name),
            ast::Expr::Member(member) => {
                let object = self.expression(&member.obj);
                let key = self.member_key(&member.prop);
                self.load(object, key)
            }
            ast::Expr::Object(object) => {
                let target = self.object(Object::default());
                for property in &object.props {
                    match property {
                        ast::PropOrSpread::Prop(property) => match property.as_ref() {
                            ast::Prop::KeyValue(property) => {
                                let value = self.expression(&property.value);
                                self.stores
                                    .push((target, property_key(&property.key), value));
                            }
                            ast::Prop::Shorthand(name) => {
                                let value = self.binding(name);
                                self.stores
                                    .push((target, Some(name.sym.to_string()), value));
                            }
                            _ => property.visit_children_with(self),
                        },
                        ast::PropOrSpread::Spread(spread) => {
                            let source = self.expression(&spread.expr);
                            self.field_copies.push((source, target));
                        }
                    }
                }
                target
            }
            ast::Expr::Array(array) => {
                let target = self.object(Object::default());
                for (index, item) in array.elems.iter().enumerate() {
                    if let Some(item) = item {
                        let value = self.expression(&item.expr);
                        if item.spread.is_some() {
                            self.field_copies.push((value, target));
                        } else {
                            self.stores.push((target, Some(index.to_string()), value));
                        }
                    }
                }
                target
            }
            ast::Expr::Assign(assign) => {
                let value = self.expression(&assign.right);
                match &assign.left {
                    ast::AssignTarget::Simple(target) => {
                        let expression: Box<ast::Expr> = target.clone().into();
                        self.assign(&expression, value);
                    }
                    ast::AssignTarget::Pat(pattern) => {
                        self.bind_pattern(&pattern.clone().into(), value)
                    }
                }
                value
            }
            ast::Expr::Update(update) => {
                let value = self.node();
                self.assign(&update.arg, value);
                value
            }
            ast::Expr::Unary(unary) if unary.op == ast::UnaryOp::Delete => {
                let value = self.node();
                self.assign(&unary.arg, value);
                value
            }
            ast::Expr::Cond(condition) => {
                self.expression(&condition.test);
                let left = self.expression(&condition.cons);
                let right = self.expression(&condition.alt);
                let result = self.node();
                self.copies.extend([(left, result), (right, result)]);
                result
            }
            ast::Expr::Bin(binary)
                if matches!(
                    binary.op,
                    ast::BinaryOp::LogicalOr
                        | ast::BinaryOp::LogicalAnd
                        | ast::BinaryOp::NullishCoalescing
                ) =>
            {
                let left = self.expression(&binary.left);
                let right = self.expression(&binary.right);
                let result = self.node();
                self.copies.extend([(left, result), (right, result)]);
                result
            }
            ast::Expr::Await(awaited) => self.expression(&awaited.arg),
            ast::Expr::OptChain(chain) => match chain.base.as_ref() {
                ast::OptChainBase::Member(member) => {
                    let object = self.expression(&member.obj);
                    let key = self.member_key(&member.prop);
                    self.load(object, key)
                }
                ast::OptChainBase::Call(call) => self.call(&ast::CallExpr {
                    span: call.span,
                    ctxt: SyntaxContext::empty(),
                    callee: ast::Callee::Expr(call.callee.clone()),
                    args: call.args.clone(),
                    type_args: call.type_args.clone(),
                }),
            },
            ast::Expr::Seq(sequence) => sequence
                .exprs
                .iter()
                .map(|expr| self.expression(expr))
                .last()
                .unwrap(),
            ast::Expr::Call(call) => self.call(call),
            ast::Expr::Fn(function) => self.function(
                function
                    .function
                    .params
                    .iter()
                    .map(|param| param.pat.clone()),
                |graph| function.function.body.visit_with(graph),
            ),
            ast::Expr::Arrow(function) => self.function(function.params.iter().cloned(), |graph| {
                match function.body.as_ref() {
                    ast::BlockStmtOrExpr::BlockStmt(body) => body.visit_with(graph),
                    ast::BlockStmtOrExpr::Expr(body) => {
                        let value = graph.expression(body);
                        graph.copies.push((value, graph.result.unwrap()));
                    }
                }
            }),
            _ => {
                expression.visit_children_with(self);
                self.node()
            }
        }
    }

    fn call(&mut self, call: &ast::CallExpr) -> Node {
        let arguments: Vec<_> = call
            .args
            .iter()
            .map(|arg| self.expression(&arg.expr))
            .collect();
        let ast::Callee::Expr(callee) = &call.callee else {
            return self.node();
        };
        if let ast::Expr::Member(member) = underlying_expression(callee) {
            let key = self.member_key(&member.prop);
            let builtin = match underlying_expression(&member.obj) {
                ast::Expr::Ident(name) if name.ctxt == self.unresolved => Some(name.sym.as_ref()),
                _ => None,
            };
            if builtin == Some("Object")
                && key.as_deref() == Some("assign")
                && !arguments.is_empty()
            {
                let target = arguments[0];
                self.writes.push(target);
                self.field_copies
                    .extend(arguments.iter().skip(1).map(|&source| (source, target)));
                return target;
            }
            if matches!(
                (builtin, key.as_deref()),
                (Some("Object"), Some("keys" | "values"))
                    | (Some("JSON"), Some("parse" | "stringify"))
                    | (Some("Array"), Some("isArray"))
                    | (Some("process"), Some("cwd" | "exit"))
            ) {
                return self.object(Object::default());
            }
            let receiver = self.expression(&member.obj);
            if matches!(
                key.as_deref(),
                Some("join" | "includes" | "indexOf" | "lastIndexOf" | "at" | "slice" | "concat")
            ) {
                let result = self.object(Object::default());
                if matches!(key.as_deref(), Some("slice" | "concat")) {
                    self.field_copies.push((receiver, result));
                }
                if key.as_deref() == Some("concat") {
                    self.field_copies
                        .extend(arguments.iter().map(|&argument| (argument, result)));
                }
                if key.as_deref() == Some("at") {
                    self.loads.push((receiver, None, result));
                }
                return result;
            }
            // Unknown methods may mutate their receiver, including computed method names.
            self.writes.push(receiver);
        }
        let callee = self.expression(callee);
        let result = self.node();
        self.calls.push(Call {
            callee,
            arguments,
            result,
        });
        result
    }

    fn solve(&mut self) {
        loop {
            let mut changed = false;
            for &(from, to) in &self.copies {
                changed |= extend_values(&mut self.values, from, to);
            }
            for call in &self.calls {
                for &object in &self.values[call.callee].clone() {
                    if let Some(function) = &self.objects[object].function {
                        for (&argument, &parameter) in
                            call.arguments.iter().zip(&function.parameters)
                        {
                            changed |= extend_values(&mut self.values, argument, parameter);
                        }
                        changed |= extend_values(&mut self.values, function.result, call.result);
                    }
                }
            }
            for (receiver, key, value) in &self.stores {
                for &object in &self.values[*receiver] {
                    let field = self.objects[object].fields.entry(key.clone()).or_default();
                    for &origin in &self.values[*value] {
                        changed |= field.insert(origin);
                    }
                }
            }
            for &(source, target) in &self.field_copies {
                for &source in &self.values[source] {
                    let fields = self.objects[source].fields.clone();
                    for &target in &self.values[target] {
                        for (key, values) in &fields {
                            let field = self.objects[target].fields.entry(key.clone()).or_default();
                            for &origin in values {
                                changed |= field.insert(origin);
                            }
                        }
                    }
                }
            }
            for (receiver, key, result) in &self.loads {
                let mut origins = BTreeSet::new();
                for &object in &self.values[*receiver] {
                    for (field, values) in &self.objects[object].fields {
                        if key.is_none() || field.is_none() || field == key {
                            origins.extend(values);
                        }
                    }
                }
                for origin in origins {
                    changed |= self.values[*result].insert(origin);
                }
            }
            if !changed {
                break;
            }
        }
    }

    fn readonly_reachable(
        &self,
        value: Node,
        visited: &mut BTreeSet<usize>,
    ) -> Option<&'static str> {
        let mut pending: Vec<_> = self.values[value].iter().copied().collect();
        while let Some(object) = pending.pop() {
            if !visited.insert(object) {
                continue;
            }
            let object = &self.objects[object];
            if object.readonly.is_some() {
                return object.readonly;
            }
            pending.extend(object.fields.values().flatten());
        }
        None
    }
}

impl Visit for Aliases {
    fn visit_expr(&mut self, expression: &ast::Expr) {
        self.expression(expression);
    }
    fn visit_var_declarator(&mut self, declaration: &ast::VarDeclarator) {
        if let Some(init) = &declaration.init {
            let value = self.expression(init);
            self.bind_pattern(&declaration.name, value);
        }
    }
    fn visit_fn_decl(&mut self, function: &ast::FnDecl) {
        let value = self.function(
            function
                .function
                .params
                .iter()
                .map(|param| param.pat.clone()),
            |graph| function.function.body.visit_with(graph),
        );
        let target = self.binding(&function.ident);
        self.copies.push((value, target));
    }
    fn visit_return_stmt(&mut self, statement: &ast::ReturnStmt) {
        if let Some(argument) = &statement.arg {
            let value = self.expression(argument);
            if let Some(result) = self.result {
                self.copies.push((value, result));
            }
        }
    }
    fn visit_for_of_stmt(&mut self, statement: &ast::ForOfStmt) {
        let iterable = self.expression(&statement.right);
        let element = self.load(iterable, None);
        match &statement.left {
            ast::ForHead::VarDecl(declaration) => {
                for declaration in &declaration.decls {
                    self.bind_pattern(&declaration.name, element);
                }
            }
            ast::ForHead::Pat(pattern) => self.bind_pattern(pattern, element),
            _ => statement.left.visit_with(self),
        }
        statement.body.visit_with(self);
    }
}

fn literal_key(expression: &ast::Expr) -> Key {
    match underlying_expression(expression) {
        ast::Expr::Lit(ast::Lit::Str(text)) => text.value.as_str().map(str::to_owned),
        ast::Expr::Lit(ast::Lit::Num(number)) => Some(number.value.to_string()),
        _ => None,
    }
}

fn property_key(property: &ast::PropName) -> Key {
    match property {
        ast::PropName::Ident(name) => Some(name.sym.to_string()),
        ast::PropName::Str(text) => text.value.as_str().map(str::to_owned),
        ast::PropName::Num(number) => Some(number.value.to_string()),
        ast::PropName::Computed(key) => literal_key(&key.expr),
        _ => None,
    }
}

fn extend_values(values: &mut [BTreeSet<usize>], source: Node, target: Node) -> bool {
    let mut changed = false;
    for origin in values[source].clone() {
        changed |= values[target].insert(origin);
    }
    changed
}
