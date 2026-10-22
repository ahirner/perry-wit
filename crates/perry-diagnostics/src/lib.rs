//! Portable compiler diagnostics with conventional Rust error chains.
pub use perry_mir::SourceSpan;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticCode {
    ParseError,
    UnsupportedOperator,
    UnsupportedSpread,
    MissingTypeAnnotation,
    UnsupportedType,
    UnsupportedExpression,
    IncompatibleRecordLayout,
    MissingRequiredField,
    AbsenceMismatch,
    InterfaceMismatch,
    ExportMismatch,
    ImportSignatureMismatch,
    ModuleResolution,
    InvalidInput,
    WitResolution,
    CodegenFailure,
    MissingErrorChannel,
    Io,
    InternalCompilerError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerryDiagnostic {
    pub code: DiagnosticCode,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub span: Option<SourceSpan>,
    pub notes: Vec<String>,
    pub help: Option<String>,
}

impl PerryDiagnostic {
    pub fn error(code: DiagnosticCode, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: DiagnosticSeverity::Error,
            message: message.into(),
            span: None,
            notes: Vec::new(),
            help: None,
        }
    }
    pub fn with_span(mut self, span: SourceSpan) -> Self {
        self.span = Some(span);
        self
    }
    pub fn with_optional_span(mut self, span: Option<SourceSpan>) -> Self {
        self.span = span;
        self
    }
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}

impl fmt::Display for PerryDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(span) = &self.span {
            write!(
                f,
                "{}:{}:{}: ",
                span.file.display(),
                span.start_line,
                span.start_column
            )?;
        }
        write!(f, "{:?}: {}", self.code, self.message)?;
        for note in &self.notes {
            write!(f, "\n  {note}")?;
        }
        if let Some(help) = &self.help {
            write!(f, "\n  help: {help}")?;
        }
        Ok(())
    }
}
impl std::error::Error for PerryDiagnostic {}

#[derive(Debug)]
pub struct CompileError {
    pub diagnostics: Vec<PerryDiagnostic>,
    source: anyhow::Error,
}

impl CompileError {
    pub fn stage(error: anyhow::Error, code: DiagnosticCode) -> anyhow::Error {
        if error.downcast_ref::<PerryDiagnostic>().is_some()
            || error.downcast_ref::<CompileError>().is_some()
        {
            error
        } else {
            let diagnostic = PerryDiagnostic::error(code, error.to_string());
            error.context(diagnostic)
        }
    }

    pub fn from_error(error: anyhow::Error, fallback: DiagnosticCode) -> Self {
        if let Some(inner) = error.downcast_ref::<CompileError>() {
            return Self {
                diagnostics: inner.diagnostics.clone(),
                source: error,
            };
        }
        let diagnostic = error
            .downcast_ref::<PerryDiagnostic>()
            .cloned()
            .or_else(|| {
                error
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<PerryDiagnostic>())
                    .cloned()
            });
        let mut diagnostic =
            diagnostic.unwrap_or_else(|| PerryDiagnostic::error(fallback, error.to_string()));
        let rendered = diagnostic.to_string();
        for cause in error.chain() {
            if cause.downcast_ref::<PerryDiagnostic>().is_some()
                || cause.downcast_ref::<CompileError>().is_some()
            {
                continue;
            }
            let message = cause.to_string().trim().to_owned();
            if !message.is_empty()
                && message != diagnostic.message.trim()
                && message != rendered.trim()
                && !diagnostic.notes.iter().any(|note| note.trim() == message)
            {
                diagnostic.notes.push(message);
            }
        }
        Self {
            diagnostics: vec![diagnostic],
            source: error,
        }
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, diagnostic) in self.diagnostics.iter().enumerate() {
            if index != 0 {
                writeln!(f)?;
            }
            write!(f, "{diagnostic}")?;
        }
        Ok(())
    }
}
impl std::error::Error for CompileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}
