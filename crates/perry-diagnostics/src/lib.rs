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

    pub(crate) fn set_span_if_missing(&mut self, span: Option<SourceSpan>) {
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
    use crate::{CompileError, Diagnostic, ErrorKind, Note, SourceSpan, codes};

    pub fn into_error(diagnostic: Diagnostic) -> anyhow::Error {
        anyhow::Error::new(diagnostic)
    }

    fn extract_diagnostic(error: anyhow::Error) -> Result<(Diagnostic, Vec<Note>), anyhow::Error> {
        let mut chain = error.chain();
        let prefix = if chain.next().is_some_and(|top| top.is::<Diagnostic>()) {
            Vec::new()
        } else {
            match chain.find_map(|cause| cause.downcast_ref::<Diagnostic>()) {
                Some(inner) if inner.kind() == ErrorKind::Internal => error
                    .chain()
                    .take_while(|cause| cause.downcast_ref::<Diagnostic>().is_none())
                    .map(|cause| Note::new(cause.to_string()))
                    .collect(),
                Some(_) => Vec::new(),
                None => return Err(error),
            }
        };
        match error.downcast::<Diagnostic>() {
            Ok(diagnostic) => Ok((diagnostic, prefix)),
            Err(error) => match error
                .chain()
                .find_map(|cause| cause.downcast_ref::<Diagnostic>())
                .cloned()
            {
                Some(diagnostic) => Ok((diagnostic, prefix)),
                None => Err(error),
            },
        }
    }

    pub trait FromAnyhow: Sized {
        fn from_error(error: anyhow::Error) -> Self;
    }

    impl FromAnyhow for Diagnostic {
        fn from_error(error: anyhow::Error) -> Self {
            match extract_diagnostic(error) {
                Ok((mut diagnostic, prefix)) => {
                    diagnostic.prepend_notes(prefix);
                    diagnostic
                }
                Err(error) => {
                    let mut diagnostic = Diagnostic::new(codes::INTERNAL, error.to_string());
                    if let Some(source) = error.source() {
                        diagnostic = diagnostic.with_source_notes(source);
                    }
                    diagnostic
                }
            }
        }
    }

    impl FromAnyhow for CompileError {
        fn from_error(error: anyhow::Error) -> Self {
            match error.downcast::<Self>() {
                Ok(inner) => inner,
                Err(error) => Self::from(Diagnostic::from_error(error)),
            }
        }
    }

    /// Attach `span` to an existing [`Diagnostic`] in `error`, or convert an untyped error
    /// to `codes::INTERNAL` and attach `span` when missing.
    pub fn annotate(error: anyhow::Error, span: Option<SourceSpan>) -> anyhow::Error {
        let mut diagnostic = Diagnostic::from_error(error);
        diagnostic.set_span_if_missing(span);
        diagnostic.into()
    }

    /// Attach `span` only when `error` already carries a [`Diagnostic`]; untyped errors pass through.
    pub fn annotate_existing(error: anyhow::Error, span: Option<SourceSpan>) -> anyhow::Error {
        match extract_diagnostic(error) {
            Ok((mut diagnostic, prefix)) => {
                diagnostic.prepend_notes(prefix);
                diagnostic.set_span_if_missing(span);
                diagnostic.into()
            }
            Err(error) => error,
        }
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
        match extract_diagnostic(source) {
            Ok((mut inner, prefix)) => {
                inner.set_span_if_missing(span);
                if inner.kind() == ErrorKind::Internal {
                    let mut notes = Vec::with_capacity(prefix.len() + 1);
                    notes.push(Note::new(internal_context));
                    notes.extend(prefix);
                    inner.prepend_notes(notes);
                    inner
                } else {
                    inner.with_note(user_note)
                }
            }
            Err(source) => Diagnostic::new(codes::INTERNAL, internal_context)
                .with_optional_span(span)
                .with_source_notes(source.as_ref()),
        }
    }

    /// Add an internal breadcrumb note around `source` while passing non-`Internal` diagnostics
    /// through unchanged.
    pub fn internal_context(
        source: impl Into<anyhow::Error>,
        context: impl Into<String>,
    ) -> anyhow::Error {
        match extract_diagnostic(source.into()) {
            Ok((mut inner, prefix)) => {
                if inner.kind() == ErrorKind::Internal {
                    let mut notes = Vec::with_capacity(prefix.len() + 1);
                    notes.push(Note::new(context));
                    notes.extend(prefix);
                    inner.prepend_notes(notes);
                }
                inner.into()
            }
            Err(source) => Diagnostic::new(codes::INTERNAL, context)
                .with_source_notes(source.as_ref())
                .into(),
        }
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

#[cfg(test)]
mod tests {
    use super::{
        CompileError, Diagnostic, ErrorKind, SourceSpan,
        anyhow_ext::{FromAnyhow as _, wrap_source},
        codes,
    };
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
    fn from_error_context_retention_and_nested_extraction() {
        let user_span = SourceSpan {
            file: "src/app.ts".into(),
            start_line: 3,
            start_column: 5,
            end_line: 3,
            end_column: 10,
            start_byte: 12,
            end_byte: 17,
        };
        let user_err: anyhow::Error =
            Diagnostic::new(codes::POSSIBLY_NULL_RECEIVER, "receiver is possibly null")
                .with_span(user_span.clone())
                .into();
        let wrapped_user = user_err
            .context("optimizing MIR")
            .context("compiling module");
        let extracted = Diagnostic::from_error(wrapped_user);
        assert_eq!(extracted.code(), codes::POSSIBLY_NULL_RECEIVER);
        assert_eq!(extracted.span(), Some(&user_span));
        assert!(extracted.notes().is_empty());

        let internal_err: anyhow::Error =
            Diagnostic::new(codes::INTERNAL, "invariant violated").into();
        let wrapped_internal = internal_err
            .context("lowering function run")
            .context("compiling module");
        let extracted_internal = Diagnostic::from_error(wrapped_internal);
        assert_eq!(extracted_internal.kind(), ErrorKind::Internal);
        assert_eq!(extracted_internal.message(), "invariant violated");
        assert_eq!(
            extracted_internal
                .notes()
                .iter()
                .map(|note| note.message())
                .collect::<Vec<_>>(),
            vec!["compiling module", "lowering function run"]
        );

        let untyped = anyhow::anyhow!("root cause").context("outer phase");
        let extracted_untyped = Diagnostic::from_error(untyped);
        assert_eq!(extracted_untyped.code(), codes::INTERNAL);
        assert_eq!(extracted_untyped.message(), "outer phase");
        assert_eq!(
            extracted_untyped
                .notes()
                .iter()
                .map(|note| note.message())
                .collect::<Vec<_>>(),
            vec!["root cause"]
        );

        let inner_user: anyhow::Error =
            Diagnostic::new(codes::EXPORT_ARITY, "expected 2 arguments")
                .with_help("pass 2 arguments")
                .into();
        let wrapped = wrap_source(
            inner_user.context("dropped internal layer"),
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

        let inner_bug: anyhow::Error = Diagnostic::new(codes::INTERNAL, "bad carrier").into();
        let wrapped_bug = wrap_source(
            inner_bug.context("lowering op"),
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
    }
}
