//! Source coordinates retained on MIR declarations through compiler transformations.
use perry_wit_diagnostics::SourceSpan;
use pliron::{
    builtin::{attributes::StringAttr, ops::FuncOp},
    context::Context,
    identifier::Identifier,
    op::Op,
};
const SOURCE: Identifier = Identifier::new("perry_source_span");
pub fn set_source_span(ctx: &mut Context, function: &FuncOp, span: &SourceSpan) {
    let serialized = serde_json::to_string(span).expect("serializable source coordinates");
    function
        .get_operation()
        .deref_mut(ctx)
        .attributes
        .set(SOURCE, StringAttr::new(serialized));
}
pub fn source_span(ctx: &Context, function: &FuncOp) -> Option<SourceSpan> {
    let operation = function.get_operation().deref(ctx);
    let value = operation.attributes.get::<StringAttr>(&SOURCE)?;
    serde_json::from_str(value.as_str()).ok()
}
