//! Maps the supported ECMAScript literal syntax to Rust regex syntax.

use super::{Captures, ParsedRegex, RegexLiteral};
use perry_wit_diagnostics::{Diagnostic, Result, bail_diag, codes, diag, ensure_diag};
use regex_syntax::{
    ast::{self, Ast, GroupKind},
    hir::translate::Translator,
};

const SPACE: &str =
    r"\x09-\x0D\x20\u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}";

pub(super) fn parse(pattern: &str, flags: &str) -> Result<ParsedRegex> {
    let mut seen = 0u8;
    for flag in flags.chars() {
        let bit = match flag {
            'u' => 1,
            's' => 2,
            'g' => 4,
            'i' => 8,
            _ => bail_diag!(
                codes::UNSUPPORTED_REGEX,
                "Regex search supports only the u, s, g, and i flags"
            ),
        };
        ensure_diag!(
            seen & bit == 0,
            codes::UNSUPPORTED_REGEX,
            "Duplicate regex flags"
        );
        seen |= bit;
    }
    let mut output = String::new();
    if seen & 8 != 0 {
        output.push_str("(?i)");
    }
    let mut chars = pattern.chars();
    let mut in_class = false;
    while let Some(character) = chars.next() {
        match character {
            '[' => {
                ensure_diag!(
                    !in_class,
                    codes::UNSUPPORTED_REGEX,
                    "Nested regex character classes are unsupported"
                );
                if chars.as_str().starts_with(']') {
                    chars.next();
                    output.push_str(r"[^\s\S]");
                    continue;
                }
                if chars.as_str().starts_with("^]") {
                    chars.next();
                    chars.next();
                    output.push_str(r"[\s\S]");
                    continue;
                }
                in_class = true;
                output.push('[');
            }
            ']' => {
                in_class = false;
                output.push(']');
            }
            '&' | '-' | '~' if in_class && chars.as_str().starts_with(character) => {
                bail_diag!(
                    codes::UNSUPPORTED_REGEX,
                    "Regex character-class set operators are unsupported"
                )
            }
            '(' if !in_class && chars.as_str().starts_with('?') => {
                output.push('(');
                let prefix = chars.as_str();
                if let Some(name) = prefix
                    .strip_prefix("?<")
                    .filter(|s| !s.starts_with(['=', '!']))
                {
                    let (header, rest) =
                        name.split_at(name.find('>').map_or(name.len(), |end| end + 1));
                    output.push_str("?<");
                    output.push_str(header);
                    chars = rest.chars();
                } else if !prefix.starts_with("?:") {
                    bail_diag!(
                        codes::UNSUPPORTED_REGEX,
                        "Regex lookaround and inline flags are unsupported"
                    );
                }
            }
            '.' if !in_class => output.push_str(if flags.contains('s') {
                r"[\s\S]"
            } else {
                r"[^\n\r\u{2028}\u{2029}]"
            }),
            '\\' => {
                let escaped = chars.next().ok_or_else(|| {
                    Diagnostic::new(codes::UNSUPPORTED_REGEX, "Unfinished regex escape")
                })?;
                match escaped {
                    'd' | 'D' | 'w' | 'W' | 's' | 'S' => {
                        let body = match escaped.to_ascii_lowercase() {
                            'd' => "0-9",
                            'w' => "A-Za-z0-9_",
                            _ => SPACE,
                        };
                        output.push('[');
                        if escaped.is_ascii_uppercase() {
                            output.push('^');
                        }
                        output.push_str(body);
                        output.push(']');
                    }
                    'b' if in_class => output.push_str(r"\x08"),
                    'b' if !in_class => output.push_str(r"(?-u:\b)"),
                    'B' if !in_class => output.push_str(r"(?-u:\B)"),
                    '0' => {
                        ensure_diag!(
                            !chars
                                .as_str()
                                .as_bytes()
                                .first()
                                .is_some_and(u8::is_ascii_digit),
                            codes::UNSUPPORTED_REGEX,
                            "Regex octal escapes are unsupported"
                        );
                        output.push_str(r"\x00");
                    }
                    'u' => {
                        let braced = chars.as_str().strip_prefix('{');
                        let mut code = if let Some(digits) = braced {
                            ensure_diag!(
                                flags.contains('u'),
                                codes::UNSUPPORTED_REGEX,
                                "Braced Unicode regex escapes require the u flag"
                            );
                            let (hex, rest) = digits.split_once('}').ok_or_else(|| {
                                diag!(codes::UNSUPPORTED_REGEX, "Unfinished regex Unicode escape")
                            })?;
                            chars = rest.chars();
                            u32::from_str_radix(hex, 16).map_err(|_| {
                                diag!(codes::UNSUPPORTED_REGEX, "Invalid regex Unicode escape")
                            })?
                        } else {
                            read_hex(&mut chars, 4)?
                        };
                        if braced.is_none() && (0xD800..=0xDBFF).contains(&code) {
                            ensure_diag!(
                                chars.next() == Some('\\') && chars.next() == Some('u'),
                                codes::UNSUPPORTED_REGEX,
                                "Unpaired surrogate in regex escape"
                            );
                            let low = read_hex(&mut chars, 4)?;
                            ensure_diag!(
                                (0xDC00..=0xDFFF).contains(&low),
                                codes::UNSUPPORTED_REGEX,
                                "Unpaired surrogate in regex escape"
                            );
                            code = 0x10000 + ((code - 0xD800) << 10) + low - 0xDC00;
                        }
                        ensure_diag!(
                            char::from_u32(code).is_some(),
                            codes::UNSUPPORTED_REGEX,
                            "Invalid scalar or unpaired surrogate in regex escape"
                        );
                        output.push_str(&format!(r"\u{{{code:X}}}"));
                    }
                    'x' => {
                        let code = read_hex(&mut chars, 2)?;
                        output.push_str(&format!(r"\u{{{code:X}}}"));
                    }
                    'n' | 'r' | 't' | 'f' | 'v' | '.' | '*' | '+' | '?' | '^' | '$' | '\\'
                    | '[' | ']' | '{' | '}' | '(' | ')' | '/' | '|' | '-' => {
                        output.push('\\');
                        output.push(escaped);
                    }
                    _ => bail_diag!(
                        codes::UNSUPPORTED_REGEX,
                        "Unsupported regex escape: \\{escaped}"
                    ),
                }
            }
            _ => output.push(character),
        }
    }
    let ast = ast::parse::Parser::new()
        .parse(&output)
        .map_err(|err| RegexLiteral::build_error(&err))?;
    let captures = ast::visit(&ast, Captures::default())?;
    let hir = Translator::new()
        .translate(&output, &ast)
        .map_err(|err| RegexLiteral::build_error(&err))?;
    Ok(ParsedRegex { hir, captures })
}

impl ast::Visitor for Captures {
    type Output = Self;
    type Err = Diagnostic;

    fn visit_pre(&mut self, ast: &Ast) -> Result<()> {
        if let Ast::Group(group) = ast
            && let Some(slot) = group.capture_index()
        {
            self.count = self.count.max(slot);
            if let GroupKind::CaptureName { name, .. } = &group.kind {
                self.names.push((slot as usize, name.name.clone()));
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<Self> {
        let limit = perry_abi::layout::regex::MAX_CAPTURES;
        ensure_diag!(
            self.count as usize <= limit,
            codes::UNSUPPORTED_REGEX,
            "Regex has {} capture groups; at most {limit} are supported",
            self.count
        );
        Ok(self)
    }
}

fn read_hex(chars: &mut impl Iterator<Item = char>, count: usize) -> Result<u32> {
    let mut code = 0u32;
    for _ in 0..count {
        let digit = chars.next().and_then(|c| c.to_digit(16)).ok_or_else(|| {
            Diagnostic::new(codes::UNSUPPORTED_REGEX, "Invalid regex Unicode escape")
        })?;
        code = code * 16 + digit;
    }
    Ok(code)
}
