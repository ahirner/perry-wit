//! The boundary for source facts and compiler-generated forms that Perry HIR erases.
use crate::modules;
use perry_hir::{
    ir::{CompareOp, Expr, Stmt},
    types::LocalId,
};
use perry_wit_diagnostics::Result;
use perry_wit_diagnostics::{Diagnostic, bail_internal, codes};
#[cfg(test)]
use std::collections::HashMap;
use swc_common::{GLOBALS, Globals, Mark, SyntaxContext};
use swc_ecma_transforms_base::resolver;
use swc_ecma_visit::VisitMutWith;

mod calls;
mod classes;
pub(crate) use calls::{
    MemberKey, RuntimeCall, call_arguments, class_receiver, map_group_by, member,
    member_assignment, runtime_call,
};
pub(crate) use classes::{direct_constructor, private_field};
mod locations;
pub(crate) use calls::{is_array_insertion, is_source_extern};
pub(crate) use iteration::is_object_coercibility_check;
pub(crate) use locations::{Locations, non_null_assertion};
pub(crate) use locations::{display_name, function_label, has_source_offset};
mod iteration;
mod methods;
pub(crate) use iteration::{Availability, Binding, Locals, RecordExpr};
mod streams;
mod timers;
mod witnesses;
pub(crate) use methods::Preservation;
pub(crate) use methods::{Method, method};
pub(crate) use streams::is_writable_to_web;
pub(crate) use timers::is_timer_module;
pub(crate) use witnesses::{
    ExportDeclaration, ResourceClass, ResourceClasses, SourceWitness, Target, Witnesses,
    adapter_name,
};

pub(crate) fn lower<'s, 'r>(
    loaded: modules::Loaded<'s>,
    identity: &str,
    world: Option<(&'r wit_parser::Resolve, wit_parser::WorldId)>,
) -> Result<(crate::source::SourceModule<'s>, Vec<SourceWitness<'r>>)> {
    let mut ast = loaded.module;
    let mut locations = loaded.locations;
    let (witnesses, top_level) = GLOBALS.set(&Globals::new(), || {
        let unresolved = Mark::new();
        let top_level = Mark::new();
        ast.visit_mut_with(&mut resolver(unresolved, top_level, true));
        let unresolved = SyntaxContext::empty().apply_mark(unresolved);
        locations.collect(&ast);
        timers::check_imports(&ast, &locations)?;
        crate::source::Constructors::bind(&mut ast, unresolved, &locations)?;
        streams::preserve_options(&mut ast, unresolved, &locations)?;
        Ok::<_, Diagnostic>((
            witnesses::resolve(&ast, &loaded.exports, world, &mut locations, unresolved)?,
            SyntaxContext::empty().apply_mark(top_level),
        ))
    })?;
    let mut analysis = crate::source::analyze(&mut ast, &mut locations, top_level);
    streams::lower(&mut ast, &mut analysis)?;
    methods::preserve_arguments(&mut ast, &mut analysis);
    classes::preserve_field_types(&mut ast);
    perry_hir::ir::set_current_module_source(locations.module_text());
    let result = perry_hir::lower_module(&ast, "main", identity);
    perry_hir::ir::clear_current_module_source();
    let mut module = result.map_err(|error| {
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
    for class in &mut module.classes {
        for field in &mut class.fields {
            if private_field(field) {
                field.name = perry_hir::private_names::private_value_key(class.id, &field.name);
            }
        }
    }
    let witnesses = witnesses.in_hir(&mut module, &mut locations)?;
    for (&id, text) in &module.closure_source_text {
        if let Some((lo, hi)) = text.source_span {
            let symbol = format!("callback_{id}");
            let label = module
                .closure_display_names
                .get(&id)
                .map_or("anonymous function", String::as_str);
            locations.register_generated(
                symbol,
                swc_common::Span::new(swc_common::BytePos(lo), swc_common::BytePos(hi)),
                label.to_owned(),
            );
        }
    }
    Ok((
        crate::source::SourceModule::bind(module, analysis, locations, witnesses.resources),
        witnesses.functions,
    ))
}

/// Perry supplies source spans for authored declarations and omits them for its temporaries.
pub(crate) fn declaration_flags(
    source: &crate::source::ModuleDeclarations,
    id: LocalId,
) -> crate::engine::types::LocalFlags {
    source
        .locals
        .get(&id)
        .map_or(crate::engine::types::LocalFlags::GENERATED, |local| {
            local.flags
        })
}

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

pub(crate) fn is_object_literal(name: &str) -> bool {
    name.starts_with("__AnonShape_")
}

pub(crate) fn is_import_initializer(callee: &Expr) -> bool {
    matches!(callee, Expr::ExternFuncRef { name, .. } if name == "js_native_module_named_esm_export_value")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::ConstructorSites;
    use perry_hir::types::Type as HirType;

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
        let (lowered, _) = lower(loaded, "shadow.ts", None).unwrap();
        for function in &lowered.bodies.functions {
            let Stmt::Return(Some(expr), _) = &function.body[0] else {
                panic!("expected constructor return");
            };
            if function.name == "builtin" {
                assert!(matches!(
                    crate::source::Constructors.collection(expr),
                    Some(crate::source::Constructor {
                        kind: crate::source::CollectionKind::Set,
                        types: [HirType::String],
                        init: None,
                        ..
                    })
                ));
            } else {
                assert!(crate::source::Constructors.collection(expr).is_none());
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
            let (lowered, _) = lower(loaded, "names.ts", None).unwrap();
            let function = &lowered.bodies.functions[0];
            let user = function.body.iter().find_map(|stmt| match stmt {
                Stmt::Let {
                    id,
                    init: Some(Expr::String(value)),
                    ..
                } if value == "user" => Some(*id),
                _ => None,
            });
            let returned = function.body.iter().find_map(|stmt| match stmt {
                Stmt::Return(Some(Expr::LocalGet(id)), _) => Some(*id),
                _ => None,
            });
            assert!(user.is_some(), "no user binding in {:?}", function.body);
            assert_eq!(returned, user, "{source}");
        }
    }
}

/// A key known without executing a computed property expression.
pub(crate) fn static_property(part: &perry_hir::ir::ObjectPart) -> Option<&str> {
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

#[derive(Clone, Copy)]
pub(crate) enum ErrorClass {
    Any,
    Type,
    Range,
    Reference,
    Syntax,
}
pub(crate) fn error_class(code: u32) -> Result<ErrorClass> {
    Ok(match code {
        0 => ErrorClass::Any,
        1 => ErrorClass::Type,
        2 => ErrorClass::Range,
        3 => ErrorClass::Reference,
        4 => ErrorClass::Syntax,
        _ => bail_internal!("unsupported Error constructor kind {code}"),
    })
}

mod objects;
pub(crate) use objects::{literal_option, object_literal_pairs, read_file_text};

pub(crate) fn tuple_position(index: &perry_hir::ir::Expr) -> Option<usize> {
    use perry_hir::ir::Expr;
    match index {
        Expr::Number(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as usize),
        Expr::Integer(n) => usize::try_from(*n).ok(),
        _ => None,
    }
}
