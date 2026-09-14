//! Tokens of the DOT subset, with positions.
//!
//! The lexer never panics on any input: every problem is a
//! [`LexError`] naming the position, which the parser turns into a
//! diagnostic.

use std::fmt;
use std::iter::Peekable;
use std::str::Chars;

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Tok {
    /// A bare word: an identifier, a keyword, a number, `900s`, `gpt-5.2`.
    Word(String),
    /// A double-quoted string, escapes resolved.
    Quoted(String),
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Eq,
    Comma,
    Semi,
    Colon,
    /// `->`
    Arrow,
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Word(w) => write!(f, "`{w}`"),
            Self::Quoted(s) => write!(f, "{s:?}"),
            Self::LBrace => f.write_str("`{`"),
            Self::RBrace => f.write_str("`}`"),
            Self::LBracket => f.write_str("`[`"),
            Self::RBracket => f.write_str("`]`"),
            Self::Eq => f.write_str("`=`"),
            Self::Comma => f.write_str("`,`"),
            Self::Semi => f.write_str("`;`"),
            Self::Colon => f.write_str("`:`"),
            Self::Arrow => f.write_str("`->`"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Token {
    pub tok:    Tok,
    pub line:   u32,
    pub column: u32,
}

/// What kind of text the lexer refused, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LexError {
    pub kind:   LexErrorKind,
    pub line:   u32,
    pub column: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LexErrorKind {
    UnterminatedString,
    UnterminatedComment,
    /// `<...>`: an HTML-like label.
    HtmlString,
    /// `--`: an undirected edge.
    UndirectedEdge,
    UnexpectedChar(char),
}

impl fmt::Display for LexErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnterminatedString => f.write_str("unterminated string"),
            Self::UnterminatedComment => f.write_str("unterminated `/*` comment"),
            Self::HtmlString => f.write_str("HTML-like `<...>` strings are not supported"),
            Self::UndirectedEdge => {
                f.write_str("`--` is an undirected edge; Fabro graphs are `digraph`s with `->`")
            }
            Self::UnexpectedChar(c) => write!(f, "unexpected character {c:?}"),
        }
    }
}

struct Cursor<'a> {
    chars:  Peekable<Chars<'a>>,
    line:   u32,
    column: u32,
}

impl Cursor<'_> {
    fn peek(&mut self) -> Option<char> {
        self.chars.peek().copied()
    }

    fn peek2(&self) -> Option<char> {
        let mut it = self.chars.clone();
        it.next();
        it.next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(c)
    }
}

fn is_punct(c: char) -> bool {
    matches!(
        c,
        '{' | '}' | '[' | ']' | '=' | ',' | ';' | ':' | '"' | '<' | '>'
    )
}

/// Tokenize `text`. Comments (`//`, `/* */`) are dropped; a `#` line is a C
/// preprocessor line in Graphviz and is dropped too.
pub(super) fn lex(text: &str) -> Result<Vec<Token>, LexError> {
    let mut cur = Cursor {
        chars:  text.chars().peekable(),
        line:   1,
        column: 1,
    };
    let mut out = Vec::new();
    let mut at_line_start = true;
    while let Some(c) = cur.peek() {
        let (line, column) = (cur.line, cur.column);
        let error = |kind| LexError { kind, line, column };
        if c == '\n' {
            cur.bump();
            at_line_start = true;
            continue;
        }
        if c.is_whitespace() {
            cur.bump();
            continue;
        }
        if c == '#' && at_line_start {
            while cur.peek().is_some_and(|c| c != '\n') {
                cur.bump();
            }
            continue;
        }
        at_line_start = false;
        if c == '/' && cur.peek2() == Some('/') {
            while cur.peek().is_some_and(|c| c != '\n') {
                cur.bump();
            }
            continue;
        }
        if c == '/' && cur.peek2() == Some('*') {
            cur.bump();
            cur.bump();
            loop {
                match cur.bump() {
                    None => return Err(error(LexErrorKind::UnterminatedComment)),
                    Some('*') if cur.peek() == Some('/') => {
                        cur.bump();
                        break;
                    }
                    Some(_) => {}
                }
            }
            continue;
        }
        let simple = match c {
            '{' => Some(Tok::LBrace),
            '}' => Some(Tok::RBrace),
            '[' => Some(Tok::LBracket),
            ']' => Some(Tok::RBracket),
            '=' => Some(Tok::Eq),
            ',' => Some(Tok::Comma),
            ';' => Some(Tok::Semi),
            ':' => Some(Tok::Colon),
            _ => None,
        };
        if let Some(tok) = simple {
            cur.bump();
            out.push(Token { tok, line, column });
            continue;
        }
        if c == '<' {
            return Err(error(LexErrorKind::HtmlString));
        }
        if c == '>' {
            return Err(error(LexErrorKind::UnexpectedChar(c)));
        }
        if c == '"' {
            cur.bump();
            let mut s = String::new();
            loop {
                match cur.bump() {
                    None => return Err(error(LexErrorKind::UnterminatedString)),
                    Some('"') => break,
                    Some('\\') => match cur.bump() {
                        None => return Err(error(LexErrorKind::UnterminatedString)),
                        Some('"') => s.push('"'),
                        Some('\\') => s.push('\\'),
                        Some('n') => s.push('\n'),
                        Some('t') => s.push('\t'),
                        // A backslash before a newline joins the lines, as in Graphviz.
                        Some('\n') => {}
                        // Graphviz keeps an unknown escape verbatim.
                        Some(other) => {
                            s.push('\\');
                            s.push(other);
                        }
                    },
                    Some(other) => s.push(other),
                }
            }
            out.push(Token {
                tok: Tok::Quoted(s),
                line,
                column,
            });
            continue;
        }
        if c == '-' {
            match cur.peek2() {
                Some('>') => {
                    cur.bump();
                    cur.bump();
                    out.push(Token {
                        tok: Tok::Arrow,
                        line,
                        column,
                    });
                    continue;
                }
                Some('-') => return Err(error(LexErrorKind::UndirectedEdge)),
                _ => {}
            }
        }
        if is_punct(c) || c == '/' {
            return Err(error(LexErrorKind::UnexpectedChar(c)));
        }
        // A word runs to whitespace, punctuation, a comment start, or an arrow.
        let mut word = String::new();
        while let Some(c) = cur.peek() {
            if c.is_whitespace() || is_punct(c) {
                break;
            }
            if c == '-' && cur.peek2() == Some('>') {
                break;
            }
            if c == '/' && matches!(cur.peek2(), Some('/' | '*')) {
                break;
            }
            word.push(c);
            cur.bump();
        }
        if word.is_empty() {
            return Err(error(LexErrorKind::UnexpectedChar(c)));
        }
        out.push(Token {
            tok: Tok::Word(word),
            line,
            column,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<Tok> {
        lex(text)
            .expect("lexes")
            .into_iter()
            .map(|t| t.tok)
            .collect()
    }

    #[test]
    fn lexes_words_arrows_and_punctuation() {
        assert_eq!(words("a -> b [x=1]"), vec![
            Tok::Word("a".into()),
            Tok::Arrow,
            Tok::Word("b".into()),
            Tok::LBracket,
            Tok::Word("x".into()),
            Tok::Eq,
            Tok::Word("1".into()),
            Tok::RBracket,
        ]);
    }

    #[test]
    fn a_dash_inside_a_word_is_part_of_it() {
        assert_eq!(words("model=gpt-5.2-codex"), vec![
            Tok::Word("model".into()),
            Tok::Eq,
            Tok::Word("gpt-5.2-codex".into()),
        ]);
    }

    #[test]
    fn strings_resolve_escapes_and_keep_positions() {
        let tokens = lex("x=\"a\\\"b\\nc\"").expect("lexes");
        assert_eq!(tokens[2].tok, Tok::Quoted("a\"b\nc".into()));
        assert_eq!((tokens[2].line, tokens[2].column), (1, 3));
    }

    #[test]
    fn comments_are_dropped() {
        assert_eq!(words("// c\na /* b */ b"), vec![
            Tok::Word("a".into()),
            Tok::Word("b".into())
        ]);
    }

    #[test]
    fn refuses_html_undirected_and_unterminated() {
        assert_eq!(lex("<b>").expect_err("html").kind, LexErrorKind::HtmlString);
        assert_eq!(
            lex("a -- b").expect_err("undirected").kind,
            LexErrorKind::UndirectedEdge
        );
        assert_eq!(
            lex("\"open").expect_err("string").kind,
            LexErrorKind::UnterminatedString
        );
        assert_eq!(
            lex("/* open").expect_err("comment").kind,
            LexErrorKind::UnterminatedComment
        );
    }
}
