//! Source coordinates and diagnostic conversion for MIR operations and passes.
use perry_wit_diagnostics::{Diagnostic, Result as DiagResult, codes};
use pliron::{
    builtin::attributes::StringAttr, context::Context, identifier::Identifier, op::Op,
    printable::Printable,
};
use std::path::PathBuf;

const SOURCE: Identifier = Identifier::new("perry_source_span");

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

pub fn set_source_span(ctx: &mut Context, operation: &dyn Op, span: &SourceSpan) {
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
    operation
        .get_operation()
        .deref_mut(ctx)
        .attributes
        .set(SOURCE, StringAttr::new(serialized));
}

pub fn source_span(ctx: &Context, operation: &dyn Op) -> Option<SourceSpan> {
    let operation = operation.get_operation().deref(ctx);
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
