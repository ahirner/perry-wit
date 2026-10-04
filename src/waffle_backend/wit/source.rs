//! Resolve WIT module imports by binding identity before Perry erases import syntax.

use super::*;
use perry_parser::parse_typescript;
use swc_common::{Mark, SyntaxContext};
use swc_ecma_transforms_base::resolver;
use swc_ecma_visit::{VisitMut, VisitMutWith};

impl WitWorld {
    pub(in crate::waffle_backend) fn bind_source(
        &self,
        module: &mut ast::Module,
        unresolved: Mark,
    ) -> Result<BTreeMap<String, String>> {
        let mut functions = BTreeMap::new();
        let mut namespaces = BTreeMap::new();
        let mut declarations = String::new();
        let mut remove = Vec::new();
        for (index, item) in module.body.iter().enumerate() {
            let ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) = item else {
                continue;
            };
            let Some(name) = import.src.value.as_str() else {
                continue;
            };
            let world = &self.resolve.worlds[self.world];
            let interface = world
                .imports
                .iter()
                .chain(&world.exports)
                .find_map(|(key, item)| match item {
                    WorldItem::Interface { id, .. } if self.resolve.name_world_key(key) == name => {
                        Some(&self.resolve.interfaces[*id])
                    }
                    _ => None,
                })
                .or_else(|| {
                    self.resolve.interfaces.iter().find_map(|(id, interface)| {
                        (self.resolve.id_of(id).as_deref() == Some(name)).then_some(interface)
                    })
                });
            let Some(interface) = interface else { continue };
            ensure!(
                !import.specifiers.is_empty(),
                "WIT imports cannot run module initialization"
            );
            for specifier in &import.specifiers {
                match specifier {
                    ast::ImportSpecifier::Named(named) => {
                        let original = named.imported.as_ref().map_or_else(
                            || named.local.sym.to_string(),
                            |name| name.atom().to_string(),
                        );
                        if import.type_only || named.is_type_only {
                            let ty = interface
                                .types
                                .iter()
                                .find_map(|(name, id)| {
                                    (crate::sdk::codegen::to_pascal_case(name) == original)
                                        .then_some(*id)
                                })
                                .with_context(|| {
                                    format!("Unknown WIT type '{original}' in '{name}'")
                                })?;
                            declarations.push_str(&format!(
                                "type {} = {};\n",
                                named.local.sym,
                                source_type(&hir_type(&self.resolve, Type::Id(ty))?)?
                            ));
                        } else {
                            let function = interface
                                .functions
                                .values()
                                .find(|function| to_camel_case(&function.name) == original)
                                .with_context(|| {
                                    format!("Unknown WIT import '{original}' in '{name}'")
                                })?;
                            let key = format!("{name}#{}", function.name);
                            ensure!(
                                self.imports.contains_key(&key),
                                "Interface '{name}' is not imported by the WIT world"
                            );
                            functions.insert(named.local.to_id(), key);
                        }
                    }
                    ast::ImportSpecifier::Namespace(namespace) => {
                        ensure!(!import.type_only, "Import WIT types by name");
                        ensure!(
                            world
                                .imports
                                .iter()
                                .any(|(key, _)| self.resolve.name_world_key(key) == name),
                            "Interface '{name}' is not imported by the WIT world"
                        );
                        namespaces.insert(namespace.local.to_id(), name.to_string());
                    }
                    ast::ImportSpecifier::Default(_) => {
                        bail!("WIT interfaces have no default export")
                    }
                }
            }
            remove.push(index);
        }
        let mut calls = Calls {
            functions,
            namespaces,
            imports: &self.imports,
            used: BTreeMap::new(),
            error: None,
        };
        module.visit_mut_with(&mut calls);
        if let Some(error) = calls.error {
            return Err(error);
        }
        for (symbol, key) in &calls.used {
            let function = &self.imports[key].function;
            ensure!(
                function.kind == FunctionKind::Freestanding,
                "WIT imports currently require synchronous freestanding functions"
            );
            let params = function
                .params
                .iter()
                .enumerate()
                .map(|(index, param)| {
                    Ok(format!(
                        "arg{index}: {}",
                        source_type(&hir_type(&self.resolve, param.ty)?)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?
                .join(",");
            let result = function
                .result
                .map(|ty| hir_type(&self.resolve, ty))
                .transpose()?
                .unwrap_or(HirType::Void);
            declarations.push_str(&format!(
                "declare function {symbol}({params}): {};\n",
                source_type(&result)?
            ));
        }
        for index in remove.into_iter().rev() {
            module.body.remove(index);
        }
        let mut generated = parse_typescript(&declarations, "wit-imports.d.ts")?;
        generated.visit_mut_with(&mut resolver(unresolved, Mark::new(), true));
        module.body.append(&mut generated.body);
        Ok(calls.used)
    }
}

struct Calls<'a> {
    functions: BTreeMap<ast::Id, String>,
    namespaces: BTreeMap<ast::Id, String>,
    imports: &'a BTreeMap<String, WitImport>,
    used: BTreeMap<String, String>,
    error: Option<anyhow::Error>,
}
impl Calls<'_> {
    fn import(&self, expression: &ast::Expr) -> Result<Option<String>> {
        match expression {
            ast::Expr::Ident(name) => {
                ensure!(
                    !self.namespaces.contains_key(&name.to_id()),
                    "WIT namespaces can only be used for direct member calls"
                );
                Ok(self.functions.get(&name.to_id()).cloned())
            }
            ast::Expr::Member(member) => {
                let ast::Expr::Ident(namespace) = member.obj.as_ref() else {
                    return Ok(None);
                };
                let Some(namespace) = self.namespaces.get(&namespace.to_id()) else {
                    return Ok(None);
                };
                let member = match &member.prop {
                    ast::MemberProp::Ident(name) => name.sym.as_ref(),
                    ast::MemberProp::Computed(computed) => match computed.expr.as_ref() {
                        ast::Expr::Lit(ast::Lit::Str(name)) => name.value.as_str().unwrap_or(""),
                        _ => bail!("WIT calls require a static member name"),
                    },
                    _ => bail!("WIT interfaces have no private members"),
                };
                let key = self
                    .imports
                    .iter()
                    .find_map(|(key, import)| {
                        (import.module == *namespace
                            && to_camel_case(&import.function.name) == member)
                            .then_some(key.clone())
                    })
                    .with_context(|| format!("Unknown WIT function '{namespace}.{member}'"))?;
                Ok(Some(key))
            }
            _ => Ok(None),
        }
    }
}
impl VisitMut for Calls<'_> {
    fn visit_mut_call_expr(&mut self, call: &mut ast::CallExpr) {
        if let ast::Callee::Expr(callee) = &call.callee {
            match self.import(callee) {
                Ok(Some(key)) => {
                    if call.args.iter().any(|arg| arg.spread.is_some()) {
                        self.error = Some(anyhow::anyhow!("Spread WIT arguments are unsupported"));
                        return;
                    }
                    let symbol = format!(
                        "__perry_wit_import_{}",
                        self.imports
                            .keys()
                            .position(|candidate| *candidate == key)
                            .unwrap()
                    );
                    self.used.insert(symbol.clone(), key);
                    call.callee = ast::Callee::Expr(Box::new(ast::Expr::Ident(ast::Ident::new(
                        symbol.into(),
                        call.span,
                        SyntaxContext::empty(),
                    ))));
                    call.args.visit_mut_with(self);
                    return;
                }
                Err(error) => {
                    self.error = Some(error);
                    return;
                }
                _ => {}
            }
        }
        call.visit_mut_children_with(self);
    }
    fn visit_mut_expr(&mut self, expression: &mut ast::Expr) {
        if !matches!(expression, ast::Expr::Call(_)) {
            match self.import(expression) {
                Ok(Some(_)) => {
                    self.error = Some(anyhow::anyhow!(
                        "WIT imports can only be used as direct calls"
                    ));
                    return;
                }
                Err(error) => {
                    self.error = Some(error);
                    return;
                }
                _ => {}
            }
        }
        expression.visit_mut_children_with(self);
    }
}

fn source_type(ty: &HirType) -> Result<String> {
    Ok(match ty {
        HirType::Void => "undefined".into(),
        HirType::Null => "null".into(),
        HirType::Boolean => "boolean".into(),
        HirType::Number => "number".into(),
        HirType::BigInt => "bigint".into(),
        HirType::String => "string".into(),
        HirType::Array(inner) => format!("({})[]", source_type(inner)?),
        HirType::Named(name) if name == "Uint8Array" => name.clone(),
        HirType::StringLiteral(value) => serde_json::to_string(value)?,
        HirType::Tuple(types) => format!(
            "[{}]",
            types
                .iter()
                .map(source_type)
                .collect::<Result<Vec<_>>>()?
                .join(",")
        ),
        HirType::Union(types) => format!(
            "({})",
            types
                .iter()
                .map(source_type)
                .collect::<Result<Vec<_>>>()?
                .join("|")
        ),
        HirType::Object(record) => {
            let mut fields = record
                .properties
                .iter()
                .map(|(name, field)| {
                    Ok(format!(
                        "{name}{}:{}",
                        if field.optional { "?" } else { "" },
                        source_type(&field.ty)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            fields.sort();
            format!("{{{}}}", fields.join(";"))
        }
        other => bail!("Unsupported WIT source type: {other:?}"),
    })
}
