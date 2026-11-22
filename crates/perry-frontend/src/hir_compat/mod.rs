//! The boundary for source facts and compiler-generated forms that Perry HIR erases.
use crate::modules;
use perry_hir::{
    ir::{CompareOp, Expr, Module as HirModule, Stmt},
    types::{LocalId, Type as HirType},
};
use perry_wit_diagnostics::Result;
use perry_wit_diagnostics::{Diagnostic, bail_internal, codes};
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
mod locations;
pub(crate) use locations::display_name;
pub use locations::{Locations, non_null_assertion};
mod methods;
mod names;
mod parser;
pub(crate) use parser::parse;
mod iteration;
mod regex;
pub use iteration::{Availability, Iteration, RecordExpr};
mod source;
pub(crate) use source::{pattern_bindings, validate};
mod streams;
pub use constructors::{Constructor, Constructors};
pub use methods::{Method, method};

pub struct Metadata<'s> {
    pub locations: Locations<'s>,
    pub inferred_bindings: HashSet<LocalId>,
    /// Number of writes to each binding declared without an initializer.
    pub deferred_writes: HashMap<LocalId, usize>,
    pub constructors: Constructors,
    pub sparse_arrays: bool,
}

pub(crate) struct LoweredHir<'s> {
    pub module: HirModule,
    pub metadata: Metadata<'s>,
}

pub(crate) fn lower<'s>(loaded: modules::Loaded<'s>, identity: &str) -> Result<LoweredHir<'s>> {
    let mut ast = loaded.module;
    let mut sparse = SparseArrays(false);
    swc_ecma_visit::VisitWith::visit_with(&ast, &mut sparse);
    let mut locations = loaded.locations;
    locations.collect(&ast);
    let spans = source::validate(&ast, &locations)?;
    let mut constructors = GLOBALS.set(&Globals::new(), || {
        let unresolved = Mark::new();
        ast.visit_mut_with(&mut resolver(unresolved, Mark::new(), true));
        let unresolved = SyntaxContext::empty().apply_mark(unresolved);
        regex::fold(&mut ast, unresolved);
        let constructors = constructors::preserve(&mut ast, unresolved)?;
        streams::preserve_options(&mut ast, unresolved)?;
        Ok::<_, Diagnostic>(constructors)
    })?;
    let mut names = names::FreshNames::new(&ast);
    streams::lower(&mut ast, &mut names)?;
    methods::preserve_arguments(&mut ast, &mut names);
    classes::preserve_field_types(&mut ast);
    locations.preserve_expressions(&mut ast);
    let mut module = perry_hir::lower_module(&ast, "main", identity).map_err(|error| {
        let lower_error = error.downcast_ref::<perry_hir::error::LowerError>();
        let span = lower_error.and_then(|error| error.span);
        let detail = format!("{error:#}");
        let code = if lower_error.is_some() {
            codes::UNSUPPORTED_CONSTRUCT
        } else {
            classify_hir_error(&detail)
        };
        let diagnostic = Diagnostic::new(code, format!("failed to lower {identity}: {detail}"));
        match span {
            Some(span) => locations.annotate(diagnostic, span),
            None => diagnostic,
        }
    })?;
    constructors.collect_types(&mut module)?;
    let inferred_bindings = module
        .local_source_spans
        .iter()
        .filter_map(|(id, span)| {
            spans
                .inferred
                .contains(&(span.start, span.end))
                .then_some(*id)
        })
        .collect();
    let mut deferred_writes: HashMap<_, _> = module
        .local_source_spans
        .iter()
        .filter_map(|(id, span)| spans.deferred.contains(&span.start).then_some((*id, 0)))
        .collect();
    if !deferred_writes.is_empty() {
        perry_hir::tdz_check::for_each_module_expr(&module, &mut |expr| {
            if let Expr::LocalSet(id, _) | Expr::Update { id, .. } = expr
                && let Some(writes) = deferred_writes.get_mut(id)
            {
                *writes += 1;
            }
        });
    }
    Ok(LoweredHir {
        module,
        metadata: Metadata {
            locations,
            inferred_bindings,
            deferred_writes,
            constructors,
            sparse_arrays: sparse.0,
        },
    })
}

struct SparseArrays(bool);

fn classify_hir_error(message: &str) -> perry_wit_diagnostics::Code {
    if message.starts_with("SyntaxError:") {
        codes::PARSE_ERROR
    } else if message.starts_with("for-of pattern head not pre-resolved")
        || message.starts_with("lower_body_stmt: unhandled statement variant")
        || message.starts_with("__perry_declared_keys")
    {
        codes::INTERNAL
    } else if (message.contains(" requires ")
        && (message.contains("argument") || message.contains("initializer")))
        || message.starts_with("Assignment to undeclared variable")
        || message.starts_with("Invalid assignment target pattern")
        || message.starts_with("Invalid binding pattern")
        || message.starts_with("Rest pattern outside")
        || message.starts_with("Update expression only supports")
        || message.starts_with("Private name brand check is only supported")
    {
        codes::TYPE_MISMATCH
    } else {
        codes::UNSUPPORTED_CONSTRUCT
    }
}

impl swc_ecma_visit::Visit for SparseArrays {
    fn visit_array_lit(&mut self, array: &perry_parser::swc_ecma_ast::ArrayLit) {
        self.0 |= array.elems.iter().any(Option::is_none);
        swc_ecma_visit::VisitWith::visit_children_with(array, self);
    }
}

pub(crate) fn is_object_literal(name: &str) -> bool {
    name.starts_with("__AnonShape_")
}

pub fn is_import_initializer(callee: &Expr) -> bool {
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

    #[test]
    fn generated_bindings_avoid_source_names() {
        // Each source binds the name the first generated binding would take without a check.
        for source in [
            r#"function f(): string {
                const __perry_arr_rest_1_ = "user";
                const [first, ...others] = [1, 2].map((x) => x);
                return __perry_arr_rest_1_;
            }"#,
            r#"function f(): string {
                const __perry_loop_1_items = "user";
                for (const x of [1, 2]) {}
                return __perry_loop_1_items;
            }"#,
        ] {
            let modules = std::collections::BTreeMap::new();
            let loaded = modules::load(source, "names.ts", &modules, &HashMap::new()).unwrap();
            let lowered = lower(loaded, "names.ts").unwrap();
            let function = &lowered.module.functions[0];
            let user = function.body.iter().find_map(|stmt| match stmt {
                Stmt::Let {
                    id,
                    init: Some(Expr::String(value)),
                    ..
                } if value == "user" => Some(*id),
                _ => None,
            });
            let returned = function.body.iter().find_map(|stmt| match stmt {
                Stmt::Return(Some(Expr::LocalGet(id))) => Some(*id),
                _ => None,
            });
            assert!(user.is_some(), "no user binding in {:?}", function.body);
            assert_eq!(returned, user, "{source}");
        }
    }
}

/// A key known without executing a computed property expression.
pub fn static_property(part: &perry_hir::ir::ObjectPart) -> Option<&str> {
    use perry_hir::ir::ObjectPart;
    match part {
        ObjectPart::Define(name, _) => Some(name),
        ObjectPart::Computed { key, .. } => match &**key {
            Expr::String(name) => Some(name),
            _ => None,
        },
        ObjectPart::Spread(_) => None,
    }
}

/// Object fields retained across Perry's duplicate-key and literal-class desugarings.
pub fn object_literal_pairs<'s, F: FnOnce(&str) -> Option<&'s [&'s str]>>(
    expr: &'s Expr,
    literal_fields: F,
) -> Option<impl Iterator<Item = (&'s str, &'s Expr)> + use<'s, F>> {
    let (fields, defined, names, args) = match expr {
        Expr::Object(fields) => (&fields[..], &[][..], &[][..], &[][..]),
        Expr::ObjectSpread { parts }
            if parts.iter().all(|part| static_property(part).is_some()) =>
        {
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
                    .filter_map(|part| Some((static_property(part)?, part.value()))),
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
        _ => bail_internal!("unsupported Error constructor kind {code}"),
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
