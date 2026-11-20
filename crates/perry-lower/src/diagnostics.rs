//! Structured lowering diagnostics mapped directly into [`perry_wit_diagnostics::Diagnostic`].
use perry_wit_diagnostics::{Code, Diagnostic, codes};

/// Type-shape and coercion errors during HIR-to-MIR lowering.
pub(crate) enum ShapeError {
    MissingTypeAnnotation,
    UnsupportedType { ty: String },
    IncompatibleRecordLayout { actual: String, expected: String },
    AbsenceMismatch { actual: String, expected: String },
    TypeMismatch { actual: String, expected: String },
}

impl ShapeError {
    fn code(&self) -> Code {
        match self {
            Self::MissingTypeAnnotation => codes::MISSING_TYPE_ANNOTATION,
            Self::UnsupportedType { .. } => codes::UNSUPPORTED_TYPE,
            Self::IncompatibleRecordLayout { .. } => codes::INCOMPATIBLE_RECORD_LAYOUT,
            Self::AbsenceMismatch { .. } => codes::ABSENCE_MISMATCH,
            Self::TypeMismatch { .. } => codes::TYPE_MISMATCH,
        }
    }

    fn help(&self) -> Option<&'static str> {
        match self {
            Self::MissingTypeAnnotation => Some(
                "Add a concrete type annotation to this binding, parameter, or function return.",
            ),
            _ => None,
        }
    }
}

impl std::fmt::Display for ShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTypeAnnotation => f.write_str("the value type cannot be inferred"),
            Self::UnsupportedType { ty } => write!(f, "type {ty} is not supported yet"),
            Self::IncompatibleRecordLayout { actual, expected }
            | Self::AbsenceMismatch { actual, expected }
            | Self::TypeMismatch { actual, expected } => {
                write!(
                    f,
                    "value type {actual} differs from the expected type {expected}"
                )
            }
        }
    }
}

impl From<ShapeError> for Diagnostic {
    fn from(error: ShapeError) -> Self {
        let mut diagnostic = Diagnostic::new(error.code(), error.to_string());
        if let Some(help) = error.help() {
            diagnostic = diagnostic.with_help(help);
        }
        diagnostic
    }
}

/// Domain error kinds emitted by the HIR-to-MIR lowerer.
pub(crate) enum LowerErrorKind {
    ImportSignatureMismatch {
        binding: String,
        expected: usize,
        actual: usize,
    },
    MissingRequiredField {
        fields: Vec<String>,
    },
    UnsupportedOperator {
        op: String,
    },
}

impl LowerErrorKind {
    fn code(&self) -> Code {
        match self {
            Self::ImportSignatureMismatch { .. } => codes::IMPORT_SIGNATURE_MISMATCH,
            Self::MissingRequiredField { .. } => codes::MISSING_REQUIRED_FIELD,
            Self::UnsupportedOperator { .. } => codes::UNSUPPORTED_OPERATOR,
        }
    }
}

impl std::fmt::Display for LowerErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ImportSignatureMismatch {
                binding,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "WIT import {binding} expects {expected} arguments, got {actual}"
                )
            }
            Self::MissingRequiredField { fields } => {
                write!(f, "missing required record fields: {}", fields.join(", "))
            }
            Self::UnsupportedOperator { op } => {
                write!(f, "operator {op} is not supported yet")
            }
        }
    }
}

impl From<LowerErrorKind> for Diagnostic {
    fn from(error: LowerErrorKind) -> Self {
        Diagnostic::new(error.code(), error.to_string())
    }
}
