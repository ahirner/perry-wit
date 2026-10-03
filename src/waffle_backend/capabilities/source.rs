//! Resolve capability bindings before Perry's name-based builtin lowering.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Result, bail, ensure};
use perry_hir::types::Type as HirType;
use perry_parser::{parse_typescript, swc_ecma_ast as ast};
use swc_common::{GLOBALS, Globals, Mark, SyntaxContext};
use swc_ecma_transforms_base::resolver;
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};

use super::{CapabilityOperation, ClockOperation, LowerCapability, RandomOperation};

pub(crate) fn resolve_capabilities(
    module: &mut ast::Module,
) -> Result<BTreeMap<String, CapabilityOperation>> {
    GLOBALS.set(&Globals::new(), || {
        let unresolved = Mark::new();
        module.visit_mut_with(&mut resolver(unresolved, Mark::new(), true));
        let mut names = IdentifierNames::default();
        module.visit_with(&mut names);
        let mut bindings = HashMap::new();
        for item in &module.body {
            let ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) = item else {
                continue;
            };
            if import.type_only {
                continue;
            }
            let namespace = match import.src.value.as_str() {
                Some("perry:clocks") => CapabilityNamespace::Clocks,
                Some("perry:random") => CapabilityNamespace::Random,
                _ => bail!("Unsupported capability import: {:?}", import.src.value),
            };
            ensure!(
                !import.specifiers.is_empty(),
                "Capability imports cannot run module initialization"
            );
            for specifier in &import.specifiers {
                let binding = match specifier {
                    ast::ImportSpecifier::Named(named) if named.is_type_only => continue,
                    ast::ImportSpecifier::Named(named) => {
                        let name = named.imported.as_ref().map_or_else(
                            || named.local.sym.to_string(),
                            |name| name.atom().to_string(),
                        );
                        CapabilityBinding::Operation(namespace.operation(&name)?)
                    }
                    ast::ImportSpecifier::Namespace(_) => CapabilityBinding::Namespace(namespace),
                    ast::ImportSpecifier::Default(_) => {
                        bail!("Capability modules have no default export")
                    }
                };
                bindings.insert(specifier.local().to_id(), binding);
            }
        }
        module.body.retain(|item| {
            !matches!(
                item,
                ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(_))
            )
        });
        let mut calls = CapabilityCalls {
            bindings,
            unresolved: SyntaxContext::empty().apply_mark(unresolved),
            names: names.0,
            shadow_names: HashMap::new(),
            operations: BTreeMap::new(),
            error: None,
        };
        module.visit_mut_with(&mut calls);
        if let Some(error) = calls.error {
            return Err(error);
        }
        let mut resolved = BTreeMap::new();
        for (operation, name) in calls.operations {
            let plan = operation.lower();
            let parameters = plan
                .params
                .iter()
                .enumerate()
                .map(|(index, ty)| Ok(format!("arg{index}: {}", source_type(ty)?)))
                .collect::<Result<Vec<_>>>()?
                .join(", ");
            let declaration = format!(
                "declare function {name}({parameters}): {};",
                source_type(&plan.result)?
            );
            let mut declaration = parse_typescript(&declaration, "capability.d.ts")?;
            module.body.append(&mut declaration.body);
            resolved.insert(name, operation);
        }
        Ok(resolved)
    })
}

fn source_type(ty: &HirType) -> Result<String> {
    match ty {
        HirType::Number => Ok("number".into()),
        HirType::Boolean => Ok("boolean".into()),
        HirType::String => Ok("string".into()),
        HirType::Void => Ok("void".into()),
        HirType::Promise(inner) => Ok(format!("Promise<{}>", source_type(inner)?)),
        _ => bail!("Unsupported capability source type: {ty:?}"),
    }
}

#[derive(Clone, Copy)]
enum CapabilityNamespace {
    Clocks,
    Random,
}

impl CapabilityNamespace {
    fn operation(self, name: &str) -> Result<CapabilityOperation> {
        match (self, name) {
            (Self::Clocks, "waitFor") => Ok(CapabilityOperation::Clock(ClockOperation::WaitFor)),
            (Self::Random, "randomNumber") => {
                Ok(CapabilityOperation::Random(RandomOperation::Number))
            }
            _ => bail!("Unknown capability member '{name}'"),
        }
    }
}

enum CapabilityBinding {
    Operation(CapabilityOperation),
    Namespace(CapabilityNamespace),
}

struct CapabilityCalls {
    bindings: HashMap<ast::Id, CapabilityBinding>,
    unresolved: SyntaxContext,
    names: HashSet<String>,
    shadow_names: HashMap<ast::Id, String>,
    operations: BTreeMap<CapabilityOperation, String>,
    error: Option<anyhow::Error>,
}

impl CapabilityCalls {
    fn reject_regexp_constructor(&mut self, callee: &ast::Expr) -> bool {
        if matches!(callee, ast::Expr::Ident(name) if name.sym == "RegExp" && name.ctxt == self.unresolved)
        {
            self.error.get_or_insert_with(|| {
                anyhow::anyhow!(
                    "RegExp construction is unsupported; use a regex literal in string.search"
                )
            });
            true
        } else {
            false
        }
    }

    fn operation(&self, expression: &ast::Expr) -> Result<Option<CapabilityOperation>> {
        match expression {
            ast::Expr::Ident(ident) => match self.bindings.get(&ident.to_id()) {
                Some(CapabilityBinding::Operation(operation)) => Ok(Some(*operation)),
                Some(CapabilityBinding::Namespace(_)) => {
                    bail!("Capability namespaces cannot be used as values")
                }
                None => Ok(None),
            },
            ast::Expr::Member(member) => {
                let ast::Expr::Ident(receiver) = member.obj.as_ref() else {
                    return Ok(None);
                };
                let namespace = match self.bindings.get(&receiver.to_id()) {
                    Some(CapabilityBinding::Namespace(namespace)) => Some(*namespace),
                    _ => None,
                };
                let builtin_math = receiver.sym == "Math" && receiver.ctxt == self.unresolved;
                if namespace.is_none() && !builtin_math {
                    return Ok(None);
                }
                let name = match &member.prop {
                    ast::MemberProp::Ident(ident) => ident.sym.as_ref(),
                    ast::MemberProp::Computed(computed) => match computed.expr.as_ref() {
                        ast::Expr::Lit(ast::Lit::Str(text)) => text.value.as_str().unwrap_or(""),
                        _ => bail!("Dynamic capability member lookup is unsupported"),
                    },
                    _ => bail!("Private capability member lookup is unsupported"),
                };
                if let Some(namespace) = namespace {
                    Ok(Some(namespace.operation(name)?))
                } else if name == "random" {
                    Ok(Some(CapabilityOperation::Random(RandomOperation::Number)))
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }

    fn fresh_name(&mut self) -> String {
        let mut index = self.names.len();
        loop {
            let name = format!("__perry_resolved_{index}");
            if self.names.insert(name.clone()) {
                return name;
            }
            index += 1;
        }
    }
}

impl VisitMut for CapabilityCalls {
    fn visit_mut_call_expr(&mut self, call: &mut ast::CallExpr) {
        call.ctxt = SyntaxContext::empty();
        if let ast::Callee::Expr(callee) = &mut call.callee {
            // Perry may fold constructors before argument effects are retained.
            if self.reject_regexp_constructor(callee) {
                return;
            }
            match self.operation(callee) {
                Ok(Some(operation)) => {
                    if call.args.iter().any(|argument| argument.spread.is_some()) {
                        self.error.get_or_insert_with(|| {
                            anyhow::anyhow!("Spread capability arguments are unsupported")
                        });
                        return;
                    }
                    let name = if let Some(name) = self.operations.get(&operation) {
                        name.clone()
                    } else {
                        let name = self.fresh_name();
                        self.operations.insert(operation, name.clone());
                        name
                    };
                    **callee = ast::Expr::Ident(ast::Ident::new(
                        name.into(),
                        call.span,
                        SyntaxContext::empty(),
                    ));
                    call.args.visit_mut_with(self);
                    return;
                }
                Err(error) => {
                    self.error.get_or_insert(error);
                    return;
                }
                Ok(None) => {}
            }
        }
        call.visit_mut_children_with(self);
    }

    fn visit_mut_new_expr(&mut self, expression: &mut ast::NewExpr) {
        if !self.reject_regexp_constructor(&expression.callee) {
            expression.visit_mut_children_with(self);
        }
    }

    fn visit_mut_expr(&mut self, expression: &mut ast::Expr) {
        if !matches!(expression, ast::Expr::Call(_)) {
            match self.operation(expression) {
                Ok(Some(_)) => {
                    self.error.get_or_insert_with(|| {
                        anyhow::anyhow!("Capability functions are only supported as direct calls")
                    });
                }
                Err(error) => {
                    self.error.get_or_insert(error);
                }
                Ok(None) => {}
            }
        }
        expression.visit_mut_children_with(self);
    }

    fn visit_mut_ident(&mut self, ident: &mut ast::Ident) {
        if matches!(ident.sym.as_ref(), "Math" | "RegExp") && ident.ctxt != self.unresolved {
            let id = ident.to_id();
            let name = if let Some(name) = self.shadow_names.get(&id) {
                name.clone()
            } else {
                let name = self.fresh_name();
                self.shadow_names.insert(id, name.clone());
                name
            };
            ident.sym = name.into();
        }
        ident.ctxt = SyntaxContext::empty();
    }

    fn visit_mut_syntax_context(&mut self, context: &mut SyntaxContext) {
        *context = SyntaxContext::empty();
    }
}

#[derive(Default)]
struct IdentifierNames(HashSet<String>);

impl Visit for IdentifierNames {
    fn visit_ident(&mut self, ident: &ast::Ident) {
        self.0.insert(ident.sym.to_string());
    }
}
