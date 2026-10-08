//! UTF-8 Scalar Value Contract & Text Operation Matrix.
//!
//! Enforces the Unicode scalar contract across text decoding, escapes, HIR literals,
//! and boundaries in the WAFFLE compiler pipeline.
//!
//! Deliberately departs from ECMAScript's UTF-16 model:
//! - All strings are valid UTF-8.
//! - Length, indexing, slicing, and search operate on Unicode scalar values.
//! - Paired surrogate escapes decode to their Unicode scalar value.
//! - Unpaired surrogate escapes and WTF-8 representations are rejected at the input boundary.
//! - Lossy conversions (such as U+FFFD substitution) are forbidden.

mod source;

pub(crate) use source::validate_ast_text;
pub use source::validate_source_text;

use super::visit;
use anyhow::{Result, bail};
use perry_hir::ir::{Expr, Module as HirModule};

/// The indexing and measurement unit of a text operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexUnit {
    /// Unicode scalar values (U+0000..U+D7FF, U+E000..U+10FFFF).
    ScalarValue,
    /// UTF-16 code units. Disallowed in Perry-WIT.
    Utf16CodeUnit,
    /// Byte offsets/lengths, used at Canonical ABI and binary boundaries.
    Byte,
}

/// Status of an operation under the target UTF-8 scalar contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationStatus {
    /// Supported and obeys the Unicode scalar value contract.
    SupportedScalar,
    /// Supported for Canonical ABI / binary transport.
    SupportedBoundary,
    /// Disallowed or diagnosed because it exposes UTF-16 surrogate halves.
    DisallowedUtf16,
}

/// An entry in the compact operation matrix.
#[derive(Debug, Clone)]
pub struct TextOperationEntry {
    pub operation: &'static str,
    pub unit: IndexUnit,
    pub status: OperationStatus,
    pub coercion: &'static str,
    pub boundary_behavior: &'static str,
    pub node_difference: &'static str,
}

/// Compact operation matrix documenting units, coercion, boundaries, and deliberate Node differences.
pub struct TextContractMatrix;

impl TextContractMatrix {
    pub const ENTRIES: &'static [TextOperationEntry] = &[
        TextOperationEntry {
            operation: "length",
            unit: IndexUnit::ScalarValue,
            status: OperationStatus::SupportedScalar,
            coercion: "Property access on String",
            boundary_behavior: "Internal scalar count; Canonical ABI retains UTF-8 byte length",
            node_difference: "Counts Unicode scalar values ('😀'.length === 1); Node counts UTF-16 code units (2)",
        },
        TextOperationEntry {
            operation: "index_access",
            unit: IndexUnit::ScalarValue,
            status: OperationStatus::SupportedScalar,
            coercion: "Finite, nonnegative integer numeric index; other numeric positions yield undefined",
            boundary_behavior: "Returns single-scalar string; out-of-range yields undefined",
            node_difference: "Returns complete scalar ('😀'[0] === '😀'); Node returns lone surrogate half ('\\uD83D')",
        },
        TextOperationEntry {
            operation: "charAt",
            unit: IndexUnit::ScalarValue,
            status: OperationStatus::SupportedScalar,
            coercion: "Numeric position uses ToIntegerOrInfinity; omitted/undefined defaults to 0",
            boundary_behavior: "Returns single-scalar string; out-of-range yields empty string",
            node_difference: "Returns complete scalar ('😀'.charAt(0) === '😀'); Node returns lone surrogate half",
        },
        TextOperationEntry {
            operation: "codePointAt",
            unit: IndexUnit::ScalarValue,
            status: OperationStatus::SupportedScalar,
            coercion: "Argument coerced to integer scalar offset",
            boundary_behavior: "Returns scalar codepoint value as number; out-of-range yields undefined",
            node_difference: "Position argument is a scalar index; Node uses UTF-16 code unit offset",
        },
        TextOperationEntry {
            operation: "charCodeAt",
            unit: IndexUnit::Utf16CodeUnit,
            status: OperationStatus::DisallowedUtf16,
            coercion: "N/A",
            boundary_behavior: "Disallowed and diagnosed under scalar contract",
            node_difference: "Disallowed; consumers must use codePointAt instead of relying on a hidden UTF-16 path",
        },
        TextOperationEntry {
            operation: "fromCodePoint",
            unit: IndexUnit::ScalarValue,
            status: OperationStatus::SupportedScalar,
            coercion: "Each argument coerced to Number and checked against 0..0x10FFFF excluding 0xD800..0xDFFF",
            boundary_behavior: "Constructs valid UTF-8; invalid code points enter catch/finally through the guest exception ABI",
            node_difference: "Rejects surrogates; range failures carry the rejected number until Error objects are supported",
        },
        TextOperationEntry {
            operation: "fromCharCode",
            unit: IndexUnit::Utf16CodeUnit,
            status: OperationStatus::DisallowedUtf16,
            coercion: "N/A",
            boundary_behavior: "Disallowed for surrogate halves under scalar contract",
            node_difference: "Disallowed for surrogate halves; consumers must use fromCodePoint",
        },
        TextOperationEntry {
            operation: "slice",
            unit: IndexUnit::ScalarValue,
            status: OperationStatus::SupportedScalar,
            coercion: "Numeric bounds use ToIntegerOrInfinity, then negative integers count backwards; omitted bounds cover the string",
            boundary_behavior: "Extracts scalar substring; never splits multibyte UTF-8 byte sequences",
            node_difference: "Indexes in scalar units; Node indexes in UTF-16 code units (which can split surrogate pairs)",
        },
        TextOperationEntry {
            operation: "indexOf",
            unit: IndexUnit::ScalarValue,
            status: OperationStatus::SupportedScalar,
            coercion: "Search must be a string; numeric position uses ToIntegerOrInfinity (defaults to 0)",
            boundary_behavior: "Returns scalar index of match, or -1",
            node_difference: "Both search position and returned offset count Unicode scalar values",
        },
        TextOperationEntry {
            operation: "regex_search",
            unit: IndexUnit::ScalarValue,
            status: OperationStatus::SupportedScalar,
            coercion: "search requires a regex literal; supports u and s flags, regular groups, alternation, repetition, classes, anchors, and ASCII word boundaries",
            boundary_behavior: "Returns the leftmost match's scalar index or -1; empty matches remain at scalar boundaries",
            node_difference: "Returned positions count scalars; pattern atoms use scalar semantics even without u; constructors, other flags, lookaround, and backreferences are diagnosed",
        },
        TextOperationEntry {
            operation: "for_of",
            unit: IndexUnit::ScalarValue,
            status: OperationStatus::SupportedScalar,
            coercion: "Iterated value must be a string; evaluated once before entering the loop",
            boundary_behavior: "Yields complete scalars in order; combining marks remain separate; empty strings yield nothing",
            node_difference: "Identical to Node string iteration for valid text; custom iterator protocols remain unsupported",
        },
        TextOperationEntry {
            operation: "split",
            unit: IndexUnit::ScalarValue,
            status: OperationStatus::SupportedScalar,
            coercion: "Separator must be a string",
            boundary_behavior: "Empty separator splits into individual Unicode scalar value strings",
            node_difference: "Empty separator splits by scalar values; Node splits into UTF-16 code units (isolating surrogates)",
        },
        TextOperationEntry {
            operation: "concat",
            unit: IndexUnit::ScalarValue,
            status: OperationStatus::SupportedScalar,
            coercion: "Both operands must be known strings; non-string and string-or-undefined coercions are diagnosed",
            boundary_behavior: "Concatenates valid UTF-8 byte sequences",
            node_difference: "Identical for valid text; rejects invalid operands and lone surrogates",
        },
        TextOperationEntry {
            operation: "comparison",
            unit: IndexUnit::ScalarValue,
            status: OperationStatus::SupportedScalar,
            coercion: "Strict equality distinguishes primitive types; mixed loose equality/ordering is diagnosed",
            boundary_behavior: "Lexicographical Unicode scalar comparison",
            node_difference: "Orders by Unicode scalar value; Node orders by UTF-16 code units",
        },
        TextOperationEntry {
            operation: "template_literal",
            unit: IndexUnit::ScalarValue,
            status: OperationStatus::SupportedScalar,
            coercion: "Interpolations must be known strings; other conversions and lone surrogates in quasis are diagnosed",
            boundary_behavior: "Constructs valid UTF-8 text",
            node_difference: "Rejects lone surrogate escapes instead of silently replacing with U+FFFD",
        },
        TextOperationEntry {
            operation: "surrogate_escapes",
            unit: IndexUnit::ScalarValue,
            status: OperationStatus::SupportedScalar,
            coercion: "Paired surrogates (\\uD83D\\uDE00) decode to Unicode scalar value (U+1F600)",
            boundary_behavior: "Unpaired surrogate escapes (\\uD800, \\uDC00) rejected at input boundary",
            node_difference: "Node preserves lone surrogates in strings (WTF-16/WTF-8); this contract rejects them",
        },
        TextOperationEntry {
            operation: "TextDecoder",
            unit: IndexUnit::Byte,
            status: OperationStatus::SupportedBoundary,
            coercion: "UTF-8 labels only; Uint8Array input; literal boolean options preserve evaluation order",
            boundary_behavior: "Incremental strict decoding retains split scalars, handles BOMs, and reports malformed or unfinished bytes through catch/finally",
            node_difference: "Defaults to fatal and rejects replacement decoding; numeric errors replace Error objects; streaming errors retain the Encoding Standard input queue, which Node currently discards",
        },
        TextOperationEntry {
            operation: "canonical_abi_string",
            unit: IndexUnit::Byte,
            status: OperationStatus::SupportedBoundary,
            coercion: "Import validated as UTF-8; export produced as exact UTF-8 bytes",
            boundary_behavior: "Memory layout is (ptr: i32, byte_len: i32)",
            node_difference: "Pure UTF-8 Canonical ABI compliance",
        },
        TextOperationEntry {
            operation: "console_text_output",
            unit: IndexUnit::Byte,
            status: OperationStatus::SupportedBoundary,
            coercion: "Exactly one string; formatting, multiple arguments, and implicit coercions are diagnosed",
            boundary_behavior: "Preserves UTF-8 and embedded NULs, appends a newline, and awaits P3 transfer and capability completion",
            node_difference: "Calls return void but may suspend for host output; numeric output failures enter catch/finally",
        },
        TextOperationEntry {
            operation: "filesystem_write_text",
            unit: IndexUnit::Byte,
            status: OperationStatus::SupportedBoundary,
            coercion: "A string path and string or byte-view data; implicit coercions are diagnosed",
            boundary_behavior: "Preserves exact UTF-8 and embedded NULs in data; NUL paths and invalid options fail before opening the file",
            node_difference: "Overwrite-only writes through confined preopens; numeric failures enter catch/finally and separate P3 completion precedes return",
        },
        TextOperationEntry {
            operation: "filesystem_read_text",
            unit: IndexUnit::Byte,
            status: OperationStatus::SupportedBoundary,
            coercion: "A string path and literal UTF-8 encoding; dynamic encodings require union lowering",
            boundary_behavior: "Checks producer completion and strict UTF-8, preserving BOMs and NULs in immutable scalar text",
            node_difference: "Malformed text throws filesystem error 9 instead of replacement decoding; reads materialize memory proportional to the file",
        },
    ];

    pub fn entries() -> &'static [TextOperationEntry] {
        Self::ENTRIES
    }
}

/// Validates that Perry HIR contains no lone surrogate or WTF-8 representations,
/// and diagnoses disallowed UTF-16 operations.
pub fn validate_hir_text(hir: &HirModule) -> Result<()> {
    let mut result = Ok(());
    let mut validate = |expr: &Expr| {
        if result.is_ok() {
            result = validate_expr(expr);
        }
    };
    visit::visit_statements(&hir.init, &mut validate);
    for function in &hir.functions {
        visit::visit_function_expressions(function, &mut validate);
    }
    for global in &hir.globals {
        if let Some(init) = &global.init {
            visit::visit_expression(init, &mut validate);
        }
    }
    result
}

fn validate_expr(expr: &Expr) -> Result<()> {
    match expr {
        Expr::WtfString(bytes) => {
            bail!(
                "WTF-8 string literal containing lone surrogates ({bytes:?}) is rejected under the UTF-8 scalar contract"
            );
        }
        Expr::StringFromCharCode(_) => {
            bail!(
                "String.fromCharCode is disallowed under the UTF-8 scalar contract; use String.fromCodePoint instead"
            );
        }
        Expr::Call { callee, .. } => {
            if let Expr::PropertyGet { property, .. } = callee.as_ref()
                && property == "charCodeAt"
            {
                bail!(
                    "String.prototype.charCodeAt is disallowed under the UTF-8 scalar contract; use codePointAt instead"
                );
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Validates raw bytes at a Canonical ABI or I/O boundary as well-formed UTF-8.
pub fn validate_utf8_boundary(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|e| anyhow::anyhow!("Invalid UTF-8 at boundary: {e}"))
}

/// Count Unicode scalar values in a UTF-8 string.
pub fn scalar_length(s: &str) -> usize {
    s.chars().count()
}

/// Extract the i-th Unicode scalar value as a string slice.
pub fn scalar_char_at(s: &str, index: usize) -> Option<&str> {
    let mut chars = s.char_indices();
    let (byte_start, ch) = chars.nth(index)?;
    Some(&s[byte_start..byte_start + ch.len_utf8()])
}

/// Slices text by Unicode scalar indices. Negative indices count from the end.
pub fn scalar_slice(s: &str, start: i64, end: Option<i64>) -> &str {
    let len = s.chars().count() as i64;
    let norm_start = if start < 0 {
        (len + start).max(0)
    } else {
        start.min(len)
    } as usize;
    let norm_end = match end {
        Some(e) => {
            let e = if e < 0 { (len + e).max(0) } else { e.min(len) };
            (e as usize).max(norm_start)
        }
        None => len as usize,
    };
    if norm_start >= norm_end {
        return "";
    }
    let mut indices = s
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(s.len()));
    let byte_start = indices.clone().nth(norm_start).unwrap_or(s.len());
    let byte_end = indices.nth(norm_end).unwrap_or(s.len());
    &s[byte_start..byte_end]
}

/// Searches for `search` starting at scalar index `pos`. Returns the match's scalar index.
pub fn scalar_index_of(s: &str, search: &str, pos: usize) -> Option<usize> {
    let mut indices = s
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(s.len()));
    let start_byte = indices.nth(pos).unwrap_or(s.len());
    if start_byte > s.len() {
        return None;
    }
    let byte_pos = s[start_byte..].find(search)?;
    let match_byte = start_byte + byte_pos;
    Some(s[..match_byte].chars().count())
}
