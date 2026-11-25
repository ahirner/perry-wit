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

#[format]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CallerRelation {
    Instantiation,
    Import,
}

impl CallerRelation {
    pub fn note(self, label: &str, span: SourceSpan) -> Note {
        let message = match self {
            Self::Instantiation => format!("while instantiating '{label}'"),
            Self::Import => format!("called from here ('{label}')"),
        };
        Note::spanned(span, message)
    }
}

#[format("`(` $name `,` $relation `,` $label `,` opt($span) `,` opt($parent) `)`")]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CallerFrame {
    name: StringAttr,
    relation: CallerRelation,
    label: StringAttr,
    span: Option<StringAttr>,
    parent: Option<StringAttr>,
}

impl CallerFrame {
    pub fn new(
        name: &str,
        parent: Option<&str>,
        relation: CallerRelation,
        label: &str,
        span: Option<SourceSpan>,
    ) -> Self {
        Self {
            name: name.into(),
            parent: parent.map(Into::into),
            relation,
            label: label.into(),
            span: span.map(|span| encode_span(&span).into()),
        }
    }
}

#[pliron_attr(
    name = "perry.caller_frames",
    format = "`[` vec($0, CharSpace(`,`)) `]`",
    verifier = "succ"
)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CallerFrames(Vec<CallerFrame>);

const FRAMES: Identifier = Identifier::new("perry_caller_frames");
const ORIGIN: Identifier = Identifier::new("perry_caller_origin");

pub fn set_caller_frames(
    ctx: &mut Context,
    module: &pliron::builtin::ops::ModuleOp,
    mut frames: Vec<CallerFrame>,
) {
    if !frames.is_empty() {
        frames.sort_unstable_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        module
            .get_operation()
            .deref_mut(ctx)
            .attributes
            .set(FRAMES, CallerFrames(frames));
    }
}

pub fn set_caller_origin(ctx: &mut Context, function: &pliron::builtin::ops::FuncOp, name: &str) {
    function
        .get_operation()
        .deref_mut(ctx)
        .attributes
        .set(ORIGIN, StringAttr::from(name));
}

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
    let diagnostic = match operation.attributes.get::<CallerNotes>(&CALLER_NOTES) {
        Some(notes) => diagnostic.with_notes(notes.0.iter().map(|note| {
            match note
                .span
                .as_ref()
                .and_then(|span| decode_span(span.as_str()))
            {
                Some(span) => Note::spanned(span, note.message.as_str()),
                None => Note::new(note.message.as_str()),
            }
        })),
        None => diagnostic,
    };
    if let Some(origin) = operation.attributes.get::<StringAttr>(&ORIGIN)
        && let Some(module) = operation.get_parent_op(ctx)
        && let Some(frames) = module.deref(ctx).attributes.get::<CallerFrames>(&FRAMES)
    {
        let mut notes = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut name = origin.as_str();
        while let Ok(index) = frames
            .0
            .binary_search_by(|frame| frame.name.as_str().cmp(name))
            && seen.insert(index)
        {
            let frame = &frames.0[index];
            if let Some(span) = frame.span.as_ref().and_then(|s| decode_span(s.as_str())) {
                notes.push(frame.relation.note(frame.label.as_str(), span));
            }
            let Some(parent) = &frame.parent else { break };
            name = parent.as_str();
        }
        return diagnostic.with_notes(notes);
    }
    diagnostic
}
