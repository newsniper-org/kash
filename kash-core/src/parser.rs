//! Parser — token stream → AST.
//!
//! Hand-written recursive descent. Produces the AST defined in `ast.rs`,
//! which downstream consumers (evaluator, transpiler plugins,
//! formatter) walk. Per `project_kash_implementation.md`, no external
//! parser-combinator crate is used — error messages and recovery are
//! tuned for the REPL.
//!
//! Scope of this commit: the POSIX command grammar's three lowest
//! productions — simple command, pipeline (`|` / `|&`), and AND-OR
//! list (`&&` / `||`) — plus statement terminators (`;`, `&`, newline)
//! at the top level. A minimal redirect subset (`>`, `>>`, `<`, `&>`,
//! `&>>`) is recognised so simple I/O cases work. Everything else —
//! compound commands, function definitions, assignment prefixes,
//! here-docs, FD-dups, `!` negation, mode/namespace/typeclass
//! declarations — lands in follow-up commits.

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use crate::ast::{
    AndOrList, AndOrOp, Assignment, Command, PipeOp, Pipeline, Program, Redirect, RedirectKind,
    SimpleCommand, Terminator, Word, WordSegment,
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
    peeked: Option<Token>,
}

/// One-shot convenience: parse a complete source unit.
pub fn parse(source: &str) -> Result<Program> {
    Parser::new(source).parse_program()
}

impl<'src> Parser<'src> {
    /// New parser over `source`. Does not advance the lexer.
    #[must_use]
    pub fn new(source: &'src str) -> Self {
        Self {
            lexer: Lexer::new(source),
            peeked: None,
        }
    }

    // ---------- token plumbing ----------

    fn peek(&mut self) -> Result<&Token> {
        if self.peeked.is_none() {
            self.peeked = Some(self.lexer.next_token()?);
        }
        Ok(self.peeked.as_ref().expect("just populated"))
    }

    fn peek_kind(&mut self) -> Result<&TokenKind> {
        Ok(&self.peek()?.kind)
    }

    fn bump(&mut self) -> Result<Token> {
        if let Some(t) = self.peeked.take() {
            return Ok(t);
        }
        self.lexer.next_token()
    }

    /// Eat any leading `Newline` tokens (which double as soft
    /// statement separators after `&&`, `||`, `|`, `|&`).
    fn skip_newlines(&mut self) -> Result<()> {
        while matches!(self.peek_kind()?, TokenKind::Newline) {
            self.bump()?;
        }
        Ok(())
    }

    // ---------- entry point ----------

    /// Parse a full source unit (zero or more commands).
    pub fn parse_program(&mut self) -> Result<Program> {
        let mut commands = Vec::new();
        loop {
            self.skip_newlines()?;
            if matches!(self.peek_kind()?, TokenKind::Eof) {
                break;
            }
            commands.push(self.parse_command()?);
        }
        Ok(Program { commands })
    }

    /// Parse one and-or list plus its terminator.
    fn parse_command(&mut self) -> Result<Command> {
        let list = self.parse_and_or()?;
        let start = list.span.start;
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
            other => {
                return Err(KashError::Parse(format!(
                    "expected `;`, `&`, newline or end of input after command, got {other:?}"
                )));
            }
        };
        Ok(Command {
            list,
            terminator: term,
            span: Span::new(start, end),
        })
    }

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
            // POSIX: a newline is allowed after `&&` / `||`.
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

    fn parse_pipeline(&mut self) -> Result<Pipeline> {
        let first = self.parse_simple_command()?;
        let start = first.span.start;
        let mut end = first.span.end;
        let mut stages = vec![first];
        // Entry 0 is an unused placeholder, per the AST contract.
        let mut ops = vec![PipeOp::Pipe];
        loop {
            let op = match self.peek_kind()? {
                TokenKind::Op(Op::Pipe) => PipeOp::Pipe,
                TokenKind::Op(Op::PipeAmp) => PipeOp::PipeAmp,
                _ => break,
            };
            self.bump()?;
            // POSIX: a newline is allowed after `|`. `|&` is a kash/ksh93
            // extension but we apply the same rule for consistency.
            self.skip_newlines()?;
            let next = self.parse_simple_command()?;
            end = next.span.end;
            stages.push(next);
            ops.push(op);
        }
        Ok(Pipeline {
            stages,
            ops,
            span: Span::new(start, end),
        })
    }

    fn parse_simple_command(&mut self) -> Result<SimpleCommand> {
        let start = self.peek()?.span.start;
        let mut end = start;
        let mut words = Vec::new();
        let mut redirects = Vec::new();
        loop {
            match self.peek_kind()? {
                TokenKind::Word(_)
                | TokenKind::SingleQuoted(_)
                | TokenKind::DoubleQuoted(_)
                | TokenKind::AnsiCString(_) => {
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
        if words.is_empty() && redirects.is_empty() {
            // Caller is expected not to invoke us in this state; if it
            // happens it's a parse error against the current token.
            let got = self.peek_kind()?.clone();
            return Err(KashError::Parse(format!(
                "expected a command, got {got:?}"
            )));
        }
        Ok(SimpleCommand {
            // Assignment-prefix parsing lands with parameter expansion;
            // empty for now so the AST shape is forward-compatible.
            assignments: Vec::<Assignment>::new(),
            words,
            redirects,
            span: Span::new(start, end),
        })
    }

    fn parse_word(&mut self) -> Result<Word> {
        let tok = self.bump()?;
        let segment = match tok.kind {
            TokenKind::Word(s) => WordSegment::Bare(s),
            TokenKind::SingleQuoted(s) => WordSegment::SingleQuoted(s),
            TokenKind::DoubleQuoted(s) => WordSegment::DoubleQuoted(s),
            TokenKind::AnsiCString(s) => WordSegment::AnsiC(s),
            other => {
                return Err(KashError::Parse(format!(
                    "expected a word-like token, got {other:?}"
                )));
            }
        };
        Ok(Word {
            segments: vec![segment],
            span: tok.span,
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
        let kind = match op {
            Op::Gt => RedirectKind::Output,
            Op::DoubleGt => RedirectKind::Append,
            Op::Lt => RedirectKind::Input,
            Op::AmpGt => RedirectKind::OutputBoth,
            Op::AmpGtGt => RedirectKind::AppendBoth,
            other => {
                return Err(KashError::Parse(format!(
                    "unsupported redirect operator {other:?}"
                )));
            }
        };
        // Target word is required.
        if !matches!(
            self.peek_kind()?,
            TokenKind::Word(_)
                | TokenKind::SingleQuoted(_)
                | TokenKind::DoubleQuoted(_)
                | TokenKind::AnsiCString(_)
        ) {
            return Err(KashError::Parse(format!(
                "expected a filename after `{}`",
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
}

const fn is_redirect_op(op: Op) -> bool {
    matches!(
        op,
        Op::Gt | Op::DoubleGt | Op::Lt | Op::AmpGt | Op::AmpGtGt
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn p(src: &str) -> Program {
        parse(src).unwrap_or_else(|e| panic!("parse({src:?}) failed: {e}"))
    }

    fn bare(w: &Word) -> &str {
        match &w.segments[0] {
            WordSegment::Bare(s) => s,
            other => panic!("expected Bare segment, got {other:?}"),
        }
    }

    #[test]
    fn empty_input_yields_empty_program() {
        let prog = p("");
        assert!(prog.commands.is_empty());
    }

    #[test]
    fn whitespace_and_newlines_only() {
        let prog = p("   \n\n  \n");
        assert!(prog.commands.is_empty());
    }

    #[test]
    fn simple_two_word_command() {
        let prog = p("echo hello");
        assert_eq!(prog.commands.len(), 1);
        let cmd = &prog.commands[0];
        assert_eq!(cmd.terminator, Terminator::Sync);
        let list = &cmd.list;
        assert!(list.tail.is_empty());
        let pipe = &list.head;
        assert_eq!(pipe.stages.len(), 1);
        let stage = &pipe.stages[0];
        assert_eq!(stage.words.len(), 2);
        assert_eq!(bare(&stage.words[0]), "echo");
        assert_eq!(bare(&stage.words[1]), "hello");
        assert!(stage.redirects.is_empty());
    }

    #[test]
    fn three_commands_separated_by_semicolons() {
        let prog = p("a; b; c");
        assert_eq!(prog.commands.len(), 3);
        for (cmd, name) in prog.commands.iter().zip(["a", "b", "c"]) {
            assert_eq!(cmd.terminator, Terminator::Sync);
            assert_eq!(bare(&cmd.list.head.stages[0].words[0]), name);
        }
    }

    #[test]
    fn background_terminator() {
        let prog = p("sleep 1 &");
        assert_eq!(prog.commands.len(), 1);
        assert_eq!(prog.commands[0].terminator, Terminator::Background);
    }

    #[test]
    fn newline_acts_as_terminator() {
        let prog = p("a\nb\n");
        assert_eq!(prog.commands.len(), 2);
        assert_eq!(bare(&prog.commands[0].list.head.stages[0].words[0]), "a");
        assert_eq!(bare(&prog.commands[1].list.head.stages[0].words[0]), "b");
    }

    #[test]
    fn and_or_list_left_associative() {
        let prog = p("a && b || c");
        assert_eq!(prog.commands.len(), 1);
        let list = &prog.commands[0].list;
        assert_eq!(bare(&list.head.stages[0].words[0]), "a");
        assert_eq!(list.tail.len(), 2);
        assert_eq!(list.tail[0].0, AndOrOp::AndIf);
        assert_eq!(bare(&list.tail[0].1.stages[0].words[0]), "b");
        assert_eq!(list.tail[1].0, AndOrOp::OrIf);
        assert_eq!(bare(&list.tail[1].1.stages[0].words[0]), "c");
    }

    #[test]
    fn pipeline_with_three_stages() {
        let prog = p("a | b | c");
        let pipe = &prog.commands[0].list.head;
        assert_eq!(pipe.stages.len(), 3);
        assert_eq!(pipe.ops, vec![PipeOp::Pipe, PipeOp::Pipe, PipeOp::Pipe]);
        assert_eq!(bare(&pipe.stages[0].words[0]), "a");
        assert_eq!(bare(&pipe.stages[1].words[0]), "b");
        assert_eq!(bare(&pipe.stages[2].words[0]), "c");
    }

    #[test]
    fn coprocess_pipe_amp_recognised() {
        let prog = p("a |& b");
        let pipe = &prog.commands[0].list.head;
        assert_eq!(pipe.stages.len(), 2);
        // ops[0] is the unused placeholder; ops[1] is the real join.
        assert_eq!(pipe.ops[1], PipeOp::PipeAmp);
    }

    #[test]
    fn newline_allowed_after_pipe_and_andor() {
        let prog = p("a |\nb && \n c");
        let list = &prog.commands[0].list;
        // Pipeline: a | b.
        assert_eq!(list.head.stages.len(), 2);
        // Then `&& c`.
        assert_eq!(list.tail.len(), 1);
        assert_eq!(list.tail[0].0, AndOrOp::AndIf);
    }

    #[test]
    fn output_redirect_attached_to_command() {
        let prog = p("echo hi > out.txt");
        let stage = &prog.commands[0].list.head.stages[0];
        assert_eq!(stage.redirects.len(), 1);
        assert_eq!(stage.redirects[0].kind, RedirectKind::Output);
        assert_eq!(bare(&stage.redirects[0].target), "out.txt");
    }

    #[test]
    fn multiple_redirects_in_source_order() {
        let prog = p("cat < in.txt > out.txt >> log.txt");
        let stage = &prog.commands[0].list.head.stages[0];
        assert_eq!(stage.redirects.len(), 3);
        assert_eq!(stage.redirects[0].kind, RedirectKind::Input);
        assert_eq!(stage.redirects[1].kind, RedirectKind::Output);
        assert_eq!(stage.redirects[2].kind, RedirectKind::Append);
    }

    #[test]
    fn amp_gt_redirects_both() {
        let prog = p("noisy &> log.txt");
        let stage = &prog.commands[0].list.head.stages[0];
        assert_eq!(stage.redirects[0].kind, RedirectKind::OutputBoth);
    }

    #[test]
    fn quoted_segments_preserved() {
        let prog = p("echo 'lit' \"dq\" $'an'");
        let words = &prog.commands[0].list.head.stages[0].words;
        assert_eq!(words.len(), 4);
        assert!(matches!(words[1].segments[0], WordSegment::SingleQuoted(ref s) if s == "lit"));
        assert!(matches!(words[2].segments[0], WordSegment::DoubleQuoted(ref s) if s == "dq"));
        assert!(matches!(words[3].segments[0], WordSegment::AnsiC(ref s) if s == "an"));
    }

    #[test]
    fn redirect_without_target_errors() {
        let err = parse("echo >").unwrap_err();
        assert!(err.to_string().contains("filename"), "got: {err}");
    }

    #[test]
    fn dangling_andand_errors() {
        // `a &&` with no rhs should not parse.
        assert!(parse("a && ").is_err());
    }

    #[test]
    fn dangling_pipe_errors() {
        assert!(parse("a |").is_err());
    }

    #[test]
    fn spans_cover_full_command() {
        // The first command's span should start at 0 and reach the
        // terminator. We don't check exact byte offsets to avoid
        // coupling to whitespace details — just monotonicity.
        let prog = p("echo hi; echo bye");
        let cmd0 = &prog.commands[0];
        assert!(cmd0.span.start < cmd0.span.end);
        assert!(prog.commands[1].span.start > cmd0.span.end - 1);
    }
}
