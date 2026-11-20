//! Portable compiler diagnostics grouped by stable, caller-actionable [`ErrorKind`]s.
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

/// Broad, caller-actionable category of a compiler error.
///
/// Adding a variant is a breaking change. Consumers should branch on `ErrorKind`
/// and use [`Code`] for display, documentation, and tests.
///
/// Tie-breakers:
/// - A type or shape error on a TypeScript value is [`ErrorKind::TypeCheck`], even
///   when the expected type came from WIT.
/// - A declared TypeScript signature that disagrees with the WIT signature is
///   [`ErrorKind::Contract`]; a TypeScript value that does not fit a declared type
///   is [`ErrorKind::TypeCheck`].
/// - A valid WIT type with no compiler ABI mapping is [`ErrorKind::Unsupported`],
///   not [`ErrorKind::Wit`].
/// - A missing WIT dependency is [`ErrorKind::Wit`]; an unresolved TypeScript import
///   is [`ErrorKind::Module`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// The embedding caller passed malformed API input: bytes, names, or empty lists.
    InvalidInput,
    /// A host file read or write failed. Native file entrypoints only.
    Io,
    /// The WIT input is invalid on its own: syntax, decoding, missing dependency, or world selection.
    Wit,
    /// The TypeScript source does not parse.
    Syntax,
    /// The TypeScript module graph cannot be built: an unresolved, cyclic, unknown, or ambiguous local import/export.
    Module,
    /// A TypeScript value does not fit its declared or required type: missing annotation, nullability, record fields, or mismatch.
    TypeCheck,
    /// Valid TypeScript, regex, or WIT construct that Perry-WIT does not compile yet.
    Unsupported,
    /// The TypeScript module's export and import surface disagrees with the WIT world: names, arity, async, error channels, or top-level statements.
    Contract,
    /// A compiler bug.
    Internal,
}

impl ErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid-input",
            Self::Io => "io",
            Self::Wit => "wit",
            Self::Syntax => "syntax",
            Self::Module => "module",
            Self::TypeCheck => "type-check",
            Self::Unsupported => "unsupported",
            Self::Contract => "contract",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A stable diagnostic identifier paired with its [`ErrorKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Code {
    name: &'static str,
    kind: ErrorKind,
}

impl Code {
    const fn new(name: &'static str, kind: ErrorKind) -> Self {
        Self { name, kind }
    }

    pub const fn name(self) -> &'static str {
        self.name
    }

    pub const fn kind(self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

macro_rules! define_codes {
    ($( $ident:ident => ($name:literal, $kind:ident) ),+ $(,)?) => {
        pub mod codes {
            use super::{Code, ErrorKind};

            $(
                pub const $ident: Code = Code::new($name, ErrorKind::$kind);
            )+

            pub const ALL: &[Code] = &[
                $( $ident ),+
            ];
        }
    };
}

define_codes! {
    PARSE_ERROR => ("parse-error", Syntax),

    UNRESOLVED_MODULE => ("unresolved-module", Module),
    CYCLIC_IMPORT => ("cyclic-import", Module),
    UNKNOWN_EXPORT => ("unknown-export", Module),
    AMBIGUOUS_EXPORT => ("ambiguous-export", Module),

    INVALID_UTF8 => ("invalid-utf8", InvalidInput),
    INVALID_MODULE_PATH => ("invalid-module-path", InvalidInput),
    DUPLICATE_MODULE => ("duplicate-module", InvalidInput),
    EMPTY_WIT_SOURCES => ("empty-wit-sources", InvalidInput),

    WIT_PARSE => ("wit-parse", Wit),
    WIT_PACKAGE_ENCODING => ("wit-package-encoding", Wit),
    WIT_DEPENDENCY => ("wit-dependency", Wit),
    WIT_WORLD_SELECTION => ("wit-world-selection", Wit),
    MISSING_WIT_DIRECTORY => ("missing-wit-directory", Wit),

    MISSING_TYPE_ANNOTATION => ("missing-type-annotation", TypeCheck),
    MISSING_REQUIRED_FIELD => ("missing-required-field", TypeCheck),
    ABSENCE_MISMATCH => ("absence-mismatch", TypeCheck),
    INCOMPATIBLE_RECORD_LAYOUT => ("incompatible-record-layout", TypeCheck),
    TYPE_MISMATCH => ("type-mismatch", TypeCheck),
    POSSIBLY_NULL_RECEIVER => ("possibly-null-receiver", TypeCheck),

    UNSUPPORTED_TYPE => ("unsupported-type", Unsupported),
    UNSUPPORTED_OPERATOR => ("unsupported-operator", Unsupported),
    UNSUPPORTED_CONSTRUCT => ("unsupported-construct", Unsupported),
    UNSUPPORTED_REGEX => ("unsupported-regex", Unsupported),
    UNSUPPORTED_ABI_TYPE => ("unsupported-abi-type", Unsupported),
    BYTE_STREAM_NEEDS_WIT_STREAM => ("byte-stream-needs-wit-stream", Unsupported),

    INVALID_EXPORT_NAME => ("invalid-export-name", Contract),
    RESOURCE_IMPORTED_AND_EXPORTED => ("resource-imported-and-exported", Contract),
    EXPORT_TYPE_MISMATCH => ("export-type-mismatch", Contract),
    EXPORT_ARITY => ("export-arity", Contract),
    MISSING_EXPORT => ("missing-export", Contract),
    ASYNC_MISMATCH => ("async-mismatch", Contract),
    WIDE_INTEGER_REQUIRES_BIGINT => ("wide-integer-requires-bigint", Contract),
    TOP_LEVEL_REQUIRES_COMMAND_WORLD => ("top-level-requires-command-world", Contract),
    IMPORT_SIGNATURE_MISMATCH => ("import-signature-mismatch", Contract),
    MISSING_ERROR_CHANNEL => ("missing-error-channel", Contract),
    ASYNC_INITIALIZER_REQUIRES_ASYNC_EXPORT => ("async-initializer-requires-async-export", Contract),
    PENDING_INITIALIZATION => ("pending-initialization", Contract),
    SYNC_EXPORT_CONSOLE => ("sync-export-console", Contract),

    IO => ("io", Io),
    INTERNAL => ("internal", Internal),
}

/// An additional note attached to a [`Diagnostic`], optionally anchored to a [`SourceSpan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    message: String,
    span: Option<SourceSpan>,
}

impl Note {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span: None,
        }
    }

    pub fn spanned(span: SourceSpan, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span: Some(span),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn span(&self) -> Option<&SourceSpan> {
        self.span.as_ref()
    }
}

/// A structured compiler error diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    code: Code,
    message: String,
    span: Option<SourceSpan>,
    notes: Vec<Note>,
    help: Option<String>,
}

impl Diagnostic {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            span: None,
            notes: Vec::new(),
            help: None,
        }
    }

    pub fn io(message: impl Into<String>, source: &(dyn std::error::Error + 'static)) -> Self {
        Self::new(codes::IO, message).with_source_notes(source)
    }

    pub fn with_span(mut self, span: SourceSpan) -> Self {
        self.span = Some(span);
        self
    }

    pub fn with_optional_span(mut self, span: Option<SourceSpan>) -> Self {
        self.span = span;
        self
    }

    pub fn set_span_if_missing(&mut self, span: Option<SourceSpan>) {
        if self.span.is_none() {
            self.span = span;
        }
    }

    pub fn with_note(mut self, message: impl Into<String>) -> Self {
        self.notes.push(Note::new(message));
        self
    }

    pub fn with_spanned_note(mut self, span: SourceSpan, message: impl Into<String>) -> Self {
        self.notes.push(Note::spanned(span, message));
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn with_source_notes(mut self, source: &(dyn std::error::Error + 'static)) -> Self {
        let mut current = Some(source);
        while let Some(cause) = current {
            self.notes.push(Note::new(cause.to_string()));
            current = cause.source();
        }
        self
    }

    pub(crate) fn prepend_notes(&mut self, mut prefix: Vec<Note>) {
        if !prefix.is_empty() {
            prefix.append(&mut self.notes);
            self.notes = prefix;
        }
    }

    pub fn code(&self) -> Code {
        self.code
    }

    pub fn kind(&self) -> ErrorKind {
        self.code.kind()
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn span(&self) -> Option<&SourceSpan> {
        self.span.as_ref()
    }

    pub fn notes(&self) -> &[Note] {
        &self.notes
    }

    pub fn help(&self) -> Option<&str> {
        self.help.as_deref()
    }
}

impl fmt::Display for Diagnostic {
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
        write!(f, "error[{}]: {}", self.code, self.message)?;
        for note in &self.notes {
            if let Some(span) = &note.span {
                write!(
                    f,
                    "\n  {}:{}:{}: {}",
                    span.file.display(),
                    span.start_line,
                    span.start_column,
                    note.message
                )?;
            } else {
                write!(f, "\n  {}", note.message)?;
            }
        }
        if let Some(help) = &self.help {
            write!(f, "\n  help: {help}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostic {}

/// A compiler failure holding one or more [`Diagnostic`]s.
#[derive(Debug)]
pub struct CompileError {
    diagnostics: Vec<Diagnostic>,
}

impl From<Diagnostic> for CompileError {
    fn from(diagnostic: Diagnostic) -> Self {
        Self {
            diagnostics: vec![diagnostic],
        }
    }
}

impl CompileError {
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub fn primary(&self) -> &Diagnostic {
        &self.diagnostics[0]
    }

    pub fn into_diagnostics(self) -> Vec<Diagnostic> {
        self.diagnostics
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
        Some(self.primary())
    }
}

/// Internal `anyhow` adapters used by compiler crates; not re-exported by `perry-wit`.
pub mod anyhow_ext {
    use crate::{Code, CompileError, Diagnostic, ErrorKind, Note, SourceSpan, codes};

    pub fn into_error(diagnostic: Diagnostic) -> anyhow::Error {
        anyhow::Error::new(diagnostic)
    }

    fn find_diagnostic(error: &anyhow::Error) -> Option<Diagnostic> {
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<Diagnostic>())
            .cloned()
    }

    fn context_prefix(error: &anyhow::Error) -> Vec<Note> {
        error
            .chain()
            .take_while(|cause| cause.downcast_ref::<Diagnostic>().is_none())
            .map(|cause| Note::new(cause.to_string()))
            .collect()
    }

    pub trait FromAnyhow: Sized {
        fn from_error(error: anyhow::Error) -> Self;
        fn from_error_with_fallback(error: anyhow::Error, fallback: Code) -> Self;
    }

    impl FromAnyhow for Diagnostic {
        fn from_error(error: anyhow::Error) -> Self {
            Self::from_error_with_fallback(error, codes::INTERNAL)
        }

        fn from_error_with_fallback(error: anyhow::Error, fallback: Code) -> Self {
            if let Some(mut diagnostic) = find_diagnostic(&error) {
                if diagnostic.kind() == ErrorKind::Internal {
                    diagnostic.prepend_notes(context_prefix(&error));
                }
                return diagnostic;
            }
            let mut diagnostic = Diagnostic::new(fallback, error.to_string());
            if let Some(source) = error.source() {
                diagnostic = diagnostic.with_source_notes(source);
            }
            diagnostic
        }
    }

    impl FromAnyhow for CompileError {
        fn from_error(error: anyhow::Error) -> Self {
            Self::from_error_with_fallback(error, codes::INTERNAL)
        }

        fn from_error_with_fallback(error: anyhow::Error, fallback: Code) -> Self {
            match error.downcast::<Self>() {
                Ok(inner) => inner,
                Err(error) => Self::from(Diagnostic::from_error_with_fallback(error, fallback)),
            }
        }
    }

    /// Attach `span` to an existing [`Diagnostic`] in `error`, or convert `error` using `fallback`
    /// and attach `span` when missing.
    pub fn annotate_with_fallback(
        error: anyhow::Error,
        span: Option<SourceSpan>,
        fallback: Code,
    ) -> anyhow::Error {
        let mut diagnostic = Diagnostic::from_error_with_fallback(error, fallback);
        diagnostic.set_span_if_missing(span);
        diagnostic.into()
    }

    /// Attach `span` to an existing [`Diagnostic`] in `error`, leaving non-diagnostic errors untouched
    /// when `span` is `None`, or wrapping them as `codes::INTERNAL` with `span`.
    pub fn annotate(error: anyhow::Error, span: Option<SourceSpan>) -> anyhow::Error {
        let mut diagnostic = Diagnostic::from_error(error);
        diagnostic.set_span_if_missing(span);
        diagnostic.into()
    }

    /// Attach `span` only when `error` already carries a [`Diagnostic`]; untyped errors pass through.
    pub fn annotate_existing(error: anyhow::Error, span: Option<SourceSpan>) -> anyhow::Error {
        if let Some(mut diagnostic) = find_diagnostic(&error) {
            if diagnostic.kind() == ErrorKind::Internal {
                diagnostic.prepend_notes(context_prefix(&error));
            }
            diagnostic.set_span_if_missing(span);
            return diagnostic.into();
        }
        error
    }

    /// Apply the shared propagation rule when a boundary wrapper adds context around `source`.
    ///
    /// - If `source` contains a non-`Internal` [`Diagnostic`], returns it with `span` as a fallback
    ///   location and `user_note` appended explicitly, dropping outer `anyhow` context layers.
    /// - If `source` contains an `Internal` [`Diagnostic`], preserves outer context plus
    ///   `internal_context` as notes.
    /// - Otherwise builds `codes::INTERNAL` with `internal_context`, `span`, and `source` notes.
    pub fn wrap_source(
        source: anyhow::Error,
        span: Option<SourceSpan>,
        user_note: impl Into<String>,
        internal_context: impl Into<String>,
    ) -> Diagnostic {
        if let Some(mut inner) = find_diagnostic(&source) {
            if inner.kind() == ErrorKind::Internal {
                let mut prefix = vec![Note::new(internal_context)];
                prefix.extend(context_prefix(&source));
                inner.prepend_notes(prefix);
                inner.set_span_if_missing(span);
                return inner;
            }
            inner.set_span_if_missing(span);
            return inner.with_note(user_note);
        }
        Diagnostic::new(codes::INTERNAL, internal_context)
            .with_optional_span(span)
            .with_source_notes(source.as_ref())
    }

    /// Add an internal breadcrumb note around `source` while passing non-`Internal` diagnostics
    /// through unchanged.
    pub fn internal_context(
        source: impl Into<anyhow::Error>,
        context: impl Into<String>,
    ) -> anyhow::Error {
        let source = source.into();
        if let Some(mut inner) = find_diagnostic(&source) {
            if inner.kind() == ErrorKind::Internal {
                let mut prefix = vec![Note::new(context)];
                prefix.extend(context_prefix(&source));
                inner.prepend_notes(prefix);
                return inner.into();
            }
            return inner.into();
        }
        Diagnostic::new(codes::INTERNAL, context)
            .with_source_notes(source.as_ref())
            .into()
    }
}

#[macro_export]
macro_rules! diag {
    ($code:expr, $($arg:tt)+) => {
        $crate::anyhow_ext::into_error($crate::Diagnostic::new($code, ::std::format!($($arg)+)))
    };
}

#[macro_export]
macro_rules! bail_diag {
    ($code:expr, $($arg:tt)+) => {
        return ::std::result::Result::Err($crate::diag!($code, $($arg)+))
    };
}

#[macro_export]
macro_rules! ensure_diag {
    ($cond:expr, $code:expr, $($arg:tt)+) => {
        if !($cond) {
            $crate::bail_diag!($code, $($arg)+);
        }
    };
}

#[macro_export]
macro_rules! unsupported {
    ($($arg:tt)+) => {
        $crate::diag!($crate::codes::UNSUPPORTED_CONSTRUCT, $($arg)+)
    };
}

#[macro_export]
macro_rules! bail_unsupported {
    ($($arg:tt)+) => {
        $crate::bail_diag!($crate::codes::UNSUPPORTED_CONSTRUCT, $($arg)+)
    };
}

#[macro_export]
macro_rules! ensure_unsupported {
    ($cond:expr, $($arg:tt)+) => {
        $crate::ensure_diag!($cond, $crate::codes::UNSUPPORTED_CONSTRUCT, $($arg)+)
    };
}

#[macro_export]
macro_rules! internal {
    ($($arg:tt)+) => {
        $crate::diag!($crate::codes::INTERNAL, $($arg)+)
    };
}

#[macro_export]
macro_rules! bail_internal {
    ($($arg:tt)+) => {
        $crate::bail_diag!($crate::codes::INTERNAL, $($arg)+)
    };
}

#[macro_export]
macro_rules! ensure_internal {
    ($cond:expr, $($arg:tt)+) => {
        $crate::ensure_diag!($cond, $crate::codes::INTERNAL, $($arg)+)
    };
}
