//! Shared diagnostic helpers for HIR-to-MIR lowering.
use perry_wit_diagnostics::{Diagnostic, codes};

pub(crate) fn missing_type_annotation() -> Diagnostic {
    Diagnostic::new(
        codes::MISSING_TYPE_ANNOTATION,
        "the value type cannot be inferred",
    )
    .with_help("Add a concrete type annotation to this binding, parameter, or function return.")
}
