//! The boundary for source facts and compiler-generated forms that Perry HIR erases.
use crate::*;
use swc_common::{GLOBALS, Globals, Mark, SyntaxContext};
use swc_ecma_transforms_base::resolver;
use swc_ecma_visit::VisitMutWith;

mod calls;
mod classes;
mod constructors;
pub(crate) use calls::{RuntimeCall, runtime_call};
mod defaults;
pub(super) use defaults::strip_synthetic_default_guards;
pub(crate) mod locations;
mod methods;
mod parser;
pub(crate) use parser::parse;
mod iteration;
pub(crate) use iteration::{Availability, Iteration, RecordExpr};
pub(crate) mod source;
mod streams;
pub(crate) use constructors::{Constructor, Constructors};
pub(crate) use methods::{Method, method};

pub(crate) struct Metadata<'s> {
    pub(crate) locations: locations::Locations<'s>,
    pub(crate) inferred_bindings: HashSet<LocalId>,
    pub(crate) constructors: Constructors,
}

pub(crate) struct LoweredHir<'s> {
    pub(crate) module: HirModule,
    pub(crate) metadata: Metadata<'s>,
}

pub(crate) fn lower<'s>(loaded: modules::Loaded<'s>, identity: &str) -> Result<LoweredHir<'s>> {
    let mut ast = loaded.module;
    let mut locations = loaded.locations;
    locations.collect(&ast);
    let spans = source::validate(&ast, &locations)?;
    let mut constructors = GLOBALS.set(&Globals::new(), || {
        let unresolved = Mark::new();
        ast.visit_mut_with(&mut resolver(unresolved, Mark::new(), true));
        let unresolved = SyntaxContext::empty().apply_mark(unresolved);
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
        let diagnostic = PerryDiagnostic::error(
            DiagnosticCode::UnsupportedExpression,
            format!("failed to lower {identity}: {error:#}"),
        );
        let error = error.context(diagnostic);
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
    Ok(LoweredHir {
        module,
        metadata: Metadata {
            locations,
            inferred_bindings,
            constructors,
        },
    })
}

pub(crate) fn is_object_literal(name: &str) -> bool {
    name.starts_with("__AnonShape_")
}

pub(crate) fn is_import_initializer(callee: &Expr) -> bool {
    matches!(callee, Expr::ExternFuncRef { name, .. } if name == "js_native_module_named_esm_export_value")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_preservation_respects_lexical_bindings() {
        let source = r#"
class Map<T> { constructor(value:T) {} }
function custom() { return new Map<number>(7); }
function shadow(Set:any) { return new Set<number>(9); }
function builtin() { return new Set<string>(); }
"#;
        let modules = BTreeMap::new();
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
