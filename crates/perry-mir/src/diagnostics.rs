//! Source coordinates and diagnostic conversion for MIR operations and passes.
use perry_wit_diagnostics::{Diagnostic, Note, Result as DiagResult, codes};
use pliron::{
    builtin::attributes::StringAttr, context::Context, identifier::Identifier, op::Op,
    printable::Printable,
};
use std::path::PathBuf;

const SOURCE: Identifier = Identifier::new("perry_source_span");
const CALLER_NOTES: Identifier = Identifier::new("perry_caller_notes");
const NOTE_SEP: char = '\x1e';
const FIELD_SEP: char = '\x1f';

pub use perry_wit_diagnostics::SourceSpan;

/// Convert a pliron error into a [`Diagnostic`], preserving any embedded diagnostic (such as
/// narrowing errors from MIR passes) and turning IR invariants into [`codes::INTERNAL`].
pub fn pliron_to_diagnostic(ctx: &Context, error: pliron::result::Error) -> Diagnostic {
    match error.err.downcast::<Diagnostic>() {
        Ok(diagnostic) => *diagnostic,
        Err(err) => {
            let error = pliron::result::Error { err, ..error };
            Diagnostic::new(codes::INTERNAL, error.disp(ctx).to_string())
        }
    }
}

pub trait PlironResultExt<T> {
    fn to_diag(self, ctx: &Context) -> DiagResult<T>;
}

impl<T> PlironResultExt<T> for pliron::result::Result<T> {
    fn to_diag(self, ctx: &Context) -> DiagResult<T> {
        self.map_err(|error| pliron_to_diagnostic(ctx, error))
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
    let encoded: String = notes
        .iter()
        .map(|note| {
            format!(
                "{}{FIELD_SEP}{}",
                note.span().map(encode_span).unwrap_or_default(),
                note.message()
            )
        })
        .collect::<Vec<_>>()
        .join(&NOTE_SEP.to_string());
    operation
        .get_operation()
        .deref_mut(ctx)
        .attributes
        .set(CALLER_NOTES, StringAttr::new(encoded));
}

pub fn attach_caller_notes(ctx: &Context, operation: &dyn Op, diagnostic: Diagnostic) -> Diagnostic {
    let operation = operation.get_operation().deref(ctx);
    let Some(value) = operation.attributes.get::<StringAttr>(&CALLER_NOTES) else {
        return diagnostic;
    };
    let notes = value.as_str().split(NOTE_SEP).filter_map(|entry| {
        let (span_raw, message) = entry.split_once(FIELD_SEP)?;
        Some(match decode_span(span_raw) {
            Some(span) => Note::spanned(span, message),
            None => Note::new(message),
        })
    });
    diagnostic.with_notes(notes)
}
