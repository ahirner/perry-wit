//! The boundary for source facts and compiler-generated forms that Perry HIR erases.
use crate::{LowerError, LowerErrorKind, modules};
use anyhow::{Context as _, Result, anyhow, bail};
use perry_hir::{
    ir::{CompareOp, Expr, Module as HirModule, Stmt},
    types::{LocalId, Type as HirType},
};
use std::collections::{HashMap, HashSet};
use swc_common::{GLOBALS, Globals, Mark, SyntaxContext};
use swc_ecma_transforms_base::resolver;
use swc_ecma_visit::VisitMutWith;

mod calls;
mod classes;
mod constructors;
pub use calls::{RuntimeCall, runtime_call};
mod defaults;
pub use defaults::strip_synthetic_default_guards;
pub mod locations;
mod methods;
mod parser;
pub use parser::parse;
mod iteration;
mod regex;
pub use iteration::{Availability, Iteration, RecordExpr};
pub mod source;
mod streams;
pub use constructors::{Constructor, Constructors};
pub use methods::{Method, method};

pub struct Metadata<'s> {
    pub(crate) unannotated_bindings: HashSet<LocalId>,
    pub(crate) open_types: crate::open_types::OpenTypes,
    pub locations: locations::Locations<'s>,
    pub inferred_bindings: HashSet<LocalId>,
    pub constructors: Constructors,
}

impl Metadata<'_> {
    pub fn binding_type<'t>(&self, id: LocalId, ty: &'t HirType) -> crate::TypeState<'t> {
        self.open_types.binding(id, ty)
    }
    pub fn parameter_type<'t>(&self, id: LocalId, ty: &'t HirType) -> crate::TypeState<'t> {
        self.open_types.parameter(id, ty)
    }
    pub fn return_type<'t>(
        &self,
        function: &perry_hir::ir::Function,
        ty: &'t HirType,
    ) -> crate::TypeState<'t> {
        self.open_types.result(function, ty)
    }
}

pub(crate) struct LoweredHir<'s> {
    pub module: HirModule,
    pub metadata: Metadata<'s>,
}

pub(crate) fn lower<'s>(loaded: modules::Loaded<'s>, identity: &str) -> Result<LoweredHir<'s>> {
    let mut ast = loaded.module;
    let mut locations = loaded.locations;
    locations.collect(&ast);
    let spans = source::validate(&ast, &locations)?;
    let unannotated_spans = source::unannotated_bindings(&ast);
    let mut constructors = GLOBALS.set(&Globals::new(), || {
        let unresolved = Mark::new();
        ast.visit_mut_with(&mut resolver(unresolved, Mark::new(), true));
        let unresolved = SyntaxContext::empty().apply_mark(unresolved);
        regex::fold(&mut ast, unresolved);
        let constructors = constructors::preserve(&mut ast, unresolved)?;
        streams::preserve_options(&mut ast, unresolved)?;
        Ok::<_, anyhow::Error>(constructors)
    })?;
    streams::lower(&mut ast)?;
    methods::preserve_arguments(&mut ast);
    classes::preserve_field_types(&mut ast);
    locations.preserve_operators(&mut ast);
    let mut module = perry_hir::lower_module(&ast, "main", identity).map_err(|error| {
        let span = error
            .downcast_ref::<perry_hir::error::LowerError>()
            .and_then(|error| error.span);
        let diagnostic = LowerError::new(LowerErrorKind::UnsupportedExpression(format!(
            "failed to lower {identity}: {error:#}"
        )));
        let error = diagnostic.into();
        match span {
            Some(span) => locations.annotate(error, span),
            None => error,
        }
    })?;
    constructors.collect_types(&mut module)?;
    let inferred_bindings = module
        .local_source_spans
        .iter()
        .filter_map(|(id, span)| spans.contains(&(span.start, span.end)).then_some(*id))
        .collect();
    let unannotated_bindings = module
        .local_source_spans
        .iter()
        .filter_map(|(id, span)| {
            unannotated_spans
                .contains(&(span.start, span.end))
                .then_some(*id)
        })
        .collect();
    Ok(LoweredHir {
        module,
        metadata: Metadata {
            unannotated_bindings,
            open_types: crate::open_types::OpenTypes::default(),
            locations,
            inferred_bindings,
            constructors,
        },
    })
}

pub fn is_object_literal(name: &str) -> bool {
    name.starts_with("__AnonShape_")
}

pub fn is_import_initializer(callee: &Expr) -> bool {
    matches!(callee, Expr::ExternFuncRef { name, .. } if name == "js_native_module_named_esm_export_value")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_types_preserve_source_annotation_origins() {
        let source = r#"
export function inspect(value:any):number {
    let open;
    const dynamic:any = value;
    return 1;
}
class Reader {
    open(value) { return value; }
    dynamic(value:any):any { return value; }
}
"#;
        let modules = std::collections::BTreeMap::new();
        let foreign = HashMap::new();
        let program = crate::load_sources(
            &mut pliron::context::Context::new(),
            source,
            "origins.ts",
            &modules,
            &foreign,
        )
        .unwrap();
        let function = program
            .module()
            .functions
            .iter()
            .find(|function| function.name == "inspect")
            .unwrap();
        let metadata = program.metadata();
        let mut checked_bindings = 0;
        for statement in &function.body {
            if let Stmt::Let { id, name, ty, .. } = statement {
                match name.as_str() {
                    "open" => {
                        checked_bindings += 1;
                        assert_eq!(metadata.binding_type(*id, ty), crate::TypeState::Open);
                    }
                    "dynamic" => {
                        checked_bindings += 1;
                        assert_eq!(metadata.binding_type(*id, ty), crate::TypeState::Dynamic)
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(checked_bindings, 2);
        let class = program
            .module()
            .classes
            .iter()
            .find(|class| class.name == "Reader")
            .unwrap();
        for method in &class.methods {
            let state = if method.name == "open" {
                crate::TypeState::Open
            } else {
                crate::TypeState::Dynamic
            };
            assert_eq!(
                metadata.parameter_type(method.params[0].id, &method.params[0].ty),
                state
            );
            assert_eq!(metadata.return_type(method, &method.return_type), state);
        }
    }

    #[test]
    fn constructor_preservation_respects_lexical_bindings() {
        let source = r#"
class Map<T> { constructor(value:T) {} }
function custom() { return new Map<number>(7); }
function shadow(Set:any) { return new Set<number>(9); }
function builtin() { return new Set<string>(); }
"#;
        let modules = std::collections::BTreeMap::new();
        let loaded = modules::load(source, "shadow.ts", &modules, &HashMap::new()).unwrap();
        let lowered = lower(loaded, "shadow.ts").unwrap();
        for function in &lowered.module.functions {
            let Stmt::Return(Some(expr)) = &function.body[0] else {
                panic!("expected constructor return");
            };
            if function.name == "builtin" {
                assert!(matches!(
                    lowered.metadata.constructors.get(expr),
                    Some(Constructor::Set {
                        element: Some(HirType::String),
                        init: None
                    })
                ));
            } else {
                assert!(lowered.metadata.constructors.get(expr).is_none());
                let args = match expr {
                    Expr::New { args, .. } | Expr::NewDynamic { args, .. } => args,
                    other => panic!("unexpected shadowed constructor: {other:?}"),
                };
                assert!(matches!(
                    args.as_slice(),
                    [Expr::Number(7.0 | 9.0)] | [Expr::Integer(7 | 9)]
                ));
            }
        }
    }
}

/// Source fields carried by a synthesized object-literal class.
pub fn object_literal_shapes(module: &HirModule) -> HashMap<&str, Vec<&str>> {
    module
        .classes
        .iter()
        .filter(|class| is_object_literal(&class.name))
        .map(|class| {
            (
                class.name.as_str(),
                class
                    .fields
                    .iter()
                    .map(|field| field.name.as_str())
                    .collect(),
            )
        })
        .collect()
}

pub fn describe_expression(expression: &Expr) -> String {
    format!("{expression:?}")
}
pub fn describe_statement(statement: &Stmt) -> String {
    format!("{statement:?}")
}

/// Object fields retained across Perry's duplicate-key and literal-class desugarings.
pub fn object_literal_pairs<'s, F: FnOnce(&str) -> Option<&'s [&'s str]>>(
    expr: &'s Expr,
    literal_fields: F,
) -> Option<impl Iterator<Item = (&'s str, &'s Expr)> + use<'s, F>> {
    let (fields, defined, names, args) = match expr {
        Expr::Object(fields) => (&fields[..], &[][..], &[][..], &[][..]),
        Expr::ObjectSpread { parts } if parts.iter().all(|(key, _)| key.is_some()) => {
            (&[][..], &parts[..], &[][..], &[][..])
        }
        Expr::New {
            class_name, args, ..
        } => (&[][..], &[][..], literal_fields(class_name)?, &args[..]),
        _ => return None,
    };
    Some(
        fields
            .iter()
            .map(|(name, value)| (name.as_str(), value))
            .chain(
                defined
                    .iter()
                    .filter_map(|(name, value)| Some((name.as_deref()?, value))),
            )
            .chain(names.iter().copied().zip(args)),
    )
}

#[derive(Clone, Copy)]
pub enum ErrorClass {
    Any,
    Type,
    Range,
    Reference,
    Syntax,
}
pub fn error_class(code: u32) -> Result<ErrorClass> {
    Ok(match code {
        0 => ErrorClass::Any,
        1 => ErrorClass::Type,
        2 => ErrorClass::Range,
        3 => ErrorClass::Reference,
        4 => ErrorClass::Syntax,
        _ => bail!("unsupported Error constructor kind {code}"),
    })
}

pub fn native_member_is_property(
    receiver: crate::methods::Receiver,
    name: &str,
    arguments: &[Expr],
) -> bool {
    arguments.is_empty()
        && crate::methods::Method::lookup(receiver, name)
            .is_some_and(crate::methods::Method::is_property)
}

mod objects;
pub use objects::init_fields;
