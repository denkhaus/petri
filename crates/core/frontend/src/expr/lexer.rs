//! Tokens of the `${{ }}` grammar.

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    Null,
    True,
    False,
    Number(f64),
    /// Contents with `''` already unescaped to `'`.
    Str(String),
    Ident(String),
    Dot,
    Star,
    Comma,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Not,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
    /// End of input.
    Eof,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind:   TokenKind,
    /// Byte offset of the token's first character.
    pub offset: usize,
    pub len:    usize,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LexError {
    #[error("unexpected character `{ch}`")]
    Unexpected { ch: char, offset: usize },
    #[error("unterminated string")]
    UnterminatedString { offset: usize },
    #[error("malformed number `{text}`")]
    BadNumber { text: String, offset: usize },
}

impl LexError {
    pub fn offset(&self) -> usize {
        match self {
            Self::Unexpected { offset, .. }
            | Self::UnterminatedString { offset }
            | Self::BadNumber { offset, .. } => *offset,
        }
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

/// Identifier bodies allow `-`, because GitHub property names do: `fail-fast`,
/// `job-index`, `head_ref`.
fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

pub fn lex(source: &str) -> Result<Vec<Token>, LexError> {
    let mut tokens = Vec::new();
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = source[i..].chars().next().unwrap_or('\0');
        let start = i;
        let mut push = |kind: TokenKind, len: usize| {
            tokens.push(Token {
                kind,
                offset: start,
                len,
            });
        };
        match c {
            ' ' | '\t' | '\r' | '\n' => {
                i += 1;
            }
            '(' => {
                push(TokenKind::LParen, 1);
                i += 1;
            }
            ')' => {
                push(TokenKind::RParen, 1);
                i += 1;
            }
            '[' => {
                push(TokenKind::LBracket, 1);
                i += 1;
            }
            ']' => {
                push(TokenKind::RBracket, 1);
                i += 1;
            }
            ',' => {
                push(TokenKind::Comma, 1);
                i += 1;
            }
            '.' => {
                // A leading `.` followed by a digit is a number (`.5`).
                if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    let (n, len) = lex_number(source, i)?;
                    push(TokenKind::Number(n), len);
                    i += len;
                } else {
                    push(TokenKind::Dot, 1);
                    i += 1;
                }
            }
            '*' => {
                push(TokenKind::Star, 1);
                i += 1;
            }
            '!' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    push(TokenKind::Ne, 2);
                    i += 2;
                } else {
                    push(TokenKind::Not, 1);
                    i += 1;
                }
            }
            '<' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    push(TokenKind::Le, 2);
                    i += 2;
                } else {
                    push(TokenKind::Lt, 1);
                    i += 1;
                }
            }
            '>' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    push(TokenKind::Ge, 2);
                    i += 2;
                } else {
                    push(TokenKind::Gt, 1);
                    i += 1;
                }
            }
            '=' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    push(TokenKind::Eq, 2);
                    i += 2;
                } else {
                    return Err(LexError::Unexpected {
                        ch:     '=',
                        offset: i,
                    });
                }
            }
            '&' => {
                if bytes.get(i + 1) == Some(&b'&') {
                    push(TokenKind::And, 2);
                    i += 2;
                } else {
                    return Err(LexError::Unexpected {
                        ch:     '&',
                        offset: i,
                    });
                }
            }
            '|' => {
                if bytes.get(i + 1) == Some(&b'|') {
                    push(TokenKind::Or, 2);
                    i += 2;
                } else {
                    return Err(LexError::Unexpected {
                        ch:     '|',
                        offset: i,
                    });
                }
            }
            '\'' => {
                let mut j = i + 1;
                let mut text = String::new();
                loop {
                    match source[j..].chars().next() {
                        None => return Err(LexError::UnterminatedString { offset: i }),
                        Some('\'') => {
                            if bytes.get(j + 1) == Some(&b'\'') {
                                text.push('\'');
                                j += 2;
                            } else {
                                j += 1;
                                break;
                            }
                        }
                        Some(ch) => {
                            text.push(ch);
                            j += ch.len_utf8();
                        }
                    }
                }
                push(TokenKind::Str(text), j - i);
                i = j;
            }
            '-' | '0'..='9' => {
                let (n, len) = lex_number(source, i)?;
                push(TokenKind::Number(n), len);
                i += len;
            }
            c if is_ident_start(c) => {
                let mut j = i + 1;
                while j < bytes.len()
                    && is_ident_continue(source[j..].chars().next().unwrap_or('\0'))
                {
                    j += 1;
                }
                let word = &source[i..j];
                let kind = match word {
                    "null" => TokenKind::Null,
                    "true" => TokenKind::True,
                    "false" => TokenKind::False,
                    _ => TokenKind::Ident(word.to_string()),
                };
                push(kind, j - i);
                i = j;
            }
            other => {
                return Err(LexError::Unexpected {
                    ch:     other,
                    offset: i,
                });
            }
        }
    }
    tokens.push(Token {
        kind:   TokenKind::Eof,
        offset: source.len(),
        len:    0,
    });
    Ok(tokens)
}

/// A GitHub number: JSON's grammar plus `0x` hex, with an optional leading `-`.
fn lex_number(source: &str, start: usize) -> Result<(f64, usize), LexError> {
    let bytes = source.as_bytes();
    let mut i = start;
    if bytes.get(i) == Some(&b'-') {
        i += 1;
    }
    let body_start = i;
    // Hex.
    if bytes.get(i) == Some(&b'0') && matches!(bytes.get(i + 1), Some(b'x' | b'X')) {
        i += 2;
        let hex_start = i;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        if i == hex_start {
            return Err(LexError::BadNumber {
                text:   source[start..i].to_string(),
                offset: start,
            });
        }
        let magnitude =
            i64::from_str_radix(&source[hex_start..i], 16).map_err(|_| LexError::BadNumber {
                text:   source[start..i].to_string(),
                offset: start,
            })? as f64;
        let value = if bytes[start] == b'-' {
            -magnitude
        } else {
            magnitude
        };
        return Ok((value, i - start));
    }
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    // A `.` belongs to the number only when a digit follows: `1.5` is one token,
    // `1.x` is a number and then a property access.
    if bytes.get(i) == Some(&b'.') && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        let save = i;
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let digits = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == digits {
            i = save;
        }
    }
    let text = &source[start..i];
    if i == body_start {
        return Err(LexError::BadNumber {
            text:   text.to_string(),
            offset: start,
        });
    }
    text.parse::<f64>()
        .map(|n| (n, i - start))
        .map_err(|_| LexError::BadNumber {
            text:   text.to_string(),
            offset: start,
        })
}
