//! Source coordinates and diagnostic conversion for MIR operations and passes.
use perry_wit_diagnostics::{Diagnostic, Note, Result as DiagResult};
use pliron::{
    builtin::attributes::StringAttr,
    context::Context,
    derive::{format, pliron_attr},
    identifier::Identifier,
    op::Op,
    printable::Printable,
};
use std::path::PathBuf;

const SOURCE: Identifier = Identifier::new("perry_source_span");
const CALLER_NOTES: Identifier = Identifier::new("perry_caller_notes");

#[format("`(` $message opt($span) `)`")]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CallerNote {
    message: StringAttr,
    span: Option<StringAttr>,
}

#[pliron_attr(
    name = "perry.caller_notes",
    format = "`[` vec($0, CharSpace(`,`)) `]`",
    verifier = "succ"
)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CallerNotes(Vec<CallerNote>);

pub use perry_wit_diagnostics::SourceSpan;

pub trait PlironResultExt<T> {
    /// Preserve embedded diagnostics and render other IR errors with their context.
    fn to_diag(self, ctx: &Context) -> DiagResult<T>;
}

impl<T> PlironResultExt<T> for pliron::result::Result<T> {
    fn to_diag(self, ctx: &Context) -> DiagResult<T> {
        self.map_err(|error| match error.err.downcast::<Diagnostic>() {
            Ok(diagnostic) => *diagnostic,
            Err(err) => {
                Diagnostic::internal(pliron::result::Error { err, ..error }.disp(ctx).to_string())
            }
        })
    }
}

fn encode_span(span: &SourceSpan) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}:{}",
        span.start_line,
        span.start_column,
        span.end_line,
        span.end_column,
        span.start_byte,
        span.end_byte,
        span.file.to_string_lossy(),
    )
}

fn decode_span(raw: &str) -> Option<SourceSpan> {
    let mut parts = raw.splitn(7, ':');
    Some(SourceSpan {
        start_line: parts.next()?.parse().ok()?,
        start_column: parts.next()?.parse().ok()?,
        end_line: parts.next()?.parse().ok()?,
        end_column: parts.next()?.parse().ok()?,
        start_byte: parts.next()?.parse().ok()?,
        end_byte: parts.next()?.parse().ok()?,
        file: PathBuf::from(parts.next()?),
    })
}

pub fn set_source_span(ctx: &mut Context, operation: &dyn Op, span: &SourceSpan) {
    operation
        .get_operation()
        .deref_mut(ctx)
        .attributes
        .set(SOURCE, StringAttr::new(encode_span(span)));
}

pub fn source_span(ctx: &Context, operation: &dyn Op) -> Option<SourceSpan> {
    let operation = operation.get_operation().deref(ctx);
    let value = operation.attributes.get::<StringAttr>(&SOURCE)?;
    decode_span(value.as_str())
}

pub fn set_caller_notes(ctx: &mut Context, operation: &dyn Op, notes: &[Note]) {
    if notes.is_empty() {
        return;
    }
    let notes = notes
        .iter()
        .map(|note| CallerNote {
            message: StringAttr::from(note.message()),
            span: note.span().map(|span| StringAttr::new(encode_span(span))),
        })
        .collect();
    operation
        .get_operation()
        .deref_mut(ctx)
        .attributes
        .set(CALLER_NOTES, CallerNotes(notes));
}

pub fn attach_caller_notes(
    ctx: &Context,
    operation: &dyn Op,
    diagnostic: Diagnostic,
) -> Diagnostic {
    let operation = operation.get_operation().deref(ctx);
    let Some(notes) = operation.attributes.get::<CallerNotes>(&CALLER_NOTES) else {
        return diagnostic;
    };
    diagnostic.with_notes(notes.0.iter().map(|note| {
        match note
            .span
            .as_ref()
            .and_then(|span| decode_span(span.as_str()))
        {
            Some(span) => Note::spanned(span, note.message.as_str()),
            None => Note::new(note.message.as_str()),
        }
    }))
}
