//! Infer a component contract from resolved TypeScript declarations.
use crate::{Artifact, DiagnosticCode, PerryDiagnostic, Program};
use anyhow::{Context as _, Result, bail, ensure};
use perry_source::{Locations, Span, Spanned, Visit, VisitWith, ast};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Type {
    Scalar(&'static str),
    Void,
    List(Box<Type>),
    Tuple(Vec<Type>),
    Record(Vec<(String, Type)>),
    Option(Box<Type>),
    Absent {
        value: Box<Type>,
        null: bool,
    },
    Enum(Vec<String>),
    Variant(Vec<(String, Option<Type>)>),
    Result {
        ok: Option<Box<Type>>,
        err: Option<Box<Type>>,
    },
    Stream,
}

struct Declarations<'a> {
    aliases: HashMap<&'a str, &'a ast::TsTypeAliasDecl>,
    interfaces: HashMap<&'a str, &'a ast::TsInterfaceDecl>,
    functions: HashMap<&'a str, &'a ast::FnDecl>,
    primitives: HashMap<&'a str, &'static str>,
    locations: &'a Locations<'a>,
}

struct Signature<'a> {
    function: &'a ast::FnDecl,
    params: Vec<(&'a str, Type)>,
    result: Type,
}

fn scalar(name: &str) -> Option<&'static str> {
    [
        "u8", "s8", "u16", "s16", "u32", "s32", "u64", "s64", "f32", "f64",
    ]
    .into_iter()
    .find(|candidate| *candidate == name)
}

impl Declarations<'_> {
    fn error(&self, span: Span, message: impl Into<String>) -> anyhow::Error {
        PerryDiagnostic::error(DiagnosticCode::UnsupportedType, message)
            .with_optional_span(self.locations.span(span))
            .into()
    }

    fn ty<'a>(
        &'a self,
        ty: &'a ast::TsType,
        bindings: &HashMap<&'a str, &'a ast::TsType>,
        depth: usize,
    ) -> Result<Type> {
        if depth > 64 {
            return Err(self.error(
                ty.span(),
                "Recursive or excessively nested boundary type; use a finite data contract",
            ));
        }
        let nested = |ty| self.ty(ty, bindings, depth + 1);
        Ok(match ty {
            ast::TsType::TsKeywordType(keyword) => match keyword.kind {
                ast::TsKeywordTypeKind::TsNumberKeyword => Type::Scalar("f64"),
                ast::TsKeywordTypeKind::TsBigIntKeyword => Type::Scalar("s64"),
                ast::TsKeywordTypeKind::TsBooleanKeyword => Type::Scalar("bool"),
                ast::TsKeywordTypeKind::TsStringKeyword => Type::Scalar("string"),
                ast::TsKeywordTypeKind::TsVoidKeyword => Type::Void,
                _ => {
                    return Err(self.error(
                        ty.span(),
                        "This type has no standalone component representation",
                    ));
                }
            },
            ast::TsType::TsParenthesizedType(parenthesized) => nested(&parenthesized.type_ann)?,
            ast::TsType::TsTypeOperator(operator)
                if operator.op == ast::TsTypeOperatorOp::ReadOnly =>
            {
                nested(&operator.type_ann)?
            }
            ast::TsType::TsArrayType(array) => Type::List(Box::new(nested(&array.elem_type)?)),
            ast::TsType::TsTupleType(tuple) => Type::Tuple(
                tuple
                    .elem_types
                    .iter()
                    .map(|element| nested(&element.ty))
                    .collect::<Result<_>>()?,
            ),
            ast::TsType::TsTypeLit(record) => {
                Type::Record(self.fields(&record.members, bindings, depth + 1)?)
            }
            ast::TsType::TsUnionOrIntersectionType(
                ast::TsUnionOrIntersectionType::TsUnionType(union),
            ) => self.union(&union.types, bindings, depth + 1)?,
            ast::TsType::TsLitType(literal) => match &literal.lit {
                ast::TsLit::Str(value) => {
                    Type::Enum(vec![value.value.to_string_lossy().into_owned()])
                }
                _ => return Err(self.error(
                    ty.span(),
                    "Numeric and boolean literal boundaries need a number or boolean annotation",
                )),
            },
            ast::TsType::TsImportType(import) if import.arg.value == "perry:types" => {
                let Some(ast::TsEntityName::Ident(name)) = &import.qualifier else {
                    return Err(self.error(
                        import.span,
                        "Numeric imports require a named perry:types alias",
                    ));
                };
                Type::Scalar(
                    scalar(&name.sym)
                        .ok_or_else(|| self.error(import.span, "Unknown numeric alias"))?,
                )
            }
            ast::TsType::TsTypeRef(reference) => {
                let ast::TsEntityName::Ident(name) = &reference.type_name else {
                    return Err(self.error(
                        ty.span(),
                        "Use a named type import for a component boundary",
                    ));
                };
                let name = name.sym.as_ref();
                if let Some(ty) = bindings.get(name) {
                    return nested(ty);
                }
                let args = reference
                    .type_params
                    .as_ref()
                    .map_or(&[][..], |parameters| parameters.params.as_slice());
                if let Some(alias) = self.aliases.get(name) {
                    let environment =
                        self.bind(alias.type_params.as_deref(), args, bindings, ty.span())?;
                    self.ty(&alias.type_ann, &environment, depth + 1)?
                } else if let Some(interface) = self.interfaces.get(name) {
                    let environment =
                        self.bind(interface.type_params.as_deref(), args, bindings, ty.span())?;
                    let mut fields = Vec::new();
                    for parent in &interface.extends {
                        let ast::Expr::Ident(parent_name) = &*parent.expr else {
                            return Err(self.error(
                                parent.span,
                                "Interface inheritance needs a named interface",
                            ));
                        };
                        let declaration = self
                            .interfaces
                            .get(parent_name.sym.as_ref())
                            .ok_or_else(|| self.error(parent.span, "Unknown parent interface"))?;
                        let args = parent
                            .type_args
                            .as_ref()
                            .map_or(&[][..], |args| args.params.as_slice());
                        let inherited = self.bind(
                            declaration.type_params.as_deref(),
                            args,
                            &environment,
                            parent.span,
                        )?;
                        ensure!(
                            declaration.extends.is_empty(),
                            "Nested interface inheritance requires a flattened boundary declaration"
                        );
                        fields.extend(self.fields(
                            &declaration.body.body,
                            &inherited,
                            depth + 1,
                        )?);
                    }
                    fields.extend(self.fields(&interface.body.body, &environment, depth + 1)?);
                    Type::Record(fields)
                } else if let Some(primitive) = self.primitives.get(name) {
                    Type::Scalar(primitive)
                } else {
                    match (name, args) {
                        ("Array" | "ReadonlyArray", [element]) => Type::List(Box::new(nested(element)?)),
                        ("Uint8Array", []) => Type::List(Box::new(Type::Scalar("u8"))),
                        ("ReadableStream", [element]) if nested(element)? == Type::List(Box::new(Type::Scalar("u8"))) => Type::Stream,
                        _ => return Err(self.error(ty.span(), format!("Cannot derive WIT for '{name}'; provide a concrete supported boundary type"))),
                    }
                }
            }
            _ => {
                return Err(self.error(
                    ty.span(),
                    "Unsupported component boundary type; provide a concrete data annotation",
                ));
            }
        })
    }

    fn bind<'a>(
        &'a self,
        params: Option<&'a ast::TsTypeParamDecl>,
        args: &'a [Box<ast::TsType>],
        outer: &HashMap<&'a str, &'a ast::TsType>,
        span: Span,
    ) -> Result<HashMap<&'a str, &'a ast::TsType>> {
        let params = params.map_or(&[][..], |params| params.params.as_slice());
        if args.len() > params.len() {
            return Err(self.error(span, "Too many boundary type arguments"));
        }
        let mut bindings = outer.clone();
        for (index, parameter) in params.iter().enumerate() {
            let ty = args
                .get(index)
                .map(Box::as_ref)
                .or(parameter.default.as_deref())
                .ok_or_else(|| {
                    self.error(span, "Boundary types require concrete type arguments")
                })?;
            let ty = if let ast::TsType::TsTypeRef(reference) = ty {
                if let ast::TsEntityName::Ident(name) = &reference.type_name {
                    outer.get(name.sym.as_ref()).copied().unwrap_or(ty)
                } else {
                    ty
                }
            } else {
                ty
            };
            bindings.insert(parameter.name.sym.as_ref(), ty);
        }
        Ok(bindings)
    }

    fn fields<'a>(
        &'a self,
        members: &'a [ast::TsTypeElement],
        bindings: &HashMap<&'a str, &'a ast::TsType>,
        depth: usize,
    ) -> Result<Vec<(String, Type)>> {
        self.properties(members)
            .map(|property| {
                let (name, annotation, optional) = property?;
                let ty = self.ty(annotation, bindings, depth + 1)?;
                Ok((name.to_owned(), if optional { absent(ty) } else { ty }))
            })
            .collect()
    }

    fn properties<'a>(
        &'a self,
        members: &'a [ast::TsTypeElement],
    ) -> impl Iterator<Item = Result<(&'a str, &'a ast::TsType, bool)>> {
        let mut seen = HashSet::new();
        members.iter().map(move |member| {
            let ast::TsTypeElement::TsPropertySignature(property) = member else {
                return Err(self.error(
                    member.span(),
                    "Boundary records contain data properties only",
                ));
            };
            let name = property_name(&property.key).ok_or_else(|| {
                self.error(property.span, "Boundary properties need static names")
            })?;
            ensure!(seen.insert(name), "Duplicate boundary property '{name}'");
            let annotation = property.type_ann.as_ref().ok_or_else(|| {
                self.error(
                    property.span,
                    "Boundary properties require a type annotation",
                )
            })?;
            Ok((name, &*annotation.type_ann, property.optional))
        })
    }

    fn union_members<'a>(
        &'a self,
        ty: &'a ast::TsType,
        bindings: &HashMap<&'a str, &'a ast::TsType>,
        depth: usize,
        members: &mut Vec<&'a ast::TsType>,
    ) -> Result<()> {
        if depth > 64 {
            return Err(self.error(ty.span(), "Recursive boundary union"));
        }
        match ty {
            ast::TsType::TsUnionOrIntersectionType(
                ast::TsUnionOrIntersectionType::TsUnionType(union),
            ) => {
                for ty in &union.types {
                    self.union_members(ty, bindings, depth + 1, members)?;
                }
            }
            ast::TsType::TsParenthesizedType(ty) => {
                self.union_members(&ty.type_ann, bindings, depth + 1, members)?
            }
            ast::TsType::TsTypeRef(reference) if reference.type_params.is_none() => {
                if let ast::TsEntityName::Ident(name) = &reference.type_name {
                    if let Some(bound) = bindings.get(name.sym.as_ref()) {
                        return self.union_members(bound, bindings, depth + 1, members);
                    }
                    if let Some(alias) = self.aliases.get(name.sym.as_ref())
                        && alias.type_params.is_none()
                    {
                        return self.union_members(&alias.type_ann, bindings, depth + 1, members);
                    }
                }
                members.push(ty);
            }
            _ => members.push(ty),
        }
        Ok(())
    }

    fn union<'a>(
        &'a self,
        members: &'a [Box<ast::TsType>],
        bindings: &HashMap<&'a str, &'a ast::TsType>,
        depth: usize,
    ) -> Result<Type> {
        let mut null = false;
        let mut undefined = false;
        let mut values = Vec::new();
        let mut expanded = Vec::new();
        for member in members {
            self.union_members(member, bindings, depth + 1, &mut expanded)?;
        }
        for member in expanded {
            match member {
                ast::TsType::TsKeywordType(keyword)
                    if keyword.kind == ast::TsKeywordTypeKind::TsNullKeyword =>
                {
                    null = true
                }
                ast::TsType::TsKeywordType(keyword)
                    if keyword.kind == ast::TsKeywordTypeKind::TsUndefinedKeyword =>
                {
                    undefined = true
                }
                _ => values.push(member),
            }
        }
        ensure!(
            !values.is_empty(),
            "An absence union needs a concrete value type"
        );
        let value = if values.len() == 1 {
            self.ty(values[0], bindings, depth + 1)?
        } else {
            self.union_values(&values, bindings, depth + 1)?
        };
        let value = if null {
            match value {
                Type::Option(_) | Type::Absent { null: true, .. } => value,
                Type::Absent { value, .. } => Type::Absent { value, null: true },
                value => Type::Option(Box::new(value)),
            }
        } else {
            value
        };
        Ok(if undefined { absent(value) } else { value })
    }

    fn union_values<'a>(
        &'a self,
        values: &[&'a ast::TsType],
        bindings: &HashMap<&'a str, &'a ast::TsType>,
        depth: usize,
    ) -> Result<Type> {
        enum Discriminant<'a> {
            Variant(&'a ast::Str),
            Result(bool),
        }
        let mut cases = Vec::new();
        let mut strings = Vec::new();
        let mut result = [None, None];
        for &member in values {
            if let ast::TsType::TsLitType(literal) = member
                && let ast::TsLit::Str(value) = &literal.lit
            {
                strings.push(value.value.to_string_lossy().into_owned());
                continue;
            }
            let ast::TsType::TsTypeLit(object) = member else {
                return Err(self.error(
                    member.span(),
                    "Unions need string literals or explicit tag/val or ok/value/error cases",
                ));
            };
            let properties = self
                .properties(&object.members)
                .collect::<Result<Vec<_>>>()?;
            let Some(&(name, ast::TsType::TsLitType(literal), false)) = properties
                .iter()
                .find(|(name, _, _)| matches!(*name, "tag" | "ok"))
            else {
                return Err(self.error(
                    member.span(),
                    "Union cases need required literal tag or ok discriminants",
                ));
            };
            let (discriminant, field) = match (name, &literal.lit) {
                ("tag", ast::TsLit::Str(tag)) => (Discriminant::Variant(tag), "val"),
                ("ok", ast::TsLit::Bool(tag)) => (
                    Discriminant::Result(tag.value),
                    if tag.value { "value" } else { "error" },
                ),
                _ => return Err(self.error(literal.span, "Invalid union discriminant")),
            };
            ensure!(
                properties
                    .iter()
                    .all(|(property, _, _)| *property == name || *property == field),
                "Union case requires '{name}' and optional '{field}' properties"
            );
            let payload = properties
                .iter()
                .find(|(name, _, _)| *name == field)
                .map(|(_, ty, optional)| {
                    self.ty(ty, bindings, depth + 1)
                        .map(|ty| if *optional { absent(ty) } else { ty })
                })
                .transpose()?;
            match discriminant {
                Discriminant::Variant(tag) => {
                    cases.push((tag.value.to_string_lossy().into_owned(), payload));
                }
                Discriminant::Result(ok) => {
                    let index = usize::from(!ok);
                    ensure!(result[index].is_none(), "Duplicate result case");
                    result[index] = Some(payload.map(Box::new));
                }
            }
        }
        if strings.len() == values.len() {
            return Ok(Type::Enum(strings));
        }
        if cases.len() == values.len() {
            return Ok(Type::Variant(cases));
        }
        if values.len() == 2
            && let [Some(ok), Some(err)] = result
        {
            return Ok(Type::Result { ok, err });
        }
        bail!("Mixed union representations are not supported at a component boundary")
    }

    fn return_type(
        &self,
        function: &ast::Function,
        parameters: &[(&str, Type)],
        depth: usize,
    ) -> Result<Type> {
        if depth > 64 {
            return Err(self.error(
                function.span,
                "Recursive exported functions require a return annotation",
            ));
        }
        if let Some(annotation) = &function.return_type {
            let mut ty = &*annotation.type_ann;
            if let ast::TsType::TsTypeRef(reference) = ty
                && matches!(&reference.type_name, ast::TsEntityName::Ident(name) if name.sym == "Promise")
                && let Some(args) = &reference.type_params
                && args.params.len() == 1
            {
                ty = &args.params[0];
            }
            return self.ty(ty, &HashMap::new(), 0);
        }
        struct Returns<F>(F);
        impl<F: FnMut(Option<&ast::Expr>)> Visit for Returns<F> {
            fn visit_decl(&mut self, _: &ast::Decl) {}
            fn visit_expr(&mut self, _: &ast::Expr) {}
            fn visit_return_stmt(&mut self, statement: &ast::ReturnStmt) {
                (self.0)(statement.arg.as_deref());
            }
        }
        fn exits(statement: &ast::Stmt) -> bool {
            match statement {
                ast::Stmt::Return(_) | ast::Stmt::Throw(_) => true,
                ast::Stmt::Block(block) => block.stmts.iter().any(exits),
                ast::Stmt::If(branch) => {
                    exits(&branch.cons) && branch.alt.as_deref().is_some_and(exits)
                }
                ast::Stmt::Switch(switch) => {
                    switch.cases.iter().any(|case| case.test.is_none())
                        && switch.cases.iter().all(|case| case.cons.iter().any(exits))
                }
                _ => false,
            }
        }
        let mut result = Ok(None);
        if let Some(body) = &function.body {
            body.visit_with(&mut Returns(|value: Option<&ast::Expr>| {
                if let Ok(previous) = &mut result {
                    let inferred = value
                        .map(|value| self.expression(value, parameters, depth + 1))
                        .transpose()
                        .map(|ty| ty.unwrap_or(Type::Void));
                    match inferred {
                        Ok(ty) if previous.as_ref().is_none_or(|previous| *previous == ty) => {
                            *previous = Some(ty);
                        }
                        Ok(_) => result = Err(self.error(function.span,
                            "Exported function has different return types; add an explicit annotation")),
                        Err(error) => result = Err(error),
                    }
                }
            }));
        }
        let result = result?.unwrap_or(Type::Void);
        if result != Type::Void
            && !function
                .body
                .as_ref()
                .is_some_and(|body| body.stmts.iter().any(exits))
        {
            return Err(self.error(
                function.span,
                "A public function that may fall through needs an explicit return annotation",
            ));
        }
        Ok(result)
    }

    fn expression(
        &self,
        value: &ast::Expr,
        parameters: &[(&str, Type)],
        depth: usize,
    ) -> Result<Type> {
        Ok(match value {
            ast::Expr::Lit(ast::Lit::Num(_)) => Type::Scalar("f64"),
            ast::Expr::Lit(ast::Lit::BigInt(_)) => Type::Scalar("s64"),
            ast::Expr::Lit(ast::Lit::Bool(_)) => Type::Scalar("bool"),
            ast::Expr::Lit(ast::Lit::Str(_)) | ast::Expr::Tpl(_) => Type::Scalar("string"),
            ast::Expr::Ident(name) => parameters
                .iter()
                .find(|(parameter, _)| *parameter == name.sym.as_ref())
                .map(|(_, ty)| ty.clone())
                .ok_or_else(|| {
                    self.error(
                        value.span(),
                        "Add a return annotation to this exported function",
                    )
                })?,
            ast::Expr::Paren(value) => self.expression(&value.expr, parameters, depth + 1)?,
            ast::Expr::Await(value) => self.expression(&value.arg, parameters, depth + 1)?,
            ast::Expr::TsAs(value) => self.ty(&value.type_ann, &HashMap::new(), 0)?,
            ast::Expr::Bin(binary) => match binary.op {
                ast::BinaryOp::EqEq
                | ast::BinaryOp::NotEq
                | ast::BinaryOp::EqEqEq
                | ast::BinaryOp::NotEqEq
                | ast::BinaryOp::Lt
                | ast::BinaryOp::LtEq
                | ast::BinaryOp::Gt
                | ast::BinaryOp::GtEq => Type::Scalar("bool"),
                _ => {
                    let left = self.expression(&binary.left, parameters, depth + 1)?;
                    let right = self.expression(&binary.right, parameters, depth + 1)?;
                    ensure!(
                        left == right,
                        "Add a return annotation for mixed arithmetic"
                    );
                    left
                }
            },
            ast::Expr::Call(call)
                if let ast::Callee::Expr(callee) = &call.callee
                    && let ast::Expr::Ident(name) = &**callee =>
            {
                match name.sym.as_ref() {
                    "String" => Type::Scalar("string"),
                    "Number" => Type::Scalar("f64"),
                    "BigInt" => Type::Scalar("s64"),
                    "Boolean" => Type::Scalar("bool"),
                    name => {
                        let function = self.functions.get(name).ok_or_else(|| {
                            self.error(value.span(), "Add a return annotation for a host call")
                        })?;
                        {
                            let mut arguments = Vec::new();
                            for (param, arg) in function.function.params.iter().zip(&call.args) {
                                let ast::Pat::Ident(binding) = &param.pat else {
                                    return Err(self.error(
                                        value.span(),
                                        "Add a return annotation for this helper call",
                                    ));
                                };
                                let ty = if let Some(annotation) = &binding.type_ann {
                                    self.ty(&annotation.type_ann, &HashMap::new(), 0)?
                                } else {
                                    self.expression(&arg.expr, parameters, depth + 1)?
                                };
                                arguments.push((binding.id.sym.as_ref(), ty));
                            }
                            self.return_type(&function.function, &arguments, depth + 1)?
                        }
                    }
                }
            }
            _ => {
                return Err(self.error(
                    value.span(),
                    "Add a return annotation to this exported function",
                ));
            }
        })
    }

    fn signature<'a>(&self, function: &'a ast::FnDecl) -> Result<Signature<'a>> {
        ensure!(
            function.function.type_params.is_none(),
            "Exported generic functions need a concrete wrapper"
        );
        let params = function
            .function
            .params
            .iter()
            .map(|parameter| {
                let (binding, optional) = match &parameter.pat {
                    ast::Pat::Ident(binding) => (binding, binding.id.optional),
                    ast::Pat::Assign(assign) if let ast::Pat::Ident(binding) = &*assign.left => {
                        (binding, true)
                    }
                    _ => {
                        return Err(self
                            .error(parameter.span, "Boundary parameters need named identifiers"));
                    }
                };
                let annotation = binding.type_ann.as_ref().ok_or_else(|| {
                    self.error(
                        parameter.span,
                        "Exported parameters require type annotations",
                    )
                })?;
                let ty = self.ty(&annotation.type_ann, &HashMap::new(), 0)?;
                Ok((
                    binding.id.sym.as_ref(),
                    if optional { absent(ty) } else { ty },
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let result = self.return_type(&function.function, &params, 0)?;
        Ok(Signature {
            function,
            params,
            result,
        })
    }
}

fn is_async(function: &ast::Function) -> bool {
    function.is_async
        || matches!(function.return_type.as_ref().map(|annotation| &*annotation.type_ann), Some(ast::TsType::TsTypeRef(reference)) if matches!(&reference.type_name, ast::TsEntityName::Ident(name) if name.sym == "Promise"))
}

fn absent(ty: Type) -> Type {
    match ty {
        Type::Absent { .. } => ty,
        Type::Option(value) => Type::Absent { value, null: true },
        value => Type::Absent {
            value: Box::new(value),
            null: false,
        },
    }
}
fn property_name(expression: &ast::Expr) -> Option<&str> {
    match expression {
        ast::Expr::Ident(name) => Some(name.sym.as_ref()),
        ast::Expr::Lit(ast::Lit::Str(name)) => name.value.as_str(),
        _ => None,
    }
}

fn kebab(name: &str) -> String {
    let mut output = String::new();
    for (index, character) in name.chars().enumerate() {
        if character == '_' || character == '-' {
            if !output.is_empty() && !output.ends_with('-') {
                output.push('-');
            }
        } else {
            if index > 0 && character.is_ascii_uppercase() && !output.ends_with('-') {
                output.push('-');
            }
            output.push(character.to_ascii_lowercase());
        }
    }
    output
}

#[derive(Default)]
struct Writer<'a> {
    items: Vec<String>,
    names: HashMap<&'a Type, String>,
    used: HashSet<String>,
    exports: HashSet<&'a str>,
}
impl<'a> Writer<'a> {
    fn function(&mut self, signature: &'a Signature<'_>) -> Result<()> {
        let source_name = signature.function.ident.sym.as_ref();
        let name = kebab(source_name);
        ensure!(
            crate::names::to_camel_case(&name) == source_name && self.exports.insert(source_name),
            "Export '{source_name}' cannot round-trip through a WIT name"
        );
        let params = signature
            .params
            .iter()
            .map(|(param, ty)| {
                let param = kebab(param);
                Ok(format!(
                    "%{param}: {}",
                    self.ty(ty, &format!("{name}-{param}"))?
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let result = if signature.result == Type::Void {
            String::new()
        } else {
            format!(
                " -> {}",
                self.ty(&signature.result, &format!("{name}-result"))?
            )
        };
        self.items.push(format!(
            "export %{name}: {}func({}){result};",
            if is_async(&signature.function.function) {
                "async "
            } else {
                ""
            },
            params.join(", ")
        ));
        Ok(())
    }

    fn ty(&mut self, ty: &'a Type, hint: &str) -> Result<String> {
        if let Some(name) = self.names.get(ty) {
            return Ok(format!("%{name}"));
        }
        let (kind, body) = match ty {
            Type::Scalar(name) => return Ok((*name).into()),
            Type::Void => bail!("void is only valid as a function result"),
            Type::List(value) => (
                "type",
                format!(" = list<{}>;", self.ty(value, &format!("{hint}-item"))?),
            ),
            Type::Option(value) => (
                "type",
                format!(" = option<{}>;", self.ty(value, &format!("{hint}-value"))?),
            ),
            Type::Absent { value, null } => {
                let payload = self.ty(value, &format!("{hint}-value"))?;
                let cases = if *null {
                    "null, undefined"
                } else {
                    "undefined"
                };
                ("variant", format!(" {{ {cases}, value({payload}) }}"))
            }
            Type::Record(fields) => {
                let mut names = HashSet::new();
                let mut members = Vec::new();
                for (field, value) in fields {
                    let field_name = kebab(field);
                    ensure!(
                        crate::names::to_camel_case(&field_name) == *field && names.insert(field),
                        "Property '{field}' cannot round-trip through a WIT name"
                    );
                    members.push(format!(
                        "%{field_name}: {}",
                        self.ty(value, &format!("{hint}-{field_name}"))?
                    ));
                }
                ("record", format!(" {{ {} }}", members.join(", ")))
            }
            Type::Tuple(values) => {
                let items = values
                    .iter()
                    .enumerate()
                    .map(|(i, ty)| self.ty(ty, &format!("{hint}-item-{i}")))
                    .collect::<Result<Vec<_>>>()?;
                ("type", format!(" = tuple<{}>;", items.join(", ")))
            }
            Type::Enum(cases) => (
                "enum",
                format!(
                    " {{ {} }}",
                    cases
                        .iter()
                        .map(|case| format!("%{case}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
            Type::Variant(cases) => {
                let values = cases
                    .iter()
                    .map(|(case, ty)| {
                        Ok(match ty {
                            Some(ty) => {
                                format!("%{case}({})", self.ty(ty, &format!("{hint}-{case}"))?)
                            }
                            None => format!("%{case}"),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                ("variant", format!(" {{ {} }}", values.join(", ")))
            }
            Type::Result { ok, err } => {
                let ok = ok
                    .as_ref()
                    .map(|ty| self.ty(ty, &format!("{hint}-ok")))
                    .transpose()?
                    .unwrap_or("_".into());
                let err = err
                    .as_ref()
                    .map(|ty| self.ty(ty, &format!("{hint}-error")))
                    .transpose()?
                    .unwrap_or("_".into());
                ("type", format!(" = result<{ok}, {err}>;"))
            }
            Type::Stream => ("type", " = stream<u8>;".into()),
        };
        let prefix = match ty {
            Type::Absent { null: false, .. } => "perry-undefined-",
            Type::Absent { null: true, .. } => "perry-nullish-",
            _ => "",
        };
        let base = format!("{prefix}{}", kebab(hint));
        let mut name = base.clone();
        let mut index = 2;
        while !self.used.insert(name.clone()) {
            name = format!("{base}-{index}");
            index += 1;
        }
        self.items.push(format!("{kind} %{name}{body}"));
        self.names.insert(ty, name.clone());
        Ok(format!("%{name}"))
    }
}

pub fn generate_wit(program: &Program, world: Option<&str>) -> Result<Vec<Artifact>> {
    let (source, identity, modules) = perry_source::program(program)?;
    generate_sources(source, identity, &modules, world)
}

pub fn generate_sources(
    source: &str,
    identity: &str,
    modules: &BTreeMap<String, String>,
    world: Option<&str>,
) -> Result<Vec<Artifact>> {
    let loaded = perry_source::load(source, identity, modules, &|_, _, _| {})?;
    perry_source::validate(&loaded.module, &loaded.locations)?;
    let mut declarations = Declarations {
        aliases: HashMap::new(),
        interfaces: HashMap::new(),
        functions: HashMap::new(),
        primitives: HashMap::new(),
        locations: &loaded.locations,
    };
    let mut exports = Vec::new();
    let mut command = false;
    for item in &loaded.module.body {
        let declaration = match item {
            ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportDecl(export)) => {
                match &export.decl {
                    ast::Decl::Fn(function) => exports.push(function),
                    ast::Decl::TsTypeAlias(_) | ast::Decl::TsInterface(_) => {}
                    ast::Decl::Class(class) if class.declare => {}
                    _ => {
                        return Err(declarations.error(
                            export.span,
                            "Component exports require named function declarations",
                        ));
                    }
                }
                Some(&export.decl)
            }
            ast::ModuleItem::Stmt(ast::Stmt::Decl(declaration)) => Some(declaration),
            ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) => {
                if import.src.value == "perry:types" {
                    for specifier in &import.specifiers {
                        if let ast::ImportSpecifier::Named(named) = specifier {
                            let imported = named.imported.as_ref().map(|name| name.atom());
                            let name = imported.as_deref().unwrap_or(&named.local.sym);
                            if let Some(ty) = scalar(name) {
                                declarations.primitives.insert(named.local.sym.as_ref(), ty);
                            }
                        }
                    }
                }
                None
            }
            ast::ModuleItem::ModuleDecl(
                ast::ModuleDecl::ExportDefaultDecl(_)
                | ast::ModuleDecl::ExportDefaultExpr(_)
                | ast::ModuleDecl::TsExportAssignment(_),
            ) => {
                return Err(declarations.error(
                    item.span(),
                    "Component exports require named function declarations",
                ));
            }
            ast::ModuleItem::Stmt(ast::Stmt::Empty(_)) => None,
            ast::ModuleItem::Stmt(_) => {
                command = true;
                None
            }
            _ => None,
        };
        match declaration {
            Some(ast::Decl::TsTypeAlias(alias)) => {
                declarations.aliases.insert(alias.id.sym.as_ref(), alias);
            }
            Some(ast::Decl::TsInterface(interface)) => {
                declarations
                    .interfaces
                    .insert(interface.id.sym.as_ref(), interface);
            }
            Some(ast::Decl::Fn(function)) => {
                declarations
                    .functions
                    .insert(function.ident.sym.as_ref(), function);
            }
            Some(ast::Decl::Var(declaration)) if !declaration.declare => command = true,
            Some(ast::Decl::Class(declaration)) if !declaration.declare => command = true,
            _ => {}
        }
    }
    command |= exports.is_empty();
    let annotate = |function: &ast::FnDecl, error| -> anyhow::Error {
        let mut diagnostic = PerryDiagnostic::from_error(error, DiagnosticCode::UnsupportedType);
        if diagnostic.span.is_none() {
            diagnostic.span = loaded.locations.span(function.function.span);
        }
        diagnostic.into()
    };
    let signatures = exports
        .into_iter()
        .map(|function| {
            declarations
                .signature(function)
                .map_err(|error| annotate(function, error))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut writer = Writer::default();
    for signature in &signatures {
        writer
            .function(signature)
            .map_err(|error| annotate(signature.function, error))?;
    }
    let world = world.unwrap_or("app");
    wit_parser::validate_id(world).context("Invalid generated world name")?;
    let mut output = format!(
        "package local:app;\n\nworld %{world} {{\n  include wasi:cli/{}@0.3.0;\n  import wasi:http/client@0.3.0;\n",
        if command { "command" } else { "imports" }
    );
    for item in writer.items {
        output.push_str("  ");
        output.push_str(&item);
        output.push('\n');
    }
    output.push_str("}\n");
    crate::resolution::resolve_embedded_source("world.wit", &output)?;
    Ok(vec![Artifact {
        name: "world.wit".into(),
        contents: output.into_bytes(),
    }])
}
