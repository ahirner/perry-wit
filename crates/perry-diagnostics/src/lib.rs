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

/// Convenient `Result` alias using [`Diagnostic`] as its default error type.
pub type Result<T, E = Diagnostic> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiagnosticInner {
    code: Code,
    message: String,
    span: Option<SourceSpan>,
    notes: Vec<Note>,
    help: Option<String>,
}

/// A structured compiler error diagnostic (boxed internally so `Result<T, Diagnostic>` stays pointer-sized).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic(Box<DiagnosticInner>);

impl Diagnostic {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self(Box::new(DiagnosticInner {
            code,
            message: message.into(),
            span: None,
            notes: Vec::new(),
            help: None,
        }))
    }

    pub fn io(message: impl Into<String>, source: &(dyn std::error::Error + 'static)) -> Self {
        Self::new(codes::IO, message).with_source_notes(source)
    }

    pub fn internal_from(
        message: impl Into<String>,
        source: &(dyn std::error::Error + 'static),
    ) -> Self {
        Self::new(codes::INTERNAL, message).with_source_notes(source)
    }

    pub fn with_span(mut self, span: SourceSpan) -> Self {
        self.0.span = Some(span);
        self
    }

    pub fn with_optional_span(mut self, span: Option<SourceSpan>) -> Self {
        self.0.span = span;
        self
    }

    /// Attach `span` only when this diagnostic does not already have one.
    pub fn with_fallback_span(mut self, span: Option<SourceSpan>) -> Self {
        if self.0.span.is_none() {
            self.0.span = span;
        }
        self
    }

    pub fn with_note(mut self, message: impl Into<String>) -> Self {
        self.0.notes.push(Note::new(message));
        self
    }

    pub fn with_spanned_note(mut self, span: SourceSpan, message: impl Into<String>) -> Self {
        self.0.notes.push(Note::spanned(span, message));
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.0.help = Some(help.into());
        self
    }

    pub fn with_source_notes(mut self, source: &(dyn std::error::Error + 'static)) -> Self {
        let mut current = Some(source);
        while let Some(cause) = current {
            self.0.notes.push(Note::new(cause.to_string()));
            current = cause.source();
        }
        self
    }

    /// Prepend an internal breadcrumb note when this diagnostic is [`ErrorKind::Internal`];
    /// non-`Internal` diagnostics pass through unchanged so internal phase names never leak to users.
    pub fn with_internal_context(mut self, context: impl Into<String>) -> Self {
        if self.kind() == ErrorKind::Internal {
            self.0.notes.insert(0, Note::new(context));
        }
        self
    }

    /// Apply the shared boundary-wrapping rule: fill in `span` if missing, append `user_note`
    /// for user-facing diagnostics, or prepend `internal_context` for [`ErrorKind::Internal`].
    pub fn wrap_boundary(
        self,
        span: Option<SourceSpan>,
        user_note: impl Into<String>,
        internal_context: impl Into<String>,
    ) -> Self {
        let diagnostic = self.with_fallback_span(span);
        if diagnostic.kind() == ErrorKind::Internal {
            diagnostic.with_internal_context(internal_context)
        } else {
            diagnostic.with_note(user_note)
        }
    }

    pub fn code(&self) -> Code {
        self.0.code
    }

    pub fn kind(&self) -> ErrorKind {
        self.0.code.kind()
    }

    pub fn message(&self) -> &str {
        &self.0.message
    }

    pub fn span(&self) -> Option<&SourceSpan> {
        self.0.span.as_ref()
    }

    pub fn notes(&self) -> &[Note] {
        &self.0.notes
    }

    pub fn help(&self) -> Option<&str> {
        self.0.help.as_deref()
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(span) = &self.0.span {
            write!(
                f,
                "{}:{}:{}: ",
                span.file.display(),
                span.start_line,
                span.start_column
            )?;
        }
        write!(f, "error[{}]: {}", self.0.code, self.0.message)?;
        for note in &self.0.notes {
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
        if let Some(help) = &self.0.help {
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

#[macro_export]
macro_rules! diag {
    ($code:expr, $($arg:tt)+) => {
        $crate::Diagnostic::new($code, ::std::format!($($arg)+))
    };
}

#[macro_export]
macro_rules! bail_diag {
    ($code:expr, $($arg:tt)+) => {
        return ::std::result::Result::Err(::core::convert::Into::into($crate::diag!($code, $($arg)+)))
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

#[cfg(test)]
mod tests {
    use super::{CompileError, Diagnostic, ErrorKind, SourceSpan, codes};
    use std::collections::HashSet;

    fn is_kebab_case(s: &str) -> bool {
        !s.is_empty()
            && !s.starts_with('-')
            && !s.ends_with('-')
            && !s.contains("--")
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }

    #[test]
    fn codes_are_unique_and_kebab_case() {
        assert_eq!(
            std::mem::size_of::<Diagnostic>(),
            std::mem::size_of::<usize>()
        );
        assert!(!codes::ALL.is_empty());
        let mut names = HashSet::new();
        for code in codes::ALL {
            assert!(
                names.insert(code.name()),
                "duplicate diagnostic code: {}",
                code.name()
            );
            assert!(
                is_kebab_case(code.name()),
                "code name must be kebab-case: {}",
                code.name()
            );
            assert!(
                is_kebab_case(code.kind().as_str()),
                "kind name must be kebab-case: {}",
                code.kind().as_str()
            );
        }
    }

    #[test]
    fn display_matches_js_format_and_renders_spanned_notes() {
        let first = Diagnostic::new(codes::MISSING_REQUIRED_FIELD, "Missing value")
            .with_span(SourceSpan {
                file: "src/é.ts".into(),
                start_line: 2,
                start_column: 3,
                end_line: 2,
                end_column: 8,
                start_byte: 10,
                end_byte: 15,
            })
            .with_note("Record fields are mandatory")
            .with_help("Add value: null");
        let second = Diagnostic::new(codes::PARSE_ERROR, "Unexpected token");
        let error = CompileError {
            diagnostics: vec![first, second],
        };
        assert_eq!(
            error.to_string(),
            "src/é.ts:2:3: error[missing-required-field]: Missing value\n  Record fields are mandatory\n  help: Add value: null\nerror[parse-error]: Unexpected token"
        );

        let spanned = Diagnostic::new(codes::EXPORT_TYPE_MISMATCH, "type mismatch")
            .with_span(SourceSpan {
                file: "src/main.ts".into(),
                start_line: 4,
                start_column: 1,
                end_line: 4,
                end_column: 12,
                start_byte: 20,
                end_byte: 31,
            })
            .with_note("while binding WIT export run")
            .with_spanned_note(
                SourceSpan {
                    file: "src/types.ts".into(),
                    start_line: 1,
                    start_column: 10,
                    end_line: 1,
                    end_column: 16,
                    start_byte: 9,
                    end_byte: 15,
                },
                "expected here",
            )
            .with_help("update signature");
        assert_eq!(
            spanned.to_string(),
            "src/main.ts:4:1: error[export-type-mismatch]: type mismatch\n  while binding WIT export run\n  src/types.ts:1:10: expected here\n  help: update signature"
        );
    }

    #[test]
    fn context_and_boundary_wrapping_follow_error_kind() {
        let user_span = SourceSpan {
            file: "src/app.ts".into(),
            start_line: 3,
            start_column: 5,
            end_line: 3,
            end_column: 10,
            start_byte: 12,
            end_byte: 17,
        };
        let user = Diagnostic::new(codes::POSSIBLY_NULL_RECEIVER, "receiver is possibly null")
            .with_span(user_span.clone())
            .with_internal_context("optimizing MIR")
            .with_internal_context("compiling module");
        assert_eq!(user.code(), codes::POSSIBLY_NULL_RECEIVER);
        assert_eq!(user.span(), Some(&user_span));
        assert!(user.notes().is_empty());

        let internal = Diagnostic::new(codes::INTERNAL, "invariant violated")
            .with_internal_context("lowering function run")
            .with_internal_context("compiling module");
        assert_eq!(internal.kind(), ErrorKind::Internal);
        assert_eq!(internal.message(), "invariant violated");
        assert_eq!(
            internal
                .notes()
                .iter()
                .map(|note| note.message())
                .collect::<Vec<_>>(),
            vec!["compiling module", "lowering function run"]
        );

        let wrapped = Diagnostic::new(codes::EXPORT_ARITY, "expected 2 arguments")
            .with_help("pass 2 arguments")
            .with_internal_context("dropped internal layer")
            .wrap_boundary(
                Some(user_span.clone()),
                "while binding WIT export run",
                "binding WIT export run",
            );
        assert_eq!(wrapped.code(), codes::EXPORT_ARITY);
        assert_eq!(wrapped.message(), "expected 2 arguments");
        assert_eq!(wrapped.span(), Some(&user_span));
        assert_eq!(wrapped.help(), Some("pass 2 arguments"));
        assert_eq!(
            wrapped
                .notes()
                .iter()
                .map(|note| note.message())
                .collect::<Vec<_>>(),
            vec!["while binding WIT export run"]
        );

        let wrapped_bug = Diagnostic::new(codes::INTERNAL, "bad carrier")
            .with_internal_context("lowering op")
            .wrap_boundary(
                Some(user_span.clone()),
                "while binding WIT export run",
                "binding WIT export run",
            );
        assert_eq!(wrapped_bug.code(), codes::INTERNAL);
        assert_eq!(wrapped_bug.span(), Some(&user_span));
        assert_eq!(
            wrapped_bug
                .notes()
                .iter()
                .map(|note| note.message())
                .collect::<Vec<_>>(),
            vec!["binding WIT export run", "lowering op"]
        );

        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let internal_source = Diagnostic::internal_from("outer phase", &io_err);
        assert_eq!(internal_source.code(), codes::INTERNAL);
        assert_eq!(internal_source.message(), "outer phase");
        assert_eq!(
            internal_source
                .notes()
                .iter()
                .map(|note| note.message())
                .collect::<Vec<_>>(),
            vec!["no such file"]
        );
    }
}
