//! Evaluator — AST → side effects + values.
//!
//! Walks the AST under the active mode (see `mode.rs`), threading the
//! current scope, variable table, namespace registry, and typeclass
//! instance table. Hosts the typeclass dispatch rules
//! (`project_shell_typeclass.md`) and the `-secure` modifier's lock set
//! (`project_shell_set_options.md`).
//!
//! Scope of this commit: a runnable *skeleton* of the evaluator. Walks
//! the AST top-down, executes simple commands by dispatching to a
//! tiny built-in set (`:`, `true`, `false`, `echo`, `exit`),
//! propagates AND-OR short-circuit, and persists variable assignments
//! in the [`Scope`] table. Compound commands, function calls,
//! pipelines with more than one stage, redirections, and external
//! command exec all surface as `KashError::Runtime("not yet
//! supported")` for now — they land one at a time in follow-up
//! commits.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::ast::{
    AndOrList, AndOrOp, Command, Pipeline, Program, SimpleCommand, Statement, Word, WordSegment,
};
use crate::error::{KashError, Result};
use crate::mode::Mode;
use crate::scope::Scope;
use crate::value::Value;

/// Result of evaluating a statement / command — either a normal exit
/// status or an `exit N` request that should propagate upward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// Ordinary completion. The wrapped integer is `$?`.
    Status(i32),
    /// `exit N` was called. Outer evaluation should unwind.
    Exit(i32),
}

impl Outcome {
    /// Treat the outcome as a numeric status. `Exit(n)` collapses to
    /// `n` for the purposes of "did the last thing succeed?" checks;
    /// the caller still has to look at [`is_exit_request`] to decide
    /// whether to unwind.
    ///
    /// [`is_exit_request`]: Self::is_exit_request
    #[must_use]
    pub fn status(self) -> i32 {
        match self {
            Self::Status(n) | Self::Exit(n) => n,
        }
    }

    /// `true` iff the program asked us to exit (via the `exit`
    /// builtin) rather than just completing with a status.
    #[must_use]
    pub fn is_exit_request(self) -> bool {
        matches!(self, Self::Exit(_))
    }

    /// `true` iff [`status`](Self::status) is zero — POSIX "success".
    #[must_use]
    pub fn success(self) -> bool {
        self.status() == 0
    }
}

/// Evaluator state. Construct via [`Evaluator::new`] /
/// [`Evaluator::with_mode`], drive via [`Evaluator::eval_program`],
/// and drain accumulated stdout via [`Evaluator::take_output`].
pub struct Evaluator {
    scope: Scope,
    last_status: i32,
    /// Accumulator for `echo` / `print` builtin output. The host pulls
    /// the buffer with [`take_output`](Self::take_output) and decides
    /// when (and where) to display it; the evaluator never touches
    /// real I/O. That keeps the engine `no_std + alloc` friendly.
    output: String,
    /// Currently active mode. Not yet consulted (mode declarations
    /// aren't wired in), but threaded so callers can construct an
    /// evaluator under e.g. `default-secure`.
    mode: Mode,
}

impl Evaluator {
    /// New evaluator under the default mode.
    #[must_use]
    pub fn new() -> Self {
        Self::with_mode(Mode::default())
    }

    /// New evaluator under a specific mode.
    #[must_use]
    pub fn with_mode(mode: Mode) -> Self {
        Self {
            scope: Scope::new(),
            last_status: 0,
            output: String::new(),
            mode,
        }
    }

    /// Active mode.
    #[must_use]
    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    /// Last command's `$?`.
    #[must_use]
    pub fn last_status(&self) -> i32 {
        self.last_status
    }

    /// Read-only access to the variable scope (for tests and
    /// embedders that want to peek without running anything).
    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Drain the accumulated output buffer, returning its contents.
    /// The internal buffer is left empty.
    pub fn take_output(&mut self) -> String {
        core::mem::take(&mut self.output)
    }

    /// Evaluate a full program. The returned [`Outcome`] is the *last*
    /// statement's outcome; an `exit N` short-circuits and is reported
    /// as the final outcome.
    pub fn eval_program(&mut self, prog: &Program) -> Result<Outcome> {
        let mut outcome = Outcome::Status(0);
        for stmt in &prog.statements {
            outcome = self.eval_statement(stmt)?;
            if outcome.is_exit_request() {
                return Ok(outcome);
            }
        }
        Ok(outcome)
    }

    fn eval_statement(&mut self, stmt: &Statement) -> Result<Outcome> {
        let outcome = self.eval_and_or(&stmt.list)?;
        self.last_status = outcome.status();
        Ok(outcome)
    }

    fn eval_and_or(&mut self, list: &AndOrList) -> Result<Outcome> {
        let mut outcome = self.eval_pipeline(&list.head)?;
        if outcome.is_exit_request() {
            return Ok(outcome);
        }
        for (op, pipe) in &list.tail {
            let should_run = match op {
                AndOrOp::AndIf => outcome.success(),
                AndOrOp::OrIf => !outcome.success(),
            };
            if !should_run {
                continue;
            }
            outcome = self.eval_pipeline(pipe)?;
            if outcome.is_exit_request() {
                return Ok(outcome);
            }
        }
        Ok(outcome)
    }

    fn eval_pipeline(&mut self, pipe: &Pipeline) -> Result<Outcome> {
        if pipe.stages.len() > 1 {
            return Err(KashError::Runtime(
                "multi-stage pipelines are not yet wired in the evaluator skeleton".into(),
            ));
        }
        self.eval_command(&pipe.stages[0])
    }

    fn eval_command(&mut self, cmd: &Command) -> Result<Outcome> {
        match cmd {
            Command::Simple(s) => self.eval_simple(s),
            Command::Compound(_) => Err(KashError::Runtime(
                "compound commands are not yet wired in the evaluator skeleton".into(),
            )),
        }
    }

    fn eval_simple(&mut self, cmd: &SimpleCommand) -> Result<Outcome> {
        // Phase 1: assignment prefix. With no command words it persists
        // in the current scope (POSIX). With command words present the
        // POSIX rule would scope the assignments to the command's
        // environment only, but we don't exec external commands yet —
        // for now we just persist them, and revisit when external exec
        // lands.
        for a in &cmd.assignments {
            let value = expand_word(&a.value);
            self.scope.set(a.name.clone(), Value::Scalar(value));
        }
        if cmd.words.is_empty() {
            if !cmd.redirects.is_empty() {
                return Err(KashError::Runtime(
                    "redirects on assignment-only statements are not yet supported".into(),
                ));
            }
            return Ok(Outcome::Status(0));
        }
        if !cmd.redirects.is_empty() {
            return Err(KashError::Runtime(
                "redirections are not yet wired in the evaluator skeleton".into(),
            ));
        }
        // Phase 2: expand command name + arguments.
        let argv: Vec<String> = cmd.words.iter().map(expand_word).collect();
        // Phase 3: dispatch. External exec is deferred to a follow-up.
        let name = argv[0].as_str();
        match name {
            ":" | "true" => Ok(Outcome::Status(0)),
            "false" => Ok(Outcome::Status(1)),
            "echo" => {
                self.builtin_echo(&argv[1..]);
                Ok(Outcome::Status(0))
            }
            "exit" => self.builtin_exit(&argv[1..]),
            other => Err(KashError::NotFound(format!("command `{other}`"))),
        }
    }

    // ---------- builtins ----------

    fn builtin_echo(&mut self, args: &[String]) {
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                self.output.push(' ');
            }
            self.output.push_str(arg);
        }
        self.output.push('\n');
    }

    fn builtin_exit(&self, args: &[String]) -> Result<Outcome> {
        let code = if args.is_empty() {
            self.last_status
        } else if args.len() == 1 {
            args[0].parse::<i32>().map_err(|_| {
                KashError::Runtime(format!(
                    "exit: numeric argument required, got `{}`",
                    args[0]
                ))
            })?
        } else {
            return Err(KashError::Runtime(
                "exit: too many arguments".to_string(),
            ));
        };
        Ok(Outcome::Exit(code))
    }
}

impl Default for Evaluator {
    fn default() -> Self {
        Self::new()
    }
}

/// Skeleton word expansion: glue every segment's raw payload together,
/// dropping the quote-shape distinction. Real expansion (parameter
/// substitution, command substitution, glob, brace, …) lands one
/// phase at a time once the parser is fully wired.
fn expand_word(w: &Word) -> String {
    let mut out = String::new();
    for seg in &w.segments {
        match seg {
            WordSegment::Bare(s)
            | WordSegment::SingleQuoted(s)
            | WordSegment::DoubleQuoted(s)
            | WordSegment::AnsiC(s) => out.push_str(s),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn run(src: &str) -> (Outcome, String, Evaluator) {
        let prog = parse(src).expect("parse");
        let mut ev = Evaluator::new();
        let outcome = ev.eval_program(&prog).expect("eval");
        let out = ev.take_output();
        (outcome, out, ev)
    }

    #[test]
    fn colon_returns_zero() {
        let (o, out, _) = run(":");
        assert_eq!(o, Outcome::Status(0));
        assert!(out.is_empty());
    }

    #[test]
    fn true_and_false_statuses() {
        assert_eq!(run("true").0, Outcome::Status(0));
        assert_eq!(run("false").0, Outcome::Status(1));
    }

    #[test]
    fn echo_writes_to_output_buffer() {
        let (o, out, _) = run("echo hello world");
        assert_eq!(o, Outcome::Status(0));
        assert_eq!(out, "hello world\n");
    }

    #[test]
    fn andif_short_circuits_when_lhs_fails() {
        let (_, out, _) = run("false && echo skipped");
        assert!(out.is_empty());
    }

    #[test]
    fn orif_short_circuits_when_lhs_succeeds() {
        let (_, out, _) = run("true || echo skipped");
        assert!(out.is_empty());
    }

    #[test]
    fn orif_runs_when_lhs_fails() {
        let (_, out, _) = run("false || echo backup");
        assert_eq!(out, "backup\n");
    }

    #[test]
    fn andif_runs_when_lhs_succeeds() {
        let (_, out, _) = run("true && echo ok");
        assert_eq!(out, "ok\n");
    }

    #[test]
    fn semicolon_runs_every_statement() {
        let (_, out, _) = run("echo a; echo b; echo c");
        assert_eq!(out, "a\nb\nc\n");
    }

    #[test]
    fn assignment_only_persists_in_scope() {
        let (_, _, ev) = run("FOO=bar");
        assert_eq!(
            ev.scope().get("FOO").unwrap().to_scalar_string(),
            "bar"
        );
    }

    #[test]
    fn assignment_prefix_persists_too() {
        // Until external exec lands, prefix assignments persist
        // permanently (documented in eval_simple).
        let (_, _, ev) = run("FOO=bar echo done");
        assert_eq!(
            ev.scope().get("FOO").unwrap().to_scalar_string(),
            "bar"
        );
    }

    #[test]
    fn exit_propagates_outcome() {
        let (o, _, _) = run("exit 7");
        assert_eq!(o, Outcome::Exit(7));
    }

    #[test]
    fn exit_short_circuits_remaining_statements() {
        let (o, out, _) = run("echo a; exit 3; echo b");
        assert_eq!(o, Outcome::Exit(3));
        assert_eq!(out, "a\n");
    }

    #[test]
    fn exit_with_no_arg_uses_last_status() {
        let (o, _, _) = run("false; exit");
        assert_eq!(o, Outcome::Exit(1));
    }

    #[test]
    fn unknown_command_is_not_found() {
        let prog = parse("nope_such_cmd").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert_eq!(err.exit_code(), 127);
    }

    #[test]
    fn quoted_arguments_join_segments() {
        let (_, out, _) = run("echo 'foo bar'");
        assert_eq!(out, "foo bar\n");
    }

    #[test]
    fn compound_unsupported_yields_runtime_error() {
        let prog = parse("{ echo a; }").unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_err());
    }

    #[test]
    fn multistage_pipeline_unsupported_yields_runtime_error() {
        let prog = parse("echo a | true").unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_err());
    }
}
