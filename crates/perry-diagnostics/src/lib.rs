//! Portable compiler diagnostics with conventional Rust error chains.
use std::{fmt, path::PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
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
        let mut current = Some(source);
        while let Some(cause) = current {
            self.notes.push(cause.to_string());
            current = cause.source();
        }
        self
    }
    pub fn prepend_notes(&mut self, mut prefix: Vec<String>) {
        if !prefix.is_empty() {
            prefix.append(&mut self.notes);
            self.notes = prefix;
        }
    }
    pub fn from_error(error: anyhow::Error, fallback: DiagnosticCode) -> Self {
        let error = if error.downcast_ref::<Self>().is_some() {
            let prefix: Vec<String> = error
                .chain()
                .take_while(|cause| cause.downcast_ref::<Self>().is_none())
                .map(ToString::to_string)
                .collect();
            match error.downcast::<Self>() {
                Ok(mut diagnostic) => {
                    diagnostic.prepend_notes(prefix);
                    return diagnostic;
                }
                Err(error) => error,
            }
        } else {
            error
        };
        let mut diagnostic = Self::error(fallback, error.to_string());
        if let Some(source) = error.source() {
            diagnostic = diagnostic.with_source_notes(source);
        }
        diagnostic
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
}

impl From<PerryDiagnostic> for CompileError {
    fn from(diagnostic: PerryDiagnostic) -> Self {
        Self {
            diagnostics: vec![diagnostic],
        }
    }
}

impl CompileError {
    pub fn from_error(error: anyhow::Error, fallback: DiagnosticCode) -> Self {
        match error.downcast::<Self>() {
            Ok(inner) => inner,
            Err(error) => Self::from(PerryDiagnostic::from_error(error, fallback)),
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
        self.diagnostics
            .first()
            .map(|d| d as &(dyn std::error::Error + 'static))
    }
}
