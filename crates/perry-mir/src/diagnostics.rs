//! Source coordinates retained on MIR declarations through compiler transformations.
use pliron::{
    builtin::{attributes::StringAttr, ops::FuncOp},
    context::Context,
    identifier::Identifier,
    op::Op,
};
use std::path::PathBuf;

const SOURCE: Identifier = Identifier::new("perry_source_span");

pub use perry_wit_diagnostics::SourceSpan;

pub fn set_source_span(ctx: &mut Context, function: &FuncOp, span: &SourceSpan) {
    let serialized = format!(
        "{}:{}:{}:{}:{}:{}:{}",
        span.start_line,
        span.start_column,
        span.end_line,
        span.end_column,
        span.start_byte,
        span.end_byte,
        span.file.to_string_lossy(),
    );
    function
        .get_operation()
        .deref_mut(ctx)
        .attributes
        .set(SOURCE, StringAttr::new(serialized));
}

pub fn source_span(ctx: &Context, function: &FuncOp) -> Option<SourceSpan> {
    let operation = function.get_operation().deref(ctx);
    let value = operation.attributes.get::<StringAttr>(&SOURCE)?;
    let mut parts = value.as_str().splitn(7, ':');
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
