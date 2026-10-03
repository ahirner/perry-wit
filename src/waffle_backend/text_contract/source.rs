//! Validate raw literal escapes before HIR lowering can replace lone surrogates.

use anyhow::{Context, Result, bail};
use perry_parser::{
    parse_typescript,
    swc_ecma_ast::{Module, Str, TplElement},
};
use swc_ecma_visit::{Visit, VisitWith};

/// Validates string literals and template quasis using the parser's expression boundaries.
pub fn validate_source_text(source: &str) -> Result<()> {
    let ast = parse_typescript(source, "text_contract.ts").context("Parsing source text")?;
    validate_ast_text(&ast)
}

/// Visits raw literals, including nested template expressions, before lossy HIR conversion.
pub(crate) fn validate_ast_text(ast: &Module) -> Result<()> {
    let mut validator = LiteralValidator { result: Ok(()) };
    ast.visit_with(&mut validator);
    validator.result
}

struct LiteralValidator {
    result: Result<()>,
}

impl Visit for LiteralValidator {
    fn visit_str(&mut self, literal: &Str) {
        if self.result.is_ok()
            && let Some(raw) = &literal.raw
        {
            self.result = validate_escapes(raw, literal.span.lo.0.saturating_sub(1) as usize);
        }
    }

    fn visit_tpl_element(&mut self, quasi: &TplElement) {
        if self.result.is_ok() {
            self.result = validate_escapes(&quasi.raw, quasi.span.lo.0.saturating_sub(1) as usize);
        }
    }
}

/// Validates one raw literal segment; surrogate pairs cannot cross interpolation boundaries.
fn validate_escapes(raw: &str, offset: usize) -> Result<()> {
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            index += 1;
            continue;
        }
        let escape_offset = offset + index;
        if bytes.get(index + 1) == Some(&b'u') {
            if bytes.get(index + 2) == Some(&b'{') {
                if let Some(end) = raw[index + 3..].find('}') {
                    let end = index + 3 + end;
                    if let Ok(code) = u32::from_str_radix(&raw[index + 3..end], 16) {
                        if (0xD800..=0xDFFF).contains(&code) {
                            bail!(
                                "Surrogate code point escape '\\u{{{code:X}}}' at offset {escape_offset} is rejected under the UTF-8 scalar contract"
                            );
                        }
                        index = end + 1;
                        continue;
                    }
                }
            } else if let Some(code) = unicode_escape(bytes, index) {
                if (0xD800..=0xDBFF).contains(&code) {
                    if unicode_escape(bytes, index + 6)
                        .is_some_and(|low| (0xDC00..=0xDFFF).contains(&low))
                    {
                        index += 12;
                        continue;
                    }
                    bail!(
                        "Unpaired high surrogate escape '\\u{code:04X}' at offset {escape_offset} is rejected under the UTF-8 scalar contract"
                    );
                }
                if (0xDC00..=0xDFFF).contains(&code) {
                    bail!(
                        "Unpaired low surrogate escape '\\u{code:04X}' at offset {escape_offset} is rejected under the UTF-8 scalar contract"
                    );
                }
                index += 6;
                continue;
            }
        }
        index += 2;
    }
    Ok(())
}

/// Reads a fixed-width Unicode escape without interpreting surrounding syntax.
fn unicode_escape(bytes: &[u8], index: usize) -> Option<u32> {
    let escape = bytes.get(index..index + 6)?;
    if &escape[..2] != b"\\u" {
        return None;
    }
    let digits = std::str::from_utf8(&escape[2..]).ok()?;
    u32::from_str_radix(digits, 16).ok()
}
