//! Source coordinates retained on MIR declarations through compiler transformations.
use pliron::{
    builtin::{attributes::StringAttr, ops::FuncOp},
    context::Context,
    identifier::Identifier,
    op::Op,
};
use std::path::PathBuf;

const SOURCE: Identifier = Identifier::new("perry_source_span");

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
