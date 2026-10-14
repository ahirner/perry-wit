//! Portable compiler diagnostics with conventional Rust error chains.
use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub file: PathBuf,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub start_byte: usize,
    pub end_byte: usize,
}

impl SourceSpan {
    pub fn new(file: impl Into<PathBuf>, source: &str, start: usize, end: usize) -> Self {
        fn boundary(source: &str, offset: usize) -> usize {
            let mut offset = offset.min(source.len());
            while !source.is_char_boundary(offset) {
                offset -= 1;
            }
            offset
        }
        fn position(source: &str, offset: usize) -> (u32, u32) {
            let mut line = 1;
            let mut column = 1;
            let mut chars = source[..offset].chars().peekable();
            while let Some(ch) = chars.next() {
                match ch {
                    '\r' => {
                        chars.next_if_eq(&'\n');
                        line += 1;
                        column = 1;
                    }
                    '\n' | '\u{2028}' | '\u{2029}' => {
                        line += 1;
                        column = 1;
                    }
                    _ => column += 1,
                }
            }
            (line, column)
        }
        let start_byte = boundary(source, start);
        let end_byte = boundary(source, end.max(start_byte));
        let (start_line, start_column) = position(source, start_byte);
        let (end_line, end_column) = position(source, end_byte);
        Self {
            file: file.into(),
            start_line,
            start_column,
            end_line,
            end_column,
            start_byte,
            end_byte,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        diagnostic.notes.extend(
            error
                .chain()
                .filter(|cause| {
                    cause.downcast_ref::<PerryDiagnostic>().is_none()
                        && cause.downcast_ref::<CompileError>().is_none()
                })
                .map(ToString::to_string)
                .filter(|message| message != &diagnostic.message && message != &rendered),
        );
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
