/// Type-shape and coercion errors during HIR-to-MIR lowering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShapeError {
    MissingTypeAnnotation,
    UnsupportedType { ty: String },
    IncompatibleRecordLayout { actual: String, expected: String },
    AbsenceMismatch { actual: String, expected: String },
    TypeMismatch { actual: String, expected: String },
}

impl ShapeError {
    pub fn help(&self) -> Option<&'static str> {
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
            Self::MissingTypeAnnotation => {
                f.write_str("the value type cannot be inferred for the MIR backend")
            }
            Self::UnsupportedType { ty } => {
                write!(f, "MIR backend does not support type {ty} yet")
            }
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

/// Domain error kinds emitted by the HIR-to-MIR lowerer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LowerErrorKind {
    Parse {
        identity: String,
        message: String,
    },
    ModuleResolution {
        name: String,
        parent: std::path::PathBuf,
    },
    ImportSignatureMismatch {
        binding: String,
        expected: usize,
        actual: usize,
    },
    Shape(ShapeError),
    MissingRequiredField {
        fields: Vec<String>,
    },
    UnsupportedOperator {
        op: String,
    },
    UnsupportedSpread,
    UnsupportedExpression(String),
}

impl From<ShapeError> for LowerErrorKind {
    fn from(error: ShapeError) -> Self {
        Self::Shape(error)
    }
}

impl LowerErrorKind {
    pub fn help(&self) -> Option<&'static str> {
        match self {
            Self::Shape(shape) => shape.help(),
            _ => None,
        }
    }
}

impl std::fmt::Display for LowerErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse { identity, message } => write!(f, "Parsing {identity}: {message}"),
            Self::ModuleResolution { name, parent } => {
                write!(f, "Cannot resolve '{name}' from {}", parent.display())
            }
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
            Self::Shape(shape) => shape.fmt(f),
            Self::MissingRequiredField { fields } => {
                write!(f, "missing required record fields: {}", fields.join(", "))
            }
            Self::UnsupportedOperator { op } => {
                write!(f, "MIR backend does not support operator {op} yet")
            }
            Self::UnsupportedSpread => {
                f.write_str("sparse array spreads are outside the dense-array profile")
            }
            Self::UnsupportedExpression(message) => f.write_str(message),
        }
    }
}

/// Source-spanned error emitted during HIR-to-MIR lowering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LowerError {
    pub kind: LowerErrorKind,
    pub span: Option<perry_wit_diagnostics::SourceSpan>,
    pub notes: Vec<String>,
}

impl LowerError {
    pub fn new(kind: impl Into<LowerErrorKind>) -> Self {
        Self {
            kind: kind.into(),
            span: None,
            notes: Vec::new(),
        }
    }

    pub fn with_span(mut self, span: perry_wit_diagnostics::SourceSpan) -> Self {
        self.span = Some(span);
        self
    }

    pub fn help(&self) -> Option<&'static str> {
        self.kind.help()
    }

    /// Extract a [`LowerError`] from `error`, prepending any outer `anyhow` context notes.
    pub fn try_from_anyhow(error: anyhow::Error) -> std::result::Result<Self, anyhow::Error> {
        if error.downcast_ref::<Self>().is_none() {
            return Err(error);
        }
        let mut prefix: Vec<String> = error
            .chain()
            .take_while(|cause| cause.downcast_ref::<Self>().is_none())
            .map(ToString::to_string)
            .collect();
        let mut diagnostic = error.downcast::<Self>()?;
        if !prefix.is_empty() {
            prefix.append(&mut diagnostic.notes);
            diagnostic.notes = prefix;
        }
        Ok(diagnostic)
    }
}

impl std::fmt::Display for LowerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.kind.fmt(f)
    }
}

impl std::error::Error for LowerError {}
