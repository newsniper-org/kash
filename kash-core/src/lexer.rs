//! Tokeniser for the kash source language.
//!
//! Turns a `&str` source buffer into a stream of `Token`s. Hand-written,
//! single-pass, byte-driven; no allocation per token beyond the buffer
//! a `Word` / quoted-string variant carries. Implementation aims to be
//! `no_std + alloc` friendly (no `std::io`, no regex).
//!
//! Scope of this commit: the bottom layer needed to parse POSIX
//! command syntax — words, single/double/`$'...'` ANSI-C quoted
//! strings, comments, newlines, and the common operators (`;`, `|`,
//! `&`, `<`, `>`, `(`, `)`, `{`, `}` and their multi-char
//! combinations). Kash-specific keywords (`mode`, `namespace`, `use`,
//! `typeclass`, `instance`, `yield`, …) are still emitted as plain
//! `Word`s; the parser distinguishes them by context.
//!
//! Not yet handled: parameter expansion internals, here-docs,
//! arithmetic-context tokenisation, glob qualifiers. Those land in
//! follow-up commits as the parser grows.

use alloc::string::String;
use core::fmt;

use crate::error::KashError;

/// Byte-offset range into the source buffer, used by every `Token`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// Inclusive start byte offset.
    pub start: usize,
    /// Exclusive end byte offset.
    pub end: usize,
}

impl Span {
    /// Construct a span from a half-open byte range.
    #[inline]
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Zero-length span at the given offset.
    #[inline]
    #[must_use]
    pub const fn point(at: usize) -> Self {
        Self {
            start: at,
            end: at,
        }
    }
}

/// Lexical token kind.
///
/// Variant payloads carry the raw text for `Word` and quoted strings;
/// quote stripping has *not* been done (the parser / expander decides
/// when and how to unquote so it can preserve provenance for error
/// messages).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TokenKind {
    /// A bare word — identifier, command name, literal argument, etc.
    /// May contain `$`, `\\`, `=`, and most printable characters.
    Word(String),
    /// `'...'` — interior bytes verbatim, no expansion.
    SingleQuoted(String),
    /// `"..."` — interior bytes verbatim (the expander handles
    /// `$var` / `$(cmd)` / `\\` later).
    DoubleQuoted(String),
    /// `$'...'` — ANSI-C quoted string, escape sequences not yet
    /// processed.
    AnsiCString(String),
    /// A control / redirection operator (see [`Op`]).
    Op(Op),
    /// `\n` — also a command separator.
    Newline,
    /// End of input.
    Eof,
}

/// Concrete control / redirection operator carried by `TokenKind::Op`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Op {
    /// `;`
    Semi,
    /// `;;`  — `case` clause terminator
    DoubleSemi,
    /// `;&`  — `case` fall-through (bash/zsh)
    SemiAmp,
    /// `;;&` — `case` continue-match (bash/zsh)
    DoubleSemiAmp,
    /// `|`
    Pipe,
    /// `||`
    DoublePipe,
    /// `|&`  — coprocess (ksh93 baseline; see
    /// `project_shell_subshell_pipeline.md`)
    PipeAmp,
    /// `&`
    Amp,
    /// `&&`
    DoubleAmp,
    /// `&>`  — bash-style stdout+stderr redirect
    AmpGt,
    /// `&>>` — bash-style stdout+stderr append
    AmpGtGt,
    /// `<`
    Lt,
    /// `<<`
    DoubleLt,
    /// `<<-`
    DoubleLtDash,
    /// `<<<` — here-string
    TripleLt,
    /// `<&`
    LtAmp,
    /// `<>`
    LtGt,
    /// `>`
    Gt,
    /// `>>`
    DoubleGt,
    /// `>&`
    GtAmp,
    /// `>|`  — clobber override
    GtPipe,
    /// `(`
    Lparen,
    /// `)`
    Rparen,
    /// `{`
    Lbrace,
    /// `}`
    Rbrace,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Semi => ";",
            Self::DoubleSemi => ";;",
            Self::SemiAmp => ";&",
            Self::DoubleSemiAmp => ";;&",
            Self::Pipe => "|",
            Self::DoublePipe => "||",
            Self::PipeAmp => "|&",
            Self::Amp => "&",
            Self::DoubleAmp => "&&",
            Self::AmpGt => "&>",
            Self::AmpGtGt => "&>>",
            Self::Lt => "<",
            Self::DoubleLt => "<<",
            Self::DoubleLtDash => "<<-",
            Self::TripleLt => "<<<",
            Self::LtAmp => "<&",
            Self::LtGt => "<>",
            Self::Gt => ">",
            Self::DoubleGt => ">>",
            Self::GtAmp => ">&",
            Self::GtPipe => ">|",
            Self::Lparen => "(",
            Self::Rparen => ")",
            Self::Lbrace => "{",
            Self::Rbrace => "}",
        };
        f.write_str(s)
    }
}

/// A token with its source span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// Lexical kind + payload.
    pub kind: TokenKind,
    /// Byte-offset range in the source.
    pub span: Span,
}

/// Hand-written tokeniser.
///
/// `Lexer::next_token` returns the next token (with `TokenKind::Eof`
/// signalling end of input). The lexer also implements `Iterator` —
/// `Item = Result<Token, KashError>`. The iterator stops *after*
/// yielding a single `Eof` token so consumers that ignore the
/// `Eof` see a normal end-of-stream.
pub struct Lexer<'src> {
    src: &'src str,
    bytes: &'src [u8],
    pos: usize,
    finished: bool,
}

impl<'src> Lexer<'src> {
    /// Wrap a source buffer.
    #[inline]
    #[must_use]
    pub fn new(src: &'src str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            finished: false,
        }
    }

    /// Current byte position in the source buffer. Exposed so the
    /// parser can record source spans for here-doc bodies that the
    /// lexer reads via [`read_heredoc_body`](Self::read_heredoc_body).
    #[inline]
    #[must_use]
    pub fn byte_pos(&self) -> usize {
        self.pos
    }

    /// Original source buffer. Exposed for source-line lookups
    /// (e.g. `.sh.lineno`).
    #[inline]
    #[must_use]
    pub fn source(&self) -> &'src str {
        self.src
    }

    /// Peek the byte at the given offset, returning `None` past EOF.
    #[inline]
    fn peek_at(&self, off: usize) -> Option<u8> {
        self.bytes.get(self.pos + off).copied()
    }

    /// Peek the next byte (no consumption).
    #[inline]
    fn peek(&self) -> Option<u8> {
        self.peek_at(0)
    }

    /// Consume and return the next byte.
    #[inline]
    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    /// Read a here-document body. The caller is responsible for
    /// having already consumed the `<<DELIM` (or `<<-DELIM`) plus the
    /// terminating newline of the introducer line; we begin reading at
    /// the first byte of the line after that.
    ///
    /// `delim` is the closing-line marker. `strip_tabs` (the `<<-`
    /// form) removes a run of `'\t'` characters from the start of
    /// every body line *and* from the candidate delimiter line before
    /// the comparison.
    ///
    /// Returns the raw body text, with newlines preserved between
    /// lines but no trailing newline on the final body line before
    /// the delimiter.
    pub fn read_heredoc_body(
        &mut self,
        delim: &str,
        strip_tabs: bool,
    ) -> Result<alloc::string::String, KashError> {
        use alloc::string::String;
        let mut body = String::new();
        loop {
            let line_start = self.pos;
            while let Some(b) = self.peek() {
                if b == b'\n' {
                    break;
                }
                self.pos += 1;
            }
            let raw_line = &self.src[line_start..self.pos];
            let line = if strip_tabs {
                raw_line.trim_start_matches('\t')
            } else {
                raw_line
            };
            let at_eof = self.peek().is_none();
            // Consume the newline (if any) that ends this line.
            if !at_eof {
                self.pos += 1;
            }
            if line == delim {
                return Ok(body);
            }
            if at_eof {
                return Err(KashError::Parse(alloc::format!(
                    "unterminated here-document; missing closing `{delim}`"
                )));
            }
            body.push_str(line);
            body.push('\n');
        }
    }

    /// Skip horizontal whitespace (`' '`, `'\t'`) and full-line `# ...`
    /// comments. Newline is *not* skipped — it's a token.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t') => {
                    self.pos += 1;
                }
                Some(b'\\') if self.peek_at(1) == Some(b'\n') => {
                    // Line continuation: `\\` followed by `\n` is folded out.
                    self.pos += 2;
                }
                Some(b'#') => {
                    // Comment runs to end of line (or end of input).
                    while let Some(b) = self.peek() {
                        if b == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    /// Read the next token. Returns `TokenKind::Eof` once the source
    /// is exhausted; further calls keep returning `Eof`.
    pub fn next_token(&mut self) -> Result<Token, KashError> {
        self.skip_trivia();
        let start = self.pos;
        let Some(b) = self.peek() else {
            return Ok(Token {
                kind: TokenKind::Eof,
                span: Span::point(start),
            });
        };

        // Newline first.
        if b == b'\n' {
            self.pos += 1;
            return Ok(Token {
                kind: TokenKind::Newline,
                span: Span::new(start, self.pos),
            });
        }

        // Quoted strings.
        if b == b'\'' {
            return self.read_single_quoted(start);
        }
        if b == b'"' {
            return self.read_double_quoted(start);
        }
        if b == b'$' && self.peek_at(1) == Some(b'\'') {
            return self.read_ansi_c(start);
        }

        // Operators.
        if let Some(op) = self.read_operator() {
            return Ok(Token {
                kind: TokenKind::Op(op),
                span: Span::new(start, self.pos),
            });
        }

        // Otherwise: a word.
        self.read_word(start)
    }

    fn read_single_quoted(&mut self, start: usize) -> Result<Token, KashError> {
        // Consume opening '
        self.pos += 1;
        let body_start = self.pos;
        loop {
            match self.bump() {
                Some(b'\'') => {
                    let body = self.src[body_start..self.pos - 1].into();
                    return Ok(Token {
                        kind: TokenKind::SingleQuoted(body),
                        span: Span::new(start, self.pos),
                    });
                }
                Some(_) => continue,
                None => {
                    return Err(KashError::Parse(alloc::format!(
                        "unterminated single-quoted string starting at byte {start}"
                    )));
                }
            }
        }
    }

    fn read_double_quoted(&mut self, start: usize) -> Result<Token, KashError> {
        // Consume opening "
        self.pos += 1;
        let body_start = self.pos;
        loop {
            match self.peek() {
                Some(b'"') => {
                    let body = self.src[body_start..self.pos].into();
                    self.pos += 1;
                    return Ok(Token {
                        kind: TokenKind::DoubleQuoted(body),
                        span: Span::new(start, self.pos),
                    });
                }
                Some(b'\\') => {
                    // Skip backslash-escaped char inside double quotes.
                    self.pos += 1;
                    if self.peek().is_some() {
                        self.pos += 1;
                    }
                }
                Some(b'$') if self.peek_at(1) == Some(b'(') => {
                    // `$(...)` inside double quotes — track nested
                    // parens so a `"` inside the sub-command body
                    // doesn't terminate the outer string.
                    self.pos += 2;
                    let mut depth = 1usize;
                    while let Some(c) = self.peek() {
                        self.pos += 1;
                        if c == b'(' {
                            depth += 1;
                        } else if c == b')' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                    }
                    if depth != 0 {
                        return Err(KashError::Parse(alloc::format!(
                            "unterminated `$(...)` inside double-quoted string at byte {start}"
                        )));
                    }
                }
                Some(b'`') => {
                    // Backtick command substitution inside double
                    // quotes — skip until the matching backtick.
                    self.pos += 1;
                    while let Some(c) = self.peek() {
                        if c == b'`' {
                            self.pos += 1;
                            break;
                        }
                        if c == b'\\' {
                            self.pos += 1;
                            if self.peek().is_some() {
                                self.pos += 1;
                            }
                            continue;
                        }
                        self.pos += 1;
                    }
                }
                Some(_) => self.pos += 1,
                None => {
                    return Err(KashError::Parse(alloc::format!(
                        "unterminated double-quoted string starting at byte {start}"
                    )));
                }
            }
        }
    }

    fn read_ansi_c(&mut self, start: usize) -> Result<Token, KashError> {
        // Consume "$'"
        self.pos += 2;
        let body_start = self.pos;
        loop {
            match self.peek() {
                Some(b'\'') => {
                    let body = self.src[body_start..self.pos].into();
                    self.pos += 1;
                    return Ok(Token {
                        kind: TokenKind::AnsiCString(body),
                        span: Span::new(start, self.pos),
                    });
                }
                Some(b'\\') => {
                    self.pos += 1;
                    if self.peek().is_some() {
                        self.pos += 1;
                    }
                }
                Some(_) => self.pos += 1,
                None => {
                    return Err(KashError::Parse(alloc::format!(
                        "unterminated $'...' string starting at byte {start}"
                    )));
                }
            }
        }
    }

    /// Try to read an operator starting at `self.pos`. Returns `None`
    /// if no operator applies (caller will treat the byte as part of
    /// a word).
    fn read_operator(&mut self) -> Option<Op> {
        let b0 = self.peek()?;
        let b1 = self.peek_at(1);
        let b2 = self.peek_at(2);
        let (op, len) = match (b0, b1, b2) {
            (b';', Some(b';'), Some(b'&')) => (Op::DoubleSemiAmp, 3),
            (b';', Some(b';'), _) => (Op::DoubleSemi, 2),
            (b';', Some(b'&'), _) => (Op::SemiAmp, 2),
            (b';', _, _) => (Op::Semi, 1),

            (b'|', Some(b'|'), _) => (Op::DoublePipe, 2),
            (b'|', Some(b'&'), _) => (Op::PipeAmp, 2),
            (b'|', _, _) => (Op::Pipe, 1),

            (b'&', Some(b'&'), _) => (Op::DoubleAmp, 2),
            (b'&', Some(b'>'), Some(b'>')) => (Op::AmpGtGt, 3),
            (b'&', Some(b'>'), _) => (Op::AmpGt, 2),
            (b'&', _, _) => (Op::Amp, 1),

            (b'<', Some(b'<'), Some(b'<')) => (Op::TripleLt, 3),
            (b'<', Some(b'<'), Some(b'-')) => (Op::DoubleLtDash, 3),
            (b'<', Some(b'<'), _) => (Op::DoubleLt, 2),
            (b'<', Some(b'&'), _) => (Op::LtAmp, 2),
            (b'<', Some(b'>'), _) => (Op::LtGt, 2),
            (b'<', _, _) => (Op::Lt, 1),

            (b'>', Some(b'>'), _) => (Op::DoubleGt, 2),
            (b'>', Some(b'&'), _) => (Op::GtAmp, 2),
            (b'>', Some(b'|'), _) => (Op::GtPipe, 2),
            (b'>', _, _) => (Op::Gt, 1),

            (b'(', _, _) => (Op::Lparen, 1),
            (b')', _, _) => (Op::Rparen, 1),
            (b'{', _, _) => (Op::Lbrace, 1),
            (b'}', _, _) => (Op::Rbrace, 1),

            _ => return None,
        };
        self.pos += len;
        Some(op)
    }

    fn read_word(&mut self, start: usize) -> Result<Token, KashError> {
        while let Some(b) = self.peek() {
            if is_word_byte(b) {
                // `${...}` parameter expansion: brace-delimited, may
                // contain word terminators (`}` in the brace body is
                // not a closer for the outer word). Absorb the whole
                // brace body so the parser sees one `Word` token.
                if b == b'$' && self.peek_at(1) == Some(b'{') {
                    self.pos += 2; // `${`
                    let mut depth = 1usize;
                    while let Some(c) = self.peek() {
                        self.pos += 1;
                        if c == b'{' {
                            depth += 1;
                        } else if c == b'}' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                    }
                    if depth != 0 {
                        return Err(KashError::Parse(alloc::format!(
                            "unterminated `${{...}}` at offset {start}"
                        )));
                    }
                    continue;
                }
                if b == b'$' && self.peek_at(1) == Some(b'(') {
                    // `$(...)` command substitution. Nested
                    // parentheses are tracked so that e.g.
                    // `$(echo $(date))` lexes as one Word.
                    self.pos += 2; // `$(`
                    let mut depth = 1usize;
                    while let Some(c) = self.peek() {
                        self.pos += 1;
                        if c == b'(' {
                            depth += 1;
                        } else if c == b')' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                    }
                    if depth != 0 {
                        return Err(KashError::Parse(alloc::format!(
                            "unterminated `$(...)` at offset {start}"
                        )));
                    }
                    continue;
                }
                // ksh93 / bash extglob: `?(p)`, `*(p)`, `+(p)`,
                // `@(p)`, `!(p)`. Absorb the whole parenthesised
                // body into the current word so the case-pattern /
                // [[…]] matchers see one token.
                if matches!(b, b'?' | b'*' | b'+' | b'@' | b'!')
                    && self.peek_at(1) == Some(b'(')
                {
                    self.pos += 2; // leader + `(`
                    let mut depth = 1usize;
                    while let Some(c) = self.peek() {
                        self.pos += 1;
                        if c == b'(' {
                            depth += 1;
                        } else if c == b')' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                    }
                    if depth != 0 {
                        return Err(KashError::Parse(alloc::format!(
                            "unterminated extglob at offset {start}"
                        )));
                    }
                    continue;
                }
                if b == b'`' {
                    // Backtick command substitution. No nesting (the
                    // POSIX backtick form famously doesn't nest);
                    // `\\` inside escapes the next byte (so `\\` `\``
                    // / `\\$` survive).
                    self.pos += 1;
                    while let Some(c) = self.peek() {
                        if c == b'`' {
                            self.pos += 1;
                            break;
                        }
                        if c == b'\\' {
                            self.pos += 1;
                            if self.peek().is_some() {
                                self.pos += 1;
                            }
                            continue;
                        }
                        self.pos += 1;
                    }
                    continue;
                }
                if b == b'$' {
                    // `$` followed by one of the single-byte specials
                    // (`?`, `#`, `$`, `!`, `@`, `*`, or `0`-`9`) is a
                    // single parameter reference. The trailing byte is
                    // *not* a word-byte on its own (`#` would start a
                    // comment, etc.), so absorb it explicitly here.
                    if let Some(n) = self.peek_at(1)
                        && matches!(n, b'?' | b'#' | b'$' | b'!' | b'@' | b'*' | b'0'..=b'9')
                    {
                        self.pos += 2;
                        continue;
                    }
                }
                self.pos += 1;
            } else if b == b'\\' {
                if self.peek_at(1) == Some(b'\n') {
                    // Line continuation inside a word: end the word
                    // here and let `skip_trivia` fold the `\\\n` on the
                    // next call to `next_token`.
                    break;
                }
                // Backslash-escape: consume the backslash + next byte (if any)
                // as part of the word.
                self.pos += 1;
                if self.peek().is_some() {
                    self.pos += 1;
                }
            } else if b == b'{' && self.pos > start {
                // Brace group *inside* a running word — e.g. the
                // `{a,b}` half of `.utils.{a,b}` for `use`. Absorb
                // up to the matching `}` (balanced, with nesting)
                // so the whole construct lands in a single Word
                // token. A `{` at word *start* is still a brace-
                // group opener and breaks out of word-reading.
                self.pos += 1;
                let mut depth = 1usize;
                while let Some(c) = self.peek() {
                    self.pos += 1;
                    if c == b'{' {
                        depth += 1;
                    } else if c == b'}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
                if depth != 0 {
                    return Err(KashError::Parse(alloc::format!(
                        "unterminated `{{…}}` group inside word at offset {start}"
                    )));
                }
            } else {
                break;
            }
        }
        if self.pos == start {
            // Defensive: a byte we don't recognise as word/operator/quote/whitespace.
            return Err(KashError::Parse(alloc::format!(
                "unexpected byte 0x{:02x} at offset {start}",
                self.bytes[start],
            )));
        }
        Ok(Token {
            kind: TokenKind::Word(self.src[start..self.pos].into()),
            span: Span::new(start, self.pos),
        })
    }
}

/// True if `b` belongs to a bare word — i.e. is not whitespace, a
/// newline, an operator, a comment marker, or a quote character.
#[inline]
const fn is_word_byte(b: u8) -> bool {
    !matches!(
        b,
        b' ' | b'\t' | b'\n'
        | b';' | b'|' | b'&' | b'<' | b'>'
        | b'(' | b')' | b'{' | b'}'
        | b'\'' | b'"' | b'\\' | b'#'
    )
}

impl Iterator for Lexer<'_> {
    type Item = Result<Token, KashError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let result = self.next_token();
        if let Ok(Token {
            kind: TokenKind::Eof,
            ..
        }) = result
        {
            self.finished = true;
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn lex(src: &str) -> Vec<TokenKind> {
        Lexer::new(src)
            .filter_map(|r| r.ok())
            .map(|t| t.kind)
            .collect()
    }

    #[test]
    fn empty_input_yields_only_eof() {
        let kinds = lex("");
        assert_eq!(kinds, alloc::vec![TokenKind::Eof]);
    }

    #[test]
    fn simple_word() {
        let kinds = lex("echo");
        assert_eq!(
            kinds,
            alloc::vec![TokenKind::Word("echo".into()), TokenKind::Eof],
        );
    }

    #[test]
    fn multiple_words_split_by_whitespace() {
        let kinds = lex("echo hello world");
        assert_eq!(
            kinds,
            alloc::vec![
                TokenKind::Word("echo".into()),
                TokenKind::Word("hello".into()),
                TokenKind::Word("world".into()),
                TokenKind::Eof,
            ],
        );
    }

    #[test]
    fn newline_is_its_own_token() {
        let kinds = lex("a\nb");
        assert_eq!(
            kinds,
            alloc::vec![
                TokenKind::Word("a".into()),
                TokenKind::Newline,
                TokenKind::Word("b".into()),
                TokenKind::Eof,
            ],
        );
    }

    #[test]
    fn comment_skipped_until_newline() {
        let kinds = lex("a # comment\nb");
        assert_eq!(
            kinds,
            alloc::vec![
                TokenKind::Word("a".into()),
                TokenKind::Newline,
                TokenKind::Word("b".into()),
                TokenKind::Eof,
            ],
        );
    }

    #[test]
    fn line_continuation_folded() {
        let kinds = lex("a\\\nb");
        assert_eq!(
            kinds,
            alloc::vec![
                TokenKind::Word("a".into()),
                TokenKind::Word("b".into()),
                TokenKind::Eof,
            ],
        );
    }

    #[test]
    fn single_quotes_preserve_body() {
        let kinds = lex("'hi there'");
        assert_eq!(
            kinds,
            alloc::vec![
                TokenKind::SingleQuoted("hi there".into()),
                TokenKind::Eof,
            ],
        );
    }

    #[test]
    fn double_quotes_preserve_body() {
        let kinds = lex(r#""hi $name""#);
        assert_eq!(
            kinds,
            alloc::vec![
                TokenKind::DoubleQuoted(r#"hi $name"#.into()),
                TokenKind::Eof,
            ],
        );
    }

    #[test]
    fn ansi_c_quotes_lex_body() {
        let kinds = lex(r#"$'a\nb'"#);
        assert_eq!(
            kinds,
            alloc::vec![
                TokenKind::AnsiCString(r#"a\nb"#.into()),
                TokenKind::Eof,
            ],
        );
    }

    #[test]
    fn single_char_operators() {
        // Space-separated so e.g. `|&` doesn't collapse into the coprocess
        // operator. (Adjacent-operator lexing is exercised by
        // `coproc_operator_lexed_as_pipe_amp` below.)
        let kinds = lex("; | & < > ( ) { }");
        assert_eq!(
            kinds,
            alloc::vec![
                TokenKind::Op(Op::Semi),
                TokenKind::Op(Op::Pipe),
                TokenKind::Op(Op::Amp),
                TokenKind::Op(Op::Lt),
                TokenKind::Op(Op::Gt),
                TokenKind::Op(Op::Lparen),
                TokenKind::Op(Op::Rparen),
                TokenKind::Op(Op::Lbrace),
                TokenKind::Op(Op::Rbrace),
                TokenKind::Eof,
            ],
        );
    }

    #[test]
    fn coproc_operator_lexed_as_pipe_amp() {
        // Per `project_shell_subshell_pipeline.md`, `|&` is the coprocess
        // operator across all modes (ksh93 baseline), not `|` + `&`.
        let kinds = lex("foo |& bar");
        assert_eq!(
            kinds,
            alloc::vec![
                TokenKind::Word("foo".into()),
                TokenKind::Op(Op::PipeAmp),
                TokenKind::Word("bar".into()),
                TokenKind::Eof,
            ],
        );
    }

    #[test]
    fn multi_char_operators() {
        let kinds = lex(";; ;& ;;& || |& && &> &>> << <<- <<< <& <> >> >& >|");
        assert_eq!(
            kinds,
            alloc::vec![
                TokenKind::Op(Op::DoubleSemi),
                TokenKind::Op(Op::SemiAmp),
                TokenKind::Op(Op::DoubleSemiAmp),
                TokenKind::Op(Op::DoublePipe),
                TokenKind::Op(Op::PipeAmp),
                TokenKind::Op(Op::DoubleAmp),
                TokenKind::Op(Op::AmpGt),
                TokenKind::Op(Op::AmpGtGt),
                TokenKind::Op(Op::DoubleLt),
                TokenKind::Op(Op::DoubleLtDash),
                TokenKind::Op(Op::TripleLt),
                TokenKind::Op(Op::LtAmp),
                TokenKind::Op(Op::LtGt),
                TokenKind::Op(Op::DoubleGt),
                TokenKind::Op(Op::GtAmp),
                TokenKind::Op(Op::GtPipe),
                TokenKind::Eof,
            ],
        );
    }

    #[test]
    fn pipeline_sample() {
        let kinds = lex("echo hello | grep h ; ls");
        assert_eq!(
            kinds,
            alloc::vec![
                TokenKind::Word("echo".into()),
                TokenKind::Word("hello".into()),
                TokenKind::Op(Op::Pipe),
                TokenKind::Word("grep".into()),
                TokenKind::Word("h".into()),
                TokenKind::Op(Op::Semi),
                TokenKind::Word("ls".into()),
                TokenKind::Eof,
            ],
        );
    }

    #[test]
    fn unterminated_single_quote_errors() {
        let mut lx = Lexer::new("'oops");
        let err = lx.next_token().unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("unterminated"), "got: {msg}");
    }

    #[test]
    fn span_tracks_byte_offsets() {
        let mut lx = Lexer::new("ab cd");
        let t1 = lx.next_token().unwrap();
        assert_eq!(t1.span, Span::new(0, 2));
        let t2 = lx.next_token().unwrap();
        assert_eq!(t2.span, Span::new(3, 5));
    }
}
