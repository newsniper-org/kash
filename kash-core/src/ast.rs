//! Abstract syntax tree node definitions.
//!
//! Models commands, pipelines, redirections, quotes, expansion flags,
//! mode declarations, function definitions (POSIX + capture-list form),
//! typeclass / instance / namespace declarations, and the various
//! literal forms (strings, here-docs, numeric primitives, complex
//! literals). Designed to be allocation-friendly (Vec/Box) but
//! `no_std + alloc` compatible.
//!
//! Scope of this commit: the bottom layer needed to parse POSIX
//! command syntax — words (with their quoted-segment provenance),
//! a minimal redirect subset, simple commands, pipelines (with
//! `|&` coprocess flag), AND-OR lists, and statement terminators
//! (`;`, `&`, newline). Compound commands (`if`, `for`, `while`,
//! `case`, brace/subshell/arithmetic groups), function definitions,
//! assignments, here-docs, FD-dup redirects, and `!` negation are
//! intentionally *not* modelled yet — they'll land as the parser
//! grows.

use alloc::string::String;
use alloc::vec::Vec;

use crate::lexer::Span;

/// One token-sized fragment of an argument. A concatenated argument
/// like `foo"bar"$'baz'` is a [`Word`] whose `segments` Vec holds the
/// three segments in order, with no whitespace allowed between them.
///
/// Quote stripping is *not* performed here: the inner `String` is the
/// raw payload from the lexer. The expander decides when and how to
/// unquote so it can preserve provenance for error messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WordSegment {
    /// Unquoted bare text. May still contain `$var`, `$(cmd)`,
    /// backslash escapes, etc. — the expander handles those at runtime.
    Bare(String),
    /// `'...'` — interior bytes verbatim, never expanded.
    SingleQuoted(String),
    /// `"..."` — interior bytes; the expander processes `$var`,
    /// `$(cmd)`, `\\` escapes later.
    DoubleQuoted(String),
    /// `$'...'` — ANSI-C string; the expander processes its escape
    /// sequences (`\n`, `\xHH`, …) at evaluation time.
    AnsiC(String),
}

/// A complete command argument — one or more [`WordSegment`]s
/// concatenated with no whitespace between them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    /// One or more segments, in source order. Always non-empty.
    pub segments: Vec<WordSegment>,
    /// Source span covering the entire word (start of first segment to
    /// end of last).
    pub span: Span,
}

/// Redirection operator kind. Currently a minimal subset — FD dups,
/// here-docs, and here-strings will land in follow-up commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RedirectKind {
    /// `>file` — truncate-and-write stdout (or `fd`) to `file`.
    Output,
    /// `>>file` — append stdout (or `fd`) to `file`.
    Append,
    /// `<file` — read stdin (or `fd`) from `file`.
    Input,
    /// `&>file` — both stdout and stderr to `file`, truncate.
    OutputBoth,
    /// `&>>file` — both stdout and stderr to `file`, append.
    AppendBoth,
}

/// A single redirection clause attached to a [`SimpleCommand`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redirect {
    /// What the redirection does.
    pub kind: RedirectKind,
    /// Explicit fd prefix on the operator. `None` means "use the
    /// operator's default fd" (stdout for `>` / `>>`, stdin for `<`).
    /// The fd-prefix syntax (`2>file`) is not parsed yet, so this is
    /// always `None` in the current parser cut.
    pub fd: Option<i32>,
    /// Right-hand side: target filename / word.
    pub target: Word,
    /// Source span covering the whole redirect (operator + target).
    pub span: Span,
}

/// A simple command: optional assignment prefix, command name plus
/// arguments, plus zero or more redirections.
///
/// The first entry of `words` is the command name; the rest are
/// arguments. If `words` is empty the command is a pure
/// assignment-and/or-redirect form (legal in POSIX as a side-effecting
/// no-op).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimpleCommand {
    /// `KEY=VALUE` prefix assignments. Empty in the current parser cut
    /// (assignment parsing lands with parameter expansion).
    pub assignments: Vec<Assignment>,
    /// Command name (`words[0]`) followed by arguments.
    pub words: Vec<Word>,
    /// Redirection clauses, in source order.
    pub redirects: Vec<Redirect>,
    /// Source span covering all of the above.
    pub span: Span,
}

/// `KEY=VALUE` prefix on a [`SimpleCommand`]. Reserved for the
/// follow-up commit that wires up assignment parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    /// Variable name.
    pub name: String,
    /// Right-hand side expression as a word.
    pub value: Word,
    /// Source span covering `name=value`.
    pub span: Span,
}

/// Pipeline join operator. Determines runtime semantics of the join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeOp {
    /// `|` — ordinary stdout pipe.
    Pipe,
    /// `|&` — coprocess pipe (per `project_shell_subshell_pipeline.md`,
    /// the locked ksh93 baseline across every mode).
    PipeAmp,
}

/// A pipeline: one or more [`SimpleCommand`]s joined by `|` / `|&`.
///
/// `stages` and `ops` are kept in lock-step. `ops[0]` is a placeholder
/// — only `ops[i]` for `i >= 1` describes a real join (the one *into*
/// stage `i`). The placeholder is always [`PipeOp::Pipe`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pipeline {
    /// Pipeline stages, in left-to-right source order. Always
    /// non-empty.
    pub stages: Vec<SimpleCommand>,
    /// Join operators. `ops.len() == stages.len()` always — entry 0 is
    /// an unused placeholder.
    pub ops: Vec<PipeOp>,
    /// Source span covering the whole pipeline.
    pub span: Span,
}

/// AND-OR list join operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AndOrOp {
    /// `&&` — run RHS only if LHS succeeded.
    AndIf,
    /// `||` — run RHS only if LHS failed.
    OrIf,
}

/// `pipeline (&& pipeline | || pipeline)*`, joined left-associatively.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AndOrList {
    /// Leading pipeline.
    pub head: Pipeline,
    /// Subsequent `(op, pipeline)` pairs, applied left-to-right.
    pub tail: Vec<(AndOrOp, Pipeline)>,
    /// Source span covering head + all tail entries.
    pub span: Span,
}

/// Top-level statement terminator. Determines whether the and-or list
/// runs in the foreground or in the background.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminator {
    /// `;`, newline, or end-of-input — synchronous foreground execution.
    Sync,
    /// `&` — backgrounded; the shell continues without waiting.
    Background,
}

/// One top-level command: an and-or list with a terminator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The and-or list itself.
    pub list: AndOrList,
    /// Trailing terminator.
    pub terminator: Terminator,
    /// Source span covering list + terminator.
    pub span: Span,
}

/// A full source unit: zero or more [`Command`]s in source order.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Program {
    /// Commands in source order. May be empty (empty input / all
    /// comments / only blank lines).
    pub commands: Vec<Command>,
}
