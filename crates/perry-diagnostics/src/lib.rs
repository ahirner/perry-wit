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
    pub fn with_source_notes(mut self, source: &(dyn std::error::Error + 'static)) -> Self {
        let rendered = self.to_string();
        let mut current = Some(source);
        while let Some(cause) = current {
            if cause.downcast_ref::<PerryDiagnostic>().is_some()
                || cause.downcast_ref::<CompileError>().is_some()
            {
                current = cause.source();
                continue;
            }
            let message = cause.to_string().trim().to_owned();
            if !message.is_empty()
                && message != self.message.trim()
                && message != rendered.trim()
                && !self.notes.iter().any(|note| note.trim() == message)
            {
                self.notes.push(message);
            }
            current = cause.source();
        }
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

impl From<PerryDiagnostic> for CompileError {
    fn from(diagnostic: PerryDiagnostic) -> Self {
        let source = anyhow::Error::new(diagnostic.clone());
        Self {
            diagnostics: vec![diagnostic],
            source,
        }
    }
}

impl CompileError {
    pub fn from_error(error: anyhow::Error, fallback: DiagnosticCode) -> Self {
        if let Some(inner) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<CompileError>())
        {
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
            })
            .unwrap_or_else(|| PerryDiagnostic::error(fallback, error.to_string()));
        let diagnostic = diagnostic.with_source_notes(error.as_ref());
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
