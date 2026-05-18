//! Parser — token stream → AST.
//!
//! Hand-written recursive descent. Produces the AST defined in `ast.rs`,
//! which downstream consumers (evaluator, transpiler plugins,
//! formatter) walk. Per `project_kash_implementation.md`, no external
//! parser-combinator crate is used — error messages and recovery are
//! tuned for the REPL.
//!
//! Scope of this commit: the POSIX command grammar up through
//! compound commands — simple command, pipeline (`|` / `|&`), AND-OR
//! list (`&&` / `||`), `{ ... }` brace groups, `( ... )` subshells,
//! `if`/`elif`/`else`, `while`/`until`, `for`, and `case`. A minimal
//! redirect subset (`>`, `>>`, `<`, `&>`, `&>>`) is supported on both
//! simple and compound commands. Function definitions, assignment
//! prefixes, here-docs, FD-dups, `!` negation, and kash-specific
//! declarations are still deferred.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::ast::{
    AndOrList, AndOrOp, Assignment, CaseFallthrough, CaseItem, Command, CompoundCommand,
    CompoundKind, FunctionScope, IfBranch, PipeOp, Pipeline, Program, Redirect, RedirectKind,
    SimpleCommand, Statement, Terminator, Word, WordSegment,
};
use crate::error::{KashError, Result};
use crate::lexer::{Lexer, Op, Span, Token, TokenKind};

/// Recursive-descent parser for the kash source language.
///
/// One-token lookahead through `peeked`. Construct with [`Parser::new`]
/// for incremental control, or call the [`parse`] free function for
/// a one-shot source string → [`Program`].
pub struct Parser<'src> {
    lexer: Lexer<'src>,
    /// Lookahead buffer. Holds up to a few tokens — the parser peeks
    /// at index 1 / 2 when distinguishing `name() { … }` (function
    /// definition) from `name args …` (simple command).
    peeked: Vec<Token>,
}

/// One-shot convenience: parse a complete source unit.
pub fn parse(source: &str) -> Result<Program> {
    Parser::new(source).parse_program()
}

// ===== reserved words =====

/// Shell reserved words that *only* take their special meaning when
/// they appear in command position (start of a simple command).
/// Recognising them is the parser's job — the lexer always emits them
/// as plain `Word` tokens.
fn reserved_word(s: &str) -> Option<Reserved> {
    Some(match s {
        "if" => Reserved::If,
        "then" => Reserved::Then,
        "elif" => Reserved::Elif,
        "else" => Reserved::Else,
        "fi" => Reserved::Fi,
        "while" => Reserved::While,
        "until" => Reserved::Until,
        "do" => Reserved::Do,
        "done" => Reserved::Done,
        "for" => Reserved::For,
        "in" => Reserved::In,
        "case" => Reserved::Case,
        "esac" => Reserved::Esac,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reserved {
    If,
    Then,
    Elif,
    Else,
    Fi,
    While,
    Until,
    Do,
    Done,
    For,
    In,
    Case,
    Esac,
}

impl Reserved {
    fn name(self) -> &'static str {
        match self {
            Self::If => "if",
            Self::Then => "then",
            Self::Elif => "elif",
            Self::Else => "else",
            Self::Fi => "fi",
            Self::While => "while",
            Self::Until => "until",
            Self::Do => "do",
            Self::Done => "done",
            Self::For => "for",
            Self::In => "in",
            Self::Case => "case",
            Self::Esac => "esac",
        }
    }
}

impl<'src> Parser<'src> {
    /// New parser over `source`. Does not advance the lexer.
    #[must_use]
    pub fn new(source: &'src str) -> Self {
        Self {
            lexer: Lexer::new(source),
            peeked: Vec::new(),
        }
    }

    // ---------- token plumbing ----------

    fn peek(&mut self) -> Result<&Token> {
        self.peek_at(0)
    }

    fn peek_kind(&mut self) -> Result<&TokenKind> {
        Ok(&self.peek()?.kind)
    }

    /// Peek the token at offset `n` from the current position. `n=0`
    /// is the next token; `n=1` is the one after that; …
    fn peek_at(&mut self, n: usize) -> Result<&Token> {
        while self.peeked.len() <= n {
            let tok = self.lexer.next_token()?;
            self.peeked.push(tok);
        }
        Ok(&self.peeked[n])
    }

    fn bump(&mut self) -> Result<Token> {
        if !self.peeked.is_empty() {
            return Ok(self.peeked.remove(0));
        }
        self.lexer.next_token()
    }

    /// Peek the bare-word body, if the next token is `Word(_)`. Quoted
    /// words never count as a reserved keyword — `'if'` is just text.
    fn peek_bare_word(&mut self) -> Result<Option<&str>> {
        Ok(match self.peek_kind()? {
            TokenKind::Word(s) => Some(s.as_str()),
            _ => None,
        })
    }

    /// What reserved keyword is at the current position, if any.
    fn peek_reserved(&mut self) -> Result<Option<Reserved>> {
        Ok(self.peek_bare_word()?.and_then(reserved_word))
    }

    fn skip_newlines(&mut self) -> Result<()> {
        while matches!(self.peek_kind()?, TokenKind::Newline) {
            self.bump()?;
        }
        Ok(())
    }

    /// Skip any statement separator(s) (`;`, `&`, newline) between
    /// statements inside a compound body. Returns whether we just ate
    /// a `&` — callers use this to override the trailing terminator.
    fn skip_statement_separators(&mut self) -> Result<()> {
        loop {
            match self.peek_kind()? {
                TokenKind::Newline | TokenKind::Op(Op::Semi) => {
                    self.bump()?;
                }
                _ => break,
            }
        }
        Ok(())
    }

    /// Expect a bare-word reserved keyword. Consumes it; errors with a
    /// helpful message otherwise.
    fn expect_reserved(&mut self, want: Reserved) -> Result<Token> {
        if self.peek_reserved()? == Some(want) {
            return self.bump();
        }
        let got = match self.peek_kind()? {
            TokenKind::Word(s) => format!("`{s}`"),
            TokenKind::Op(op) => format!("{op:?}"),
            other => format!("{other:?}"),
        };
        Err(KashError::Parse(format!(
            "expected `{}`, got {}",
            want.name(),
            got
        )))
    }

    // ---------- entry point ----------

    /// Parse a full source unit (zero or more statements).
    pub fn parse_program(&mut self) -> Result<Program> {
        let mut statements = Vec::new();
        loop {
            self.skip_newlines()?;
            if matches!(self.peek_kind()?, TokenKind::Eof) {
                break;
            }
            statements.push(self.parse_statement(StatementContext::Top)?);
        }
        Ok(Program { statements })
    }

    // ---------- statements ----------

    fn parse_statement(&mut self, ctx: StatementContext) -> Result<Statement> {
        let list = self.parse_and_or()?;
        let start = list.span.start;
        // Pre-compute whether the next token is a reserved keyword so
        // the match below doesn't need a second mutable borrow.
        let here_is_reserved = self.peek_reserved()?.is_some();
        let (term, end) = match self.peek_kind()? {
            TokenKind::Op(Op::Semi) => {
                let tok = self.bump()?;
                (Terminator::Sync, tok.span.end)
            }
            TokenKind::Op(Op::Amp) => {
                let tok = self.bump()?;
                (Terminator::Background, tok.span.end)
            }
            TokenKind::Newline => {
                let tok = self.bump()?;
                (Terminator::Sync, tok.span.end)
            }
            TokenKind::Eof => (Terminator::Sync, list.span.end),
            // Inside a case arm we may bump up against `;;` / `;&` /
            // `;;&` — those are the arm's fall-through marker, not our
            // problem; leave them for `parse_case_item`.
            TokenKind::Op(Op::DoubleSemi)
            | TokenKind::Op(Op::SemiAmp)
            | TokenKind::Op(Op::DoubleSemiAmp)
                if ctx == StatementContext::CaseArm =>
            {
                (Terminator::Sync, list.span.end)
            }
            // Inside a compound body, a reserved word in command
            // position (`fi`, `done`, `esac`, `then`, `else`, `elif`)
            // ends the current statement without being consumed.
            TokenKind::Word(_) if here_is_reserved && ctx != StatementContext::Top => {
                (Terminator::Sync, list.span.end)
            }
            // `)` / `}` close the surrounding subshell or brace group.
            // Don't consume them — the caller does.
            TokenKind::Op(Op::Rparen) | TokenKind::Op(Op::Rbrace)
                if ctx != StatementContext::Top =>
            {
                (Terminator::Sync, list.span.end)
            }
            other => {
                return Err(KashError::Parse(format!(
                    "expected `;`, `&`, newline or end of input after statement, got {other:?}"
                )));
            }
        };
        Ok(Statement {
            list,
            terminator: term,
            span: Span::new(start, end),
        })
    }

    // ---------- and-or list ----------

    fn parse_and_or(&mut self) -> Result<AndOrList> {
        let head = self.parse_pipeline()?;
        let start = head.span.start;
        let mut end = head.span.end;
        let mut tail = Vec::new();
        loop {
            let op = match self.peek_kind()? {
                TokenKind::Op(Op::DoubleAmp) => AndOrOp::AndIf,
                TokenKind::Op(Op::DoublePipe) => AndOrOp::OrIf,
                _ => break,
            };
            self.bump()?;
            self.skip_newlines()?;
            let rhs = self.parse_pipeline()?;
            end = rhs.span.end;
            tail.push((op, rhs));
        }
        Ok(AndOrList {
            head,
            tail,
            span: Span::new(start, end),
        })
    }

    // ---------- pipeline ----------

    fn parse_pipeline(&mut self) -> Result<Pipeline> {
        let first = self.parse_command()?;
        let start = first.span().start;
        let mut end = first.span().end;
        let mut stages = vec![first];
        let mut ops = vec![PipeOp::Pipe]; // placeholder for stage 0
        loop {
            let op = match self.peek_kind()? {
                TokenKind::Op(Op::Pipe) => PipeOp::Pipe,
                TokenKind::Op(Op::PipeAmp) => PipeOp::PipeAmp,
                _ => break,
            };
            self.bump()?;
            self.skip_newlines()?;
            let next = self.parse_command()?;
            end = next.span().end;
            stages.push(next);
            ops.push(op);
        }
        Ok(Pipeline {
            stages,
            ops,
            span: Span::new(start, end),
        })
    }

    // ---------- command (simple | compound) ----------

    fn parse_command(&mut self) -> Result<Command> {
        // `[[ ... ]]` extended test — recognised before function-def
        // and reserved-word dispatch because `[[` is its own keyword
        // even though it lexes as a plain Word.
        if self.peek_bare_word()? == Some("[[") {
            return self.parse_double_bracket().map(Command::Compound);
        }
        // Function-definition dispatch (must come before reserved-word
        // dispatch so e.g. `function for` falls through to a proper
        // error rather than tripping the `for` arm).
        if self.is_function_definition_here()? {
            return self.parse_function_def().map(Command::Compound);
        }
        // Compound-command dispatch.
        if let Some(reserved) = self.peek_reserved()? {
            match reserved {
                Reserved::If => return self.parse_if().map(Command::Compound),
                Reserved::While => return self.parse_while(false).map(Command::Compound),
                Reserved::Until => return self.parse_while(true).map(Command::Compound),
                Reserved::For => return self.parse_for().map(Command::Compound),
                Reserved::Case => return self.parse_case().map(Command::Compound),
                // `then`/`else`/`elif`/`fi`/`do`/`done`/`esac`/`in` in
                // command position is always an error — they only make
                // sense as compound-body terminators handled by their
                // openers above.
                other => {
                    return Err(KashError::Parse(format!(
                        "unexpected reserved word `{}` in command position",
                        other.name()
                    )));
                }
            }
        }
        if matches!(self.peek_kind()?, TokenKind::Op(Op::Lbrace)) {
            return self.parse_brace_group().map(Command::Compound);
        }
        if matches!(self.peek_kind()?, TokenKind::Op(Op::Lparen)) {
            return self.parse_subshell().map(Command::Compound);
        }
        self.parse_simple_command().map(Command::Simple)
    }

    /// True iff the next 1-3 tokens look like the head of a function
    /// definition — either `function …` (ksh / kash form) or
    /// `NAME ( )` (POSIX form). Pure lookahead, no consumption.
    fn is_function_definition_here(&mut self) -> Result<bool> {
        // ksh / kash form.
        if self.peek_bare_word()? == Some("function") {
            return Ok(true);
        }
        // POSIX form: `NAME ( )` where NAME is a non-reserved identifier.
        let Some(name) = self.peek_bare_word()? else {
            return Ok(false);
        };
        if !is_valid_function_name(name) {
            return Ok(false);
        }
        if !matches!(self.peek_at(1)?.kind, TokenKind::Op(Op::Lparen)) {
            return Ok(false);
        }
        if !matches!(self.peek_at(2)?.kind, TokenKind::Op(Op::Rparen)) {
            return Ok(false);
        }
        Ok(true)
    }

    fn parse_simple_command(&mut self) -> Result<SimpleCommand> {
        let start = self.peek()?.span.start;
        let mut end = start;
        let mut assignments = Vec::new();
        let mut words = Vec::new();
        let mut redirects = Vec::new();

        // Phase 1: leading `NAME=VALUE` assignment prefix. Stops as
        // soon as we see anything that isn't an assignment-shaped word
        // (POSIX 2.9.1: assignments must precede every command word).
        loop {
            if !self.next_is_assignment_word()? {
                break;
            }
            let w = self.parse_word()?;
            let a = split_assignment(w)
                .map_err(|_| KashError::Parse("internal: bad assignment word".into()))?;
            end = a.span.end;
            assignments.push(a);
        }

        // Phase 2: command name + arguments + redirects.
        loop {
            // FD-prefix redirect: `2>file`, `1>&2`, etc. A bare-Word
            // token consisting entirely of decimal digits, followed
            // *adjacently* (no whitespace) by a redirect operator,
            // becomes a redirect whose `fd` field is the digits.
            if let Some(fd) = self.peek_fd_prefix()? {
                let fd_tok = self.bump()?;
                let mut r = self.parse_redirect()?;
                r.fd = Some(fd);
                r.span = Span::new(fd_tok.span.start, r.span.end);
                end = r.span.end;
                redirects.push(r);
                continue;
            }
            match self.peek_kind()? {
                TokenKind::Word(_)
                | TokenKind::SingleQuoted(_)
                | TokenKind::DoubleQuoted(_)
                | TokenKind::AnsiCString(_) => {
                    // Past the first word, reserved words and
                    // assignment-shaped words are just arguments.
                    let w = self.parse_word()?;
                    end = w.span.end;
                    words.push(w);
                }
                TokenKind::Op(op) if is_redirect_op(*op) => {
                    let r = self.parse_redirect()?;
                    end = r.span.end;
                    redirects.push(r);
                }
                _ => break,
            }
        }
        if assignments.is_empty() && words.is_empty() && redirects.is_empty() {
            let got = self.peek_kind()?.clone();
            return Err(KashError::Parse(format!("expected a command, got {got:?}")));
        }
        Ok(SimpleCommand {
            assignments,
            words,
            redirects,
            span: Span::new(start, end),
        })
    }

    /// True if the next token is a bare `NAME=...` word that should be
    /// consumed as an assignment prefix.
    fn next_is_assignment_word(&mut self) -> Result<bool> {
        let TokenKind::Word(s) = self.peek_kind()? else {
            return Ok(false);
        };
        Ok(looks_like_assignment(s))
    }

    /// If the next token is a bare all-digits Word and is immediately
    /// followed (no whitespace) by a redirect operator, return the
    /// digit value to attach as the redirect's `fd`. Otherwise
    /// returns `Ok(None)`.
    fn peek_fd_prefix(&mut self) -> Result<Option<i32>> {
        // Materialise the first two tokens up front so the rest of
        // this function can inspect them without nested mutable
        // borrows of `self`.
        self.peek_at(0)?;
        self.peek_at(1)?;
        let t0 = &self.peeked[0];
        let TokenKind::Word(s) = &t0.kind else {
            return Ok(None);
        };
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(None);
        }
        let w_end = t0.span.end;
        let t1 = &self.peeked[1];
        if t1.span.start != w_end {
            return Ok(None);
        }
        let TokenKind::Op(op) = t1.kind else {
            return Ok(None);
        };
        if !is_redirect_op(op) {
            return Ok(None);
        }
        let fd: i32 = s
            .parse()
            .map_err(|_| KashError::Parse(format!("invalid fd prefix `{s}`")))?;
        Ok(Some(fd))
    }

    /// Parse a single word, absorbing any adjacent word-like tokens
    /// into the same [`Word`]. Tokens count as adjacent when there is
    /// no whitespace, comment, or other trivia between them — i.e.
    /// `prev.span.end == next.span.start`.
    fn parse_word(&mut self) -> Result<Word> {
        let first = self.bump()?;
        let start = first.span.start;
        let mut end = first.span.end;
        let mut segments = vec![token_to_segment(first.kind)?];
        loop {
            // Stop if the next token is not adjacent.
            let next = self.peek()?;
            if next.span.start != end {
                break;
            }
            if !is_word_token(&next.kind) {
                break;
            }
            let tok = self.bump()?;
            end = tok.span.end;
            segments.push(token_to_segment(tok.kind)?);
        }
        Ok(Word {
            segments,
            span: Span::new(start, end),
        })
    }

    fn parse_redirect(&mut self) -> Result<Redirect> {
        let tok = self.bump()?;
        let TokenKind::Op(op) = tok.kind else {
            return Err(KashError::Parse(format!(
                "expected redirect operator, got {:?}",
                tok.kind
            )));
        };
        // Here-document forms need access to the lexer's raw source
        // buffer, so they branch out before the regular target-word
        // step.
        if matches!(op, Op::DoubleLt | Op::DoubleLtDash) {
            return self.parse_heredoc(tok.span.start, op);
        }
        let kind = match op {
            Op::Gt => RedirectKind::Output,
            Op::DoubleGt => RedirectKind::Append,
            Op::Lt => RedirectKind::Input,
            Op::AmpGt => RedirectKind::OutputBoth,
            Op::AmpGtGt => RedirectKind::AppendBoth,
            Op::TripleLt => RedirectKind::HereString,
            Op::GtAmp => RedirectKind::DupOutput,
            Op::LtAmp => RedirectKind::DupInput,
            other => {
                return Err(KashError::Parse(format!(
                    "unsupported redirect operator {other:?}"
                )));
            }
        };
        if !matches!(
            self.peek_kind()?,
            TokenKind::Word(_)
                | TokenKind::SingleQuoted(_)
                | TokenKind::DoubleQuoted(_)
                | TokenKind::AnsiCString(_)
        ) {
            return Err(KashError::Parse(format!(
                "expected a {} after `{}`",
                if matches!(kind, RedirectKind::HereString) {
                    "word"
                } else {
                    "filename"
                },
                op_display(op)
            )));
        }
        let target = self.parse_word()?;
        let span = Span::new(tok.span.start, target.span.end);
        Ok(Redirect {
            kind,
            fd: None,
            target,
            span,
        })
    }

    /// Handle `<<DELIM` / `<<-DELIM`. Reads the delimiter word, then
    /// (after the line's terminating newline) pulls the body text
    /// from the lexer's raw buffer up to the closing delimiter line.
    fn parse_heredoc(&mut self, op_start: usize, op: Op) -> Result<Redirect> {
        let strip_tabs = matches!(op, Op::DoubleLtDash);
        // Delimiter word.
        let delim_word = self.parse_word()?;
        let (delim_text, quoted) = extract_heredoc_delim(&delim_word);
        // POSIX permits the rest of the introducer line to carry
        // additional tokens, but the minimal here-doc path supports
        // exactly `<<DELIM<newline>` — bail out cleanly otherwise.
        match self.peek_kind()? {
            TokenKind::Newline => {
                self.bump()?;
            }
            TokenKind::Eof => {
                return Err(KashError::Parse(
                    "here-doc body missing — input ended right after delimiter".into(),
                ));
            }
            other => {
                return Err(KashError::Parse(format!(
                    "here-doc minimal form requires a newline immediately after `<<{delim_text}`, got {other:?}"
                )));
            }
        }
        // Drain any tokens the lexer pre-buffered before the newline.
        self.peeked.clear();
        let body_start = self.lexer_pos();
        let body = self
            .lexer_mut()
            .read_heredoc_body(&delim_text, strip_tabs)?;
        let body_end = self.lexer_pos();
        // The captured body becomes the redirect's `target` Word.
        // `Bare` payload → expansion happens at eval time; quoted →
        // pass through verbatim.
        let segment = if quoted {
            WordSegment::SingleQuoted(body)
        } else {
            WordSegment::Bare(body)
        };
        let target = Word {
            segments: alloc::vec![segment],
            span: Span::new(body_start, body_end),
        };
        Ok(Redirect {
            kind: RedirectKind::HereDoc { strip_tabs },
            fd: None,
            target,
            span: Span::new(op_start, body_end),
        })
    }

    fn lexer_pos(&self) -> usize {
        self.lexer.byte_pos()
    }

    fn lexer_mut(&mut self) -> &mut Lexer<'src> {
        &mut self.lexer
    }

    // ---------- `[[ ... ]]` extended test ----------

    /// Parse a `[[ … ]]` block. Words inside are collected verbatim
    /// (their later expansion happens at evaluator time). Newlines
    /// are allowed and skipped; the closing `]]` may carry trailing
    /// redirects like any other compound.
    fn parse_double_bracket(&mut self) -> Result<CompoundCommand> {
        let open = self.bump()?; // `[[`
        let mut tokens = Vec::new();
        loop {
            match self.peek_kind()? {
                TokenKind::Eof => {
                    return Err(KashError::Parse(
                        "unterminated `[[ ... ]]` extended test".into(),
                    ));
                }
                TokenKind::Newline => {
                    self.bump()?;
                }
                TokenKind::Word(s) if s == "]]" => {
                    let close = self.bump()?;
                    let redirects = self.parse_trailing_redirects()?;
                    let end = redirects
                        .last()
                        .map_or(close.span.end, |r| r.span.end);
                    return Ok(CompoundCommand {
                        kind: CompoundKind::DoubleBracket { tokens },
                        redirects,
                        span: Span::new(open.span.start, end),
                    });
                }
                TokenKind::Word(_)
                | TokenKind::SingleQuoted(_)
                | TokenKind::DoubleQuoted(_)
                | TokenKind::AnsiCString(_) => {
                    let w = self.parse_word()?;
                    tokens.push(w);
                }
                TokenKind::Op(op) if let Some(s) = double_bracket_op_str(*op) => {
                    // Re-cast structural operators (`&&`, `||`, `(`,
                    // `)`, `<`, `>`) as plain-text words so the
                    // evaluator's `[[ … ]]` parser can see them in
                    // its `args: &[&str]` view.
                    let tok = self.bump()?;
                    tokens.push(Word {
                        segments: alloc::vec![WordSegment::Bare(s.into())],
                        span: tok.span,
                    });
                }
                other => {
                    return Err(KashError::Parse(format!(
                        "unexpected {other:?} inside `[[ ... ]]`"
                    )));
                }
            }
        }
    }

    // ---------- function definitions ----------

    fn parse_function_def(&mut self) -> Result<CompoundCommand> {
        let (start, scope, name, captures) = if self.peek_bare_word()? == Some("function") {
            self.parse_function_head_ksh()?
        } else {
            self.parse_function_head_posix()?
        };
        self.skip_newlines()?;
        let mut body = self.parse_function_body()?;
        let body_end = body.span.end;
        // Redirects on the body apply to the function as a whole — the
        // inner compound has already swallowed them in its own
        // `parse_trailing_redirects`, so reparent them here.
        let mut redirects = core::mem::take(&mut body.redirects);
        redirects.extend(self.parse_trailing_redirects()?);
        let end = redirects.last().map_or(body_end, |r| r.span.end);
        Ok(CompoundCommand {
            kind: CompoundKind::FunctionDef {
                name,
                scope,
                captures,
                body: Box::new(body),
            },
            redirects,
            span: Span::new(start, end),
        })
    }

    /// `name ( )` — POSIX form. `(` `)` already verified by
    /// `is_function_definition_here`.
    fn parse_function_head_posix(
        &mut self,
    ) -> Result<(usize, FunctionScope, String, Option<Vec<String>>)> {
        let name_tok = self.bump()?;
        let TokenKind::Word(name) = name_tok.kind else {
            unreachable!("checked in is_function_definition_here");
        };
        let _lp = self.bump()?; // `(`
        let _rp = self.bump()?; // `)`
        Ok((name_tok.span.start, FunctionScope::Dynamic, name, None))
    }

    /// `function NAME [ ( CAPTURES? ) ]` — ksh93 / kash form.
    fn parse_function_head_ksh(
        &mut self,
    ) -> Result<(usize, FunctionScope, String, Option<Vec<String>>)> {
        let kw = self.bump()?; // `function`
        let start = kw.span.start;
        // Function name.
        let Some(name) = self.peek_bare_word()? else {
            return Err(KashError::Parse(
                "expected function name after `function`".into(),
            ));
        };
        if !is_valid_function_name(name) {
            return Err(KashError::Parse(format!(
                "invalid function name `{name}` after `function`"
            )));
        }
        let name_tok = self.bump()?;
        let TokenKind::Word(name) = name_tok.kind else { unreachable!() };
        // Optional capture list.
        let captures = if matches!(self.peek_kind()?, TokenKind::Op(Op::Lparen)) {
            self.bump()?; // `(`
            let caps = self.parse_capture_list()?;
            if !matches!(self.peek_kind()?, TokenKind::Op(Op::Rparen)) {
                return Err(KashError::Parse(
                    "expected `)` after function capture list".into(),
                ));
            }
            self.bump()?; // `)`
            Some(caps)
        } else {
            None
        };
        Ok((start, FunctionScope::Static, name, captures))
    }

    /// Collect bare-word tokens up to the closing `)` and split them
    /// on commas. Comma is a normal word-byte in the lexer, so e.g.
    /// `(a, b)` lexes as `Word("a,") Word("b")` — joining the raw
    /// payloads and splitting on `,` recovers the intended list. Empty
    /// names, quoted names, and non-identifier names are rejected.
    fn parse_capture_list(&mut self) -> Result<Vec<String>> {
        let mut raw = String::new();
        loop {
            match self.peek_kind()? {
                TokenKind::Op(Op::Rparen) => break,
                TokenKind::Word(_) => {
                    let tok = self.bump()?;
                    let TokenKind::Word(s) = tok.kind else { unreachable!() };
                    raw.push_str(&s);
                    // A space-separated entry counts as a comma boundary
                    // — `(a b)` is the same as `(a, b)`.
                    raw.push(',');
                }
                TokenKind::Newline => {
                    self.bump()?;
                }
                TokenKind::SingleQuoted(_)
                | TokenKind::DoubleQuoted(_)
                | TokenKind::AnsiCString(_) => {
                    return Err(KashError::Parse(
                        "capture-list names cannot be quoted".into(),
                    ));
                }
                other => {
                    return Err(KashError::Parse(format!(
                        "unexpected {other:?} inside capture list"
                    )));
                }
            }
        }
        let mut caps = Vec::new();
        for part in raw.split(',') {
            let name = part.trim();
            if name.is_empty() {
                continue;
            }
            if !is_valid_identifier(name) {
                return Err(KashError::Parse(format!(
                    "invalid capture name `{name}`"
                )));
            }
            caps.push(name.to_string());
        }
        Ok(caps)
    }

    /// Parse a function body — any compound command (POSIX requires a
    /// brace group, but bash/ksh93 accept any compound shape and kash
    /// matches that).
    fn parse_function_body(&mut self) -> Result<CompoundCommand> {
        match self.peek_kind()? {
            TokenKind::Op(Op::Lbrace) => self.parse_brace_group(),
            TokenKind::Op(Op::Lparen) => self.parse_subshell(),
            TokenKind::Word(_) => match self.peek_reserved()? {
                Some(Reserved::If) => self.parse_if(),
                Some(Reserved::While) => self.parse_while(false),
                Some(Reserved::Until) => self.parse_while(true),
                Some(Reserved::For) => self.parse_for(),
                Some(Reserved::Case) => self.parse_case(),
                _ => Err(KashError::Parse(
                    "expected function body (a compound command)".into(),
                )),
            },
            _ => Err(KashError::Parse(
                "expected function body (a compound command)".into(),
            )),
        }
    }

    // ---------- compound commands ----------

    /// Parse one or more statements until we hit any of the terminator
    /// reserved words supplied in `enders` (e.g. `[Then]` for an `if`
    /// condition list, `[Done]` for a loop body, `[Else, Elif, Fi]`
    /// for an `if` body).
    fn parse_compound_body(&mut self, enders: &[Reserved]) -> Result<Vec<Statement>> {
        let mut out = Vec::new();
        loop {
            self.skip_newlines()?;
            if let Some(r) = self.peek_reserved()? {
                if enders.contains(&r) {
                    return Ok(out);
                }
            }
            if matches!(self.peek_kind()?, TokenKind::Eof) {
                return Err(KashError::Parse(format!(
                    "unexpected end of input; expected one of {}",
                    join_reserved(enders),
                )));
            }
            out.push(self.parse_statement(StatementContext::Compound)?);
        }
    }

    fn parse_brace_group(&mut self) -> Result<CompoundCommand> {
        let open = self.bump()?; // `{`
        debug_assert!(matches!(open.kind, TokenKind::Op(Op::Lbrace)));
        let body = self.parse_brace_body()?;
        if !matches!(self.peek_kind()?, TokenKind::Op(Op::Rbrace)) {
            return Err(KashError::Parse(
                "expected `}` to close brace group".to_string(),
            ));
        }
        let close = self.bump()?;
        let redirects = self.parse_trailing_redirects()?;
        let end = redirects
            .last()
            .map_or(close.span.end, |r| r.span.end);
        Ok(CompoundCommand {
            kind: CompoundKind::BraceGroup { body },
            redirects,
            span: Span::new(open.span.start, end),
        })
    }

    /// Parse the statements inside `{ ... }` up to (but not including)
    /// the closing `}`. The closing brace is only recognised when
    /// it appears at statement-start position, matching POSIX.
    fn parse_brace_body(&mut self) -> Result<Vec<Statement>> {
        let mut out = Vec::new();
        loop {
            self.skip_newlines()?;
            if matches!(self.peek_kind()?, TokenKind::Op(Op::Rbrace) | TokenKind::Eof) {
                return Ok(out);
            }
            out.push(self.parse_statement(StatementContext::Compound)?);
        }
    }

    fn parse_subshell(&mut self) -> Result<CompoundCommand> {
        let open = self.bump()?; // `(`
        debug_assert!(matches!(open.kind, TokenKind::Op(Op::Lparen)));
        let body = self.parse_subshell_body()?;
        if !matches!(self.peek_kind()?, TokenKind::Op(Op::Rparen)) {
            return Err(KashError::Parse(
                "expected `)` to close subshell".to_string(),
            ));
        }
        let close = self.bump()?;
        let redirects = self.parse_trailing_redirects()?;
        let end = redirects
            .last()
            .map_or(close.span.end, |r| r.span.end);
        Ok(CompoundCommand {
            kind: CompoundKind::Subshell { body },
            redirects,
            span: Span::new(open.span.start, end),
        })
    }

    fn parse_subshell_body(&mut self) -> Result<Vec<Statement>> {
        let mut out = Vec::new();
        loop {
            self.skip_newlines()?;
            if matches!(self.peek_kind()?, TokenKind::Op(Op::Rparen) | TokenKind::Eof) {
                return Ok(out);
            }
            out.push(self.parse_statement(StatementContext::Compound)?);
        }
    }

    fn parse_if(&mut self) -> Result<CompoundCommand> {
        let if_tok = self.expect_reserved(Reserved::If)?;
        let mut branches = Vec::new();
        // First arm.
        let first_cond = self.parse_compound_body(&[Reserved::Then])?;
        self.expect_reserved(Reserved::Then)?;
        let first_body = self.parse_compound_body(&[Reserved::Elif, Reserved::Else, Reserved::Fi])?;
        let first_span_end = first_body.last().map_or(if_tok.span.end, |s| s.span.end);
        branches.push(IfBranch {
            cond: first_cond,
            body: first_body,
            span: Span::new(if_tok.span.start, first_span_end),
        });
        // elif arms.
        while self.peek_reserved()? == Some(Reserved::Elif) {
            let elif_tok = self.expect_reserved(Reserved::Elif)?;
            let cond = self.parse_compound_body(&[Reserved::Then])?;
            self.expect_reserved(Reserved::Then)?;
            let body = self.parse_compound_body(&[Reserved::Elif, Reserved::Else, Reserved::Fi])?;
            let end = body.last().map_or(elif_tok.span.end, |s| s.span.end);
            branches.push(IfBranch {
                cond,
                body,
                span: Span::new(elif_tok.span.start, end),
            });
        }
        // Optional else.
        let else_body = if self.peek_reserved()? == Some(Reserved::Else) {
            self.expect_reserved(Reserved::Else)?;
            Some(self.parse_compound_body(&[Reserved::Fi])?)
        } else {
            None
        };
        let fi_tok = self.expect_reserved(Reserved::Fi)?;
        let redirects = self.parse_trailing_redirects()?;
        let end = redirects.last().map_or(fi_tok.span.end, |r| r.span.end);
        Ok(CompoundCommand {
            kind: CompoundKind::If {
                branches,
                else_body,
            },
            redirects,
            span: Span::new(if_tok.span.start, end),
        })
    }

    fn parse_while(&mut self, is_until: bool) -> Result<CompoundCommand> {
        let opener = if is_until { Reserved::Until } else { Reserved::While };
        let head = self.expect_reserved(opener)?;
        let cond = self.parse_compound_body(&[Reserved::Do])?;
        self.expect_reserved(Reserved::Do)?;
        let body = self.parse_compound_body(&[Reserved::Done])?;
        let done = self.expect_reserved(Reserved::Done)?;
        let redirects = self.parse_trailing_redirects()?;
        let end = redirects.last().map_or(done.span.end, |r| r.span.end);
        let kind = if is_until {
            CompoundKind::Until { cond, body }
        } else {
            CompoundKind::While { cond, body }
        };
        Ok(CompoundCommand {
            kind,
            redirects,
            span: Span::new(head.span.start, end),
        })
    }

    fn parse_for(&mut self) -> Result<CompoundCommand> {
        let for_tok = self.expect_reserved(Reserved::For)?;
        // Variable name — a bare word (POSIX disallows quoted names).
        let name = match self.peek_kind()? {
            TokenKind::Word(s) if reserved_word(s).is_none() => {
                let tok = self.bump()?;
                match tok.kind {
                    TokenKind::Word(s) => s,
                    _ => unreachable!(),
                }
            }
            other => {
                return Err(KashError::Parse(format!(
                    "expected loop variable name after `for`, got {other:?}"
                )));
            }
        };
        // Optional `; ` / newlines before `in` or `do`.
        self.skip_statement_separators()?;
        self.skip_newlines()?;
        let words = match self.peek_reserved()? {
            Some(Reserved::In) => {
                self.expect_reserved(Reserved::In)?;
                let mut ws = Vec::new();
                while is_word_token(self.peek_kind()?) {
                    ws.push(self.parse_word()?);
                }
                // The `in` clause ends at the first `;` / newline.
                match self.peek_kind()? {
                    TokenKind::Op(Op::Semi) | TokenKind::Newline => {
                        self.bump()?;
                    }
                    _ => {}
                }
                Some(ws)
            }
            // No `in` — iterate `"$@"`. Still allow a separator before `do`.
            _ => None,
        };
        self.skip_newlines()?;
        self.expect_reserved(Reserved::Do)?;
        let body = self.parse_compound_body(&[Reserved::Done])?;
        let done = self.expect_reserved(Reserved::Done)?;
        let redirects = self.parse_trailing_redirects()?;
        let end = redirects.last().map_or(done.span.end, |r| r.span.end);
        Ok(CompoundCommand {
            kind: CompoundKind::For { name, words, body },
            redirects,
            span: Span::new(for_tok.span.start, end),
        })
    }

    fn parse_case(&mut self) -> Result<CompoundCommand> {
        let case_tok = self.expect_reserved(Reserved::Case)?;
        // Subject word.
        if !is_word_token(self.peek_kind()?) {
            return Err(KashError::Parse(
                "expected subject word after `case`".to_string(),
            ));
        }
        let subject = self.parse_word()?;
        self.skip_newlines()?;
        self.expect_reserved(Reserved::In)?;
        self.skip_newlines()?;
        let mut items = Vec::new();
        while self.peek_reserved()? != Some(Reserved::Esac) {
            if matches!(self.peek_kind()?, TokenKind::Eof) {
                return Err(KashError::Parse(
                    "unexpected end of input inside `case`".to_string(),
                ));
            }
            items.push(self.parse_case_item()?);
            self.skip_newlines()?;
        }
        let esac_tok = self.expect_reserved(Reserved::Esac)?;
        let redirects = self.parse_trailing_redirects()?;
        let end = redirects.last().map_or(esac_tok.span.end, |r| r.span.end);
        Ok(CompoundCommand {
            kind: CompoundKind::Case { subject, items },
            redirects,
            span: Span::new(case_tok.span.start, end),
        })
    }

    fn parse_case_item(&mut self) -> Result<CaseItem> {
        // Optional leading `(`.
        let item_start = self.peek()?.span.start;
        if matches!(self.peek_kind()?, TokenKind::Op(Op::Lparen)) {
            self.bump()?;
        }
        let mut patterns = Vec::new();
        loop {
            if !is_word_token(self.peek_kind()?) {
                return Err(KashError::Parse(
                    "expected a pattern in `case` arm".to_string(),
                ));
            }
            patterns.push(self.parse_word()?);
            if matches!(self.peek_kind()?, TokenKind::Op(Op::Pipe)) {
                self.bump()?;
                continue;
            }
            break;
        }
        if !matches!(self.peek_kind()?, TokenKind::Op(Op::Rparen)) {
            return Err(KashError::Parse(
                "expected `)` after case patterns".to_string(),
            ));
        }
        self.bump()?;
        // Body until a fall-through marker or `esac`.
        let mut body = Vec::new();
        let fall = loop {
            self.skip_newlines()?;
            match self.peek_kind()? {
                TokenKind::Op(Op::DoubleSemi) => {
                    self.bump()?;
                    break CaseFallthrough::Stop;
                }
                TokenKind::Op(Op::SemiAmp) => {
                    self.bump()?;
                    break CaseFallthrough::Continue;
                }
                TokenKind::Op(Op::DoubleSemiAmp) => {
                    self.bump()?;
                    break CaseFallthrough::MatchNext;
                }
                _ => {}
            }
            if self.peek_reserved()? == Some(Reserved::Esac) {
                // POSIX allows the final arm to omit `;;`.
                break CaseFallthrough::Stop;
            }
            if matches!(self.peek_kind()?, TokenKind::Eof) {
                return Err(KashError::Parse(
                    "unexpected end of input inside case arm".to_string(),
                ));
            }
            body.push(self.parse_statement(StatementContext::CaseArm)?);
        };
        let end = body.last().map_or(item_start, |s| s.span.end);
        Ok(CaseItem {
            patterns,
            body,
            fallthrough: fall,
            span: Span::new(item_start, end),
        })
    }

    fn parse_trailing_redirects(&mut self) -> Result<Vec<Redirect>> {
        let mut out = Vec::new();
        while let TokenKind::Op(op) = self.peek_kind()? {
            if !is_redirect_op(*op) {
                break;
            }
            out.push(self.parse_redirect()?);
        }
        Ok(out)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatementContext {
    Top,
    Compound,
    CaseArm,
}

/// Extract the here-doc delimiter text and a "was it quoted" flag
/// from a parsed [`Word`]. POSIX says that *any* quoting in the
/// delimiter — even partial — disables expansion of the body, so we
/// take the union of: any quoted segment is present, OR any
/// backslash escapes appear in the bare text. The minimal cut here
/// only checks for quoted segments; backslash detection lands when
/// the parser starts tracking backslash provenance.
fn extract_heredoc_delim(w: &Word) -> (String, bool) {
    let mut text = String::new();
    let mut quoted = false;
    for seg in &w.segments {
        match seg {
            WordSegment::Bare(s) => text.push_str(s),
            WordSegment::SingleQuoted(s) | WordSegment::DoubleQuoted(s) | WordSegment::AnsiC(s) => {
                quoted = true;
                text.push_str(s);
            }
        }
    }
    (text, quoted)
}

/// Op-to-string mapping used by `parse_double_bracket` to keep
/// structural operators inside `[[ … ]]` accessible as words.
const fn double_bracket_op_str(op: Op) -> Option<&'static str> {
    match op {
        Op::DoubleAmp => Some("&&"),
        Op::DoublePipe => Some("||"),
        Op::Lparen => Some("("),
        Op::Rparen => Some(")"),
        Op::Lt => Some("<"),
        Op::Gt => Some(">"),
        _ => None,
    }
}

fn is_word_token(t: &TokenKind) -> bool {
    matches!(
        t,
        TokenKind::Word(_)
            | TokenKind::SingleQuoted(_)
            | TokenKind::DoubleQuoted(_)
            | TokenKind::AnsiCString(_)
    )
}

/// Convert a word-like [`TokenKind`] into the matching [`WordSegment`].
fn token_to_segment(kind: TokenKind) -> Result<WordSegment> {
    Ok(match kind {
        TokenKind::Word(s) => WordSegment::Bare(s),
        TokenKind::SingleQuoted(s) => WordSegment::SingleQuoted(s),
        TokenKind::DoubleQuoted(s) => WordSegment::DoubleQuoted(s),
        TokenKind::AnsiCString(s) => WordSegment::AnsiC(s),
        other => {
            return Err(KashError::Parse(format!(
                "expected a word-like token, got {other:?}"
            )));
        }
    })
}

/// True iff `s` is a valid shell identifier (POSIX
/// `name`/`identifier`: leading `_` or letter, then letters/digits/
/// underscores). ASCII-only — POSIX names are ASCII.
fn is_valid_identifier(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let first = bytes[0];
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| *b == b'_' || b.is_ascii_alphanumeric())
}

/// A valid function name is a valid identifier that isn't a reserved
/// keyword. Reserved words used as function names lead to confusing
/// parses; reject them up front.
fn is_valid_function_name(s: &str) -> bool {
    is_valid_identifier(s) && reserved_word(s).is_none()
}

/// True iff `s` starts with `NAME=` where `NAME` is a valid
/// identifier. This is the cheap pre-check used to decide whether to
/// consume a token as an assignment prefix; the full split happens in
/// [`split_assignment`].
fn looks_like_assignment(s: &str) -> bool {
    let Some(eq) = s.find('=') else { return false };
    is_valid_identifier(&s[..eq])
}

/// Split a parsed [`Word`] (which is known to begin with `NAME=…`)
/// into a name and a value [`Word`]. Returns `Err(word)` if the word's
/// first segment doesn't have the assignment shape.
fn split_assignment(word: Word) -> core::result::Result<Assignment, Word> {
    let first = match &word.segments[0] {
        WordSegment::Bare(s) => s.clone(),
        _ => return Err(word),
    };
    let Some(eq) = first.find('=') else {
        return Err(word);
    };
    let name = first[..eq].to_string();
    if !is_valid_identifier(&name) {
        return Err(word);
    }
    let value_first_text = &first[eq + 1..];
    let mut value_segments = Vec::new();
    if !value_first_text.is_empty() {
        value_segments.push(WordSegment::Bare(value_first_text.to_string()));
    }
    for s in word.segments.into_iter().skip(1) {
        value_segments.push(s);
    }
    if value_segments.is_empty() {
        // `NAME=` form — explicit empty value.
        value_segments.push(WordSegment::Bare(String::new()));
    }
    let value_start = word.span.start + name.len() + 1; // `=` is one byte
    let value = Word {
        segments: value_segments,
        span: Span::new(value_start, word.span.end),
    };
    Ok(Assignment {
        name,
        value,
        span: word.span,
    })
}

const fn is_redirect_op(op: Op) -> bool {
    matches!(
        op,
        Op::Gt
            | Op::DoubleGt
            | Op::Lt
            | Op::AmpGt
            | Op::AmpGtGt
            | Op::TripleLt
            | Op::DoubleLt
            | Op::DoubleLtDash
            | Op::GtAmp
            | Op::LtAmp
    )
}

const fn op_display(op: Op) -> &'static str {
    match op {
        Op::Gt => ">",
        Op::DoubleGt => ">>",
        Op::Lt => "<",
        Op::AmpGt => "&>",
        Op::AmpGtGt => "&>>",
        _ => "?",
    }
}

fn join_reserved(rs: &[Reserved]) -> String {
    let mut s = String::new();
    for (i, r) in rs.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push('`');
        s.push_str(r.name());
        s.push('`');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(src: &str) -> Program {
        parse(src).unwrap_or_else(|e| panic!("parse({src:?}) failed: {e}"))
    }

    fn simple_only(cmd: &Command) -> &SimpleCommand {
        match cmd {
            Command::Simple(s) => s,
            Command::Compound(_) => panic!("expected Simple, got Compound"),
        }
    }

    fn compound_only(cmd: &Command) -> &CompoundCommand {
        match cmd {
            Command::Compound(c) => c,
            Command::Simple(_) => panic!("expected Compound, got Simple"),
        }
    }

    fn first_simple(prog: &Program) -> &SimpleCommand {
        simple_only(&prog.statements[0].list.head.stages[0])
    }

    fn first_compound(prog: &Program) -> &CompoundCommand {
        compound_only(&prog.statements[0].list.head.stages[0])
    }

    fn bare(w: &Word) -> &str {
        match &w.segments[0] {
            WordSegment::Bare(s) => s,
            other => panic!("expected Bare segment, got {other:?}"),
        }
    }

    // ===== regression tests covering the (5) baseline =====

    #[test]
    fn empty_input_yields_empty_program() {
        let prog = p("");
        assert!(prog.statements.is_empty());
    }

    #[test]
    fn whitespace_and_newlines_only() {
        let prog = p("   \n\n  \n");
        assert!(prog.statements.is_empty());
    }

    #[test]
    fn simple_two_word_command() {
        let prog = p("echo hello");
        assert_eq!(prog.statements.len(), 1);
        let stage = first_simple(&prog);
        assert_eq!(stage.words.len(), 2);
        assert_eq!(bare(&stage.words[0]), "echo");
        assert_eq!(bare(&stage.words[1]), "hello");
    }

    #[test]
    fn three_commands_separated_by_semicolons() {
        let prog = p("a; b; c");
        assert_eq!(prog.statements.len(), 3);
        for (st, name) in prog.statements.iter().zip(["a", "b", "c"]) {
            assert_eq!(st.terminator, Terminator::Sync);
            assert_eq!(bare(&simple_only(&st.list.head.stages[0]).words[0]), name);
        }
    }

    #[test]
    fn background_terminator() {
        let prog = p("sleep 1 &");
        assert_eq!(prog.statements[0].terminator, Terminator::Background);
    }

    #[test]
    fn and_or_list_left_associative() {
        let prog = p("a && b || c");
        let list = &prog.statements[0].list;
        assert_eq!(list.tail.len(), 2);
        assert_eq!(list.tail[0].0, AndOrOp::AndIf);
        assert_eq!(list.tail[1].0, AndOrOp::OrIf);
    }

    #[test]
    fn pipeline_with_three_stages() {
        let prog = p("a | b | c");
        let pipe = &prog.statements[0].list.head;
        assert_eq!(pipe.stages.len(), 3);
    }

    #[test]
    fn coprocess_pipe_amp_recognised() {
        let prog = p("a |& b");
        let pipe = &prog.statements[0].list.head;
        assert_eq!(pipe.ops[1], PipeOp::PipeAmp);
    }

    #[test]
    fn output_redirect_attached_to_command() {
        let prog = p("echo hi > out.txt");
        let cmd = first_simple(&prog);
        assert_eq!(cmd.redirects[0].kind, RedirectKind::Output);
        assert_eq!(bare(&cmd.redirects[0].target), "out.txt");
    }

    #[test]
    fn quoted_segments_preserved() {
        let prog = p("echo 'lit' \"dq\" $'an'");
        let words = &first_simple(&prog).words;
        assert!(matches!(words[1].segments[0], WordSegment::SingleQuoted(ref s) if s == "lit"));
        assert!(matches!(words[2].segments[0], WordSegment::DoubleQuoted(ref s) if s == "dq"));
        assert!(matches!(words[3].segments[0], WordSegment::AnsiC(ref s) if s == "an"));
    }

    // ===== brace group / subshell =====

    #[test]
    fn brace_group_basic() {
        let prog = p("{ echo a; echo b; }");
        let c = first_compound(&prog);
        let body = match &c.kind {
            CompoundKind::BraceGroup { body } => body,
            other => panic!("expected BraceGroup, got {other:?}"),
        };
        assert_eq!(body.len(), 2);
        assert_eq!(bare(&simple_only(&body[0].list.head.stages[0]).words[0]), "echo");
    }

    #[test]
    fn brace_group_with_trailing_redirect() {
        let prog = p("{ echo a; } > /tmp/out");
        let c = first_compound(&prog);
        assert_eq!(c.redirects.len(), 1);
        assert_eq!(c.redirects[0].kind, RedirectKind::Output);
    }

    #[test]
    fn subshell_basic() {
        let prog = p("(echo a; echo b)");
        let c = first_compound(&prog);
        let body = match &c.kind {
            CompoundKind::Subshell { body } => body,
            other => panic!("expected Subshell, got {other:?}"),
        };
        assert_eq!(body.len(), 2);
    }

    #[test]
    fn nested_brace_inside_subshell() {
        let prog = p("( { echo a; }; echo b )");
        let outer = first_compound(&prog);
        match &outer.kind {
            CompoundKind::Subshell { body } => assert_eq!(body.len(), 2),
            other => panic!("got {other:?}"),
        }
    }

    // ===== if / elif / else =====

    #[test]
    fn if_then_fi() {
        let prog = p("if true; then echo yes; fi");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::If { branches, else_body } => {
                assert_eq!(branches.len(), 1);
                assert!(else_body.is_none());
                assert_eq!(branches[0].cond.len(), 1);
                assert_eq!(branches[0].body.len(), 1);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn if_then_else_fi() {
        let prog = p("if false; then echo yes; else echo no; fi");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::If { branches, else_body } => {
                assert_eq!(branches.len(), 1);
                assert!(else_body.is_some());
                assert_eq!(else_body.as_ref().unwrap().len(), 1);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn if_elif_else_fi() {
        let prog = p("if a; then x; elif b; then y; elif c; then z; else w; fi");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::If { branches, else_body } => {
                assert_eq!(branches.len(), 3);
                assert!(else_body.is_some());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn if_unterminated_errors() {
        assert!(parse("if true; then echo yes").is_err());
    }

    // ===== while / until =====

    #[test]
    fn while_loop_basic() {
        let prog = p("while read line; do echo $line; done");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::While { cond, body } => {
                assert_eq!(cond.len(), 1);
                assert_eq!(body.len(), 1);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn until_loop_basic() {
        let prog = p("until false; do echo loop; done");
        let c = first_compound(&prog);
        assert!(matches!(c.kind, CompoundKind::Until { .. }));
    }

    // ===== for =====

    #[test]
    fn for_with_in_clause() {
        let prog = p("for x in a b c; do echo $x; done");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::For { name, words, body } => {
                assert_eq!(name, "x");
                let ws = words.as_ref().unwrap();
                assert_eq!(ws.len(), 3);
                assert_eq!(bare(&ws[0]), "a");
                assert_eq!(bare(&ws[2]), "c");
                assert_eq!(body.len(), 1);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn for_without_in_clause_iterates_positionals() {
        let prog = p("for x; do echo $x; done");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::For { name, words, body } => {
                assert_eq!(name, "x");
                assert!(words.is_none());
                assert_eq!(body.len(), 1);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn for_with_newline_separated_in_clause() {
        let prog = p("for x in a b c\ndo echo $x; done");
        let c = first_compound(&prog);
        assert!(matches!(c.kind, CompoundKind::For { .. }));
    }

    // ===== case =====

    #[test]
    fn case_basic_two_arms() {
        let prog = p("case $x in a) echo aa;; b) echo bb;; esac");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::Case { items, .. } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].fallthrough, CaseFallthrough::Stop);
                assert_eq!(items[1].fallthrough, CaseFallthrough::Stop);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn case_with_pipe_alternatives() {
        let prog = p("case $x in a|b|c) echo abc;; esac");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::Case { items, .. } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].patterns.len(), 3);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn case_fallthrough_markers() {
        let prog = p("case $x in a) echo a;& b) echo b;;& c) echo c;; esac");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::Case { items, .. } => {
                assert_eq!(items[0].fallthrough, CaseFallthrough::Continue);
                assert_eq!(items[1].fallthrough, CaseFallthrough::MatchNext);
                assert_eq!(items[2].fallthrough, CaseFallthrough::Stop);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn case_optional_leading_paren() {
        let prog = p("case $x in (a) echo aa;; (b) echo bb;; esac");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::Case { items, .. } => assert_eq!(items.len(), 2),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn case_final_arm_may_omit_double_semi() {
        // POSIX permits the final arm to drop `;;`, but it still needs
        // a `;` or newline before `esac` (otherwise `esac` would just
        // be another argument of the simple command).
        let prog = p("case $x in a) echo aa\nesac");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::Case { items, .. } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].fallthrough, CaseFallthrough::Stop);
            }
            other => panic!("got {other:?}"),
        }
    }

    // ===== reserved-word context sensitivity =====

    #[test]
    fn reserved_word_as_argument_is_just_a_word() {
        // `done` here is the second word of the simple command, not a
        // compound terminator.
        let prog = p("echo done");
        let cmd = first_simple(&prog);
        assert_eq!(cmd.words.len(), 2);
        assert_eq!(bare(&cmd.words[1]), "done");
    }

    #[test]
    fn reserved_word_in_command_position_outside_compound_errors() {
        // `done` with nothing surrounding it is an error.
        assert!(parse("done").is_err());
    }

    // ===== compound inside pipeline / and-or =====

    #[test]
    fn compound_can_be_a_pipeline_stage() {
        let prog = p("{ echo a; } | wc -l");
        let pipe = &prog.statements[0].list.head;
        assert_eq!(pipe.stages.len(), 2);
        assert!(matches!(pipe.stages[0], Command::Compound(_)));
        assert!(matches!(pipe.stages[1], Command::Simple(_)));
    }

    // ===== word concatenation =====

    #[test]
    fn adjacent_tokens_concatenate_into_one_word() {
        let prog = p("echo foo\"bar\"$'baz'");
        let words = &first_simple(&prog).words;
        assert_eq!(words.len(), 2);
        let w = &words[1];
        assert_eq!(w.segments.len(), 3);
        assert!(matches!(w.segments[0], WordSegment::Bare(ref s) if s == "foo"));
        assert!(matches!(w.segments[1], WordSegment::DoubleQuoted(ref s) if s == "bar"));
        assert!(matches!(w.segments[2], WordSegment::AnsiC(ref s) if s == "baz"));
    }

    #[test]
    fn whitespace_breaks_concatenation() {
        let prog = p("echo foo \"bar\"");
        let words = &first_simple(&prog).words;
        assert_eq!(words.len(), 3);
        assert!(matches!(words[1].segments[0], WordSegment::Bare(ref s) if s == "foo"));
        assert!(matches!(words[2].segments[0], WordSegment::DoubleQuoted(ref s) if s == "bar"));
    }

    // ===== assignment prefix =====

    #[test]
    fn single_assignment_prefix_then_command() {
        let prog = p("FOO=bar cmd a");
        let cmd = first_simple(&prog);
        assert_eq!(cmd.assignments.len(), 1);
        assert_eq!(cmd.assignments[0].name, "FOO");
        assert_eq!(bare(&cmd.assignments[0].value), "bar");
        assert_eq!(cmd.words.len(), 2);
        assert_eq!(bare(&cmd.words[0]), "cmd");
    }

    #[test]
    fn multiple_assignment_prefix() {
        let prog = p("A=1 B=2 C=3 cmd");
        let cmd = first_simple(&prog);
        assert_eq!(cmd.assignments.len(), 3);
        assert_eq!(cmd.assignments[0].name, "A");
        assert_eq!(cmd.assignments[1].name, "B");
        assert_eq!(cmd.assignments[2].name, "C");
        assert_eq!(cmd.words.len(), 1);
    }

    #[test]
    fn standalone_assignment_no_command() {
        // `FOO=bar` with nothing after it is a legal assignment-only
        // statement.
        let prog = p("FOO=bar");
        let cmd = first_simple(&prog);
        assert_eq!(cmd.assignments.len(), 1);
        assert!(cmd.words.is_empty());
    }

    #[test]
    fn empty_value_assignment() {
        let prog = p("FOO=");
        let cmd = first_simple(&prog);
        assert_eq!(cmd.assignments[0].name, "FOO");
        assert_eq!(bare(&cmd.assignments[0].value), "");
    }

    #[test]
    fn assignment_word_after_command_name_is_argument() {
        // `FOO=bar` past the command name is just an argument.
        let prog = p("cmd FOO=bar");
        let cmd = first_simple(&prog);
        assert!(cmd.assignments.is_empty());
        assert_eq!(cmd.words.len(), 2);
        assert_eq!(bare(&cmd.words[1]), "FOO=bar");
    }

    #[test]
    fn assignment_with_quoted_rhs_concatenates() {
        let prog = p("FOO='hello world' cmd");
        let cmd = first_simple(&prog);
        assert_eq!(cmd.assignments.len(), 1);
        assert_eq!(cmd.assignments[0].name, "FOO");
        // Value has two segments: empty bare leftover (skipped) and the quoted body.
        let v = &cmd.assignments[0].value;
        assert_eq!(v.segments.len(), 1);
        assert!(matches!(v.segments[0], WordSegment::SingleQuoted(ref s) if s == "hello world"));
    }

    // ===== function definitions =====

    #[test]
    fn posix_function_definition() {
        let prog = p("greet() { echo hi; }");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::FunctionDef {
                name,
                scope,
                captures,
                body,
            } => {
                assert_eq!(name, "greet");
                assert_eq!(*scope, FunctionScope::Dynamic);
                assert!(captures.is_none());
                assert!(matches!(body.kind, CompoundKind::BraceGroup { .. }));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn ksh_function_definition() {
        let prog = p("function greet { echo hi; }");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::FunctionDef {
                name, scope, captures, ..
            } => {
                assert_eq!(name, "greet");
                assert_eq!(*scope, FunctionScope::Static);
                assert!(captures.is_none());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn ksh_function_with_empty_paren() {
        // `function name() body` — also legal.
        let prog = p("function greet() { echo hi; }");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::FunctionDef {
                scope, captures, ..
            } => {
                assert_eq!(*scope, FunctionScope::Static);
                assert!(matches!(captures, Some(v) if v.is_empty()));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn kash_function_with_capture_list() {
        let prog = p("function f(a, b, c) { echo $a $b $c; }");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::FunctionDef {
                scope, captures, ..
            } => {
                assert_eq!(*scope, FunctionScope::Static);
                let caps = captures.as_ref().expect("capture list expected");
                assert_eq!(caps.len(), 3);
                assert_eq!(caps[0], "a");
                assert_eq!(caps[2], "c");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn function_body_can_be_a_subshell() {
        let prog = p("foo() ( echo a; echo b )");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::FunctionDef { body, .. } => {
                assert!(matches!(body.kind, CompoundKind::Subshell { .. }));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn function_body_can_be_a_loop() {
        let prog = p("function tick { while true; do echo .; done; }");
        let c = first_compound(&prog);
        match &c.kind {
            CompoundKind::FunctionDef { body, .. } => {
                // body is a brace group whose first statement is a while.
                let body_kind = &body.kind;
                let CompoundKind::BraceGroup { body: stmts } = body_kind else {
                    panic!("expected brace group, got {body_kind:?}");
                };
                let inner = compound_only(&stmts[0].list.head.stages[0]);
                assert!(matches!(inner.kind, CompoundKind::While { .. }));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn function_def_with_trailing_redirect() {
        let prog = p("foo() { echo a; } > out.txt");
        let c = first_compound(&prog);
        assert_eq!(c.redirects.len(), 1);
    }

    #[test]
    fn function_def_rejects_reserved_word_name() {
        assert!(parse("for() { :; }").is_err());
        assert!(parse("function if { :; }").is_err());
    }

    #[test]
    fn function_def_rejects_non_identifier_name() {
        // `1bad` starts with a digit.
        assert!(parse("function 1bad { :; }").is_err());
    }

    #[test]
    fn capture_list_rejects_quoted_names() {
        assert!(parse("function f('a', b) { :; }").is_err());
    }
}
