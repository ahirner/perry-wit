//! Maps the supported ECMAScript literal syntax to Rust regex syntax.

use anyhow::{Result, bail, ensure};

const SPACE: &str =
    r"\x09-\x0D\x20\u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}";

pub(super) fn normalize(pattern: &str, flags: &str) -> Result<String> {
    ensure!(
        flags.chars().all(|c| matches!(c, 'u' | 's')),
        "Regex search supports only the u and s flags"
    );
    ensure!(
        flags.matches('u').count() <= 1 && flags.matches('s').count() <= 1,
        "Duplicate regex flags"
    );
    let mut output = String::new();
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;
    while let Some(character) = chars.next() {
        match character {
            '[' => {
                ensure!(!in_class, "Nested regex character classes are unsupported");
                if chars.peek() == Some(&']') {
                    chars.next();
                    output.push_str(r"[^\s\S]");
                    continue;
                }
                if chars.clone().take(2).eq(['^', ']']) {
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
            '&' | '-' | '~' if in_class && chars.peek() == Some(&character) => {
                bail!("Regex character-class set operators are unsupported")
            }
            '(' if !in_class && chars.peek() == Some(&'?') => {
                chars.next();
                ensure!(
                    chars.next() == Some(':'),
                    "Regex lookaround, named groups, and inline flags are unsupported"
                );
                output.push_str("(?:");
            }
            '.' if !in_class => output.push_str(if flags.contains('s') {
                r"[\s\S]"
            } else {
                r"[^\n\r\u{2028}\u{2029}]"
            }),
            '\\' => {
                let escaped = chars
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("Unfinished regex escape"))?;
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
                        ensure!(
                            !chars.peek().is_some_and(char::is_ascii_digit),
                            "Regex octal escapes are unsupported"
                        );
                        output.push_str(r"\x00");
                    }
                    'u' => {
                        let braced = chars.peek() == Some(&'{');
                        let mut code = if braced {
                            chars.next();
                            let mut hex = String::new();
                            loop {
                                let digit = chars.next().ok_or_else(|| {
                                    anyhow::anyhow!("Unfinished regex Unicode escape")
                                })?;
                                if digit == '}' {
                                    break;
                                }
                                hex.push(digit);
                            }
                            u32::from_str_radix(&hex, 16)?
                        } else {
                            read_hex(&mut chars, 4)?
                        };
                        if !braced && (0xD800..=0xDBFF).contains(&code) {
                            ensure!(
                                chars.next() == Some('\\') && chars.next() == Some('u'),
                                "Unpaired surrogate in regex escape"
                            );
                            let low = read_hex(&mut chars, 4)?;
                            ensure!(
                                (0xDC00..=0xDFFF).contains(&low),
                                "Unpaired surrogate in regex escape"
                            );
                            code = 0x10000 + ((code - 0xD800) << 10) + low - 0xDC00;
                        }
                        ensure!(
                            char::from_u32(code).is_some(),
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
                    _ => bail!("Unsupported regex escape: \\{escaped}"),
                }
            }
            _ => output.push(character),
        }
    }
    Ok(output)
}

fn read_hex(chars: &mut impl Iterator<Item = char>, count: usize) -> Result<u32> {
    let mut code = 0u32;
    for _ in 0..count {
        let digit = chars
            .next()
            .and_then(|c| c.to_digit(16))
            .ok_or_else(|| anyhow::anyhow!("Invalid regex Unicode escape"))?;
        code = code * 16 + digit;
    }
    Ok(code)
}
