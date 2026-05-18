//! Evaluator — AST → side effects + values.
//!
//! Walks the AST under the active mode (see `mode.rs`), threading the
//! current scope, variable table, namespace registry, and typeclass
//! instance table. Hosts the typeclass dispatch rules
//! (`project_shell_typeclass.md`) and the `-secure` modifier's lock set
//! (`project_shell_set_options.md`).
//!
//! Scope of this commit: compound commands (`{ }`, `( )`, `if`,
//! `while`/`until`, `for`, `case` with `;;`/`;&`/`;;&`), function
//! definitions + calls (POSIX dynamic and `function`-form static
//! variants), and parameter expansion — `$VAR`, `${VAR}`,
//! `${VAR:-…}`/`${VAR:=…}`/`${VAR:?…}`/`${VAR:+…}` (and their
//! colon-less forms), `${#VAR}`, plus the specials `$?`, `$#`, `$0`-
//! `$9`. Multi-stage pipelines and external `exec` are still stubbed —
//! they land in the next commit.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::ast::{
    AndOrList, AndOrOp, CaseFallthrough, CaseItem, Command, CompoundCommand, CompoundKind,
    FunctionScope, IfBranch, Pipeline, Program, SimpleCommand, Statement, Word, WordSegment,
};
use crate::error::{KashError, Result};
use crate::mode::Mode;
use crate::scope::Scope;
use crate::value::Value;
use kash_macros::ifstd;

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

/// Set-style shell options. Toggled by `set -o NAME` / `set +o NAME`
/// and by the short-form letter flags (`set -e`, `set +u`, …). Only
/// the three POSIX big-three are wired so far; the wider `set -o`
/// surface is locked in `project_shell_set_options.md` and lands in
/// follow-up commits.
#[derive(Clone, Copy, Debug, Default)]
pub struct ShellOptions {
    /// `errexit` / `-e` — abort on the first command that exits
    /// non-zero in a context where the failure isn't being inspected
    /// (`if`/`while` condition, `&&`/`||` LHS, `!` prefix).
    pub errexit: bool,
    /// `nounset` / `-u` — reading an unset variable (plain `$VAR`,
    /// not `${VAR:-…}` / `${VAR-…}` etc.) is an error.
    pub nounset: bool,
    /// `pipefail` — the pipeline's exit status is the rightmost
    /// non-zero stage's, falling back to zero only if every stage
    /// succeeded.
    pub pipefail: bool,
    /// `xtrace` / `-x` — print every simple command's expanded argv
    /// to the trace buffer prefixed with the value of `PS4` (default
    /// `"+ "`) before running it.
    pub xtrace: bool,
}

/// One registered function. Stored owned so the call site doesn't
/// need a borrow of the original AST.
#[derive(Clone, Debug)]
struct FunctionEntry {
    scope: FunctionScope,
    captures: Option<Vec<String>>,
    body: Box<CompoundCommand>,
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
    /// Mirror of stderr for `set -x` (xtrace) lines and any future
    /// diagnostic emission. Kept separate from `output` so a host
    /// that wants to route it elsewhere (a real `stderr` fd, a debug
    /// pane, …) can drain just this buffer.
    trace_output: String,
    /// Currently active mode. Not yet consulted (mode declarations
    /// aren't wired in), but threaded so callers can construct an
    /// evaluator under e.g. `default-secure`.
    mode: Mode,
    /// Current positional arguments (`$1`, `$2`, …). Top-level value
    /// is empty; function calls push their argument list and restore
    /// the caller's on return.
    positionals: Vec<String>,
    /// Stack of saved positional sets for nested function calls.
    positionals_stack: Vec<Vec<String>>,
    /// Function registry: name → definition. Functions live in a flat
    /// table for now; namespace scoping lights up later.
    functions: BTreeMap<String, FunctionEntry>,
    /// Alias table: NAME → expansion text. Substitution happens at
    /// the start of a simple command's dispatch — the first
    /// (already-expanded) argv slot is matched against this table,
    /// and on a hit the slot is replaced by the alias body split on
    /// whitespace. Recursion is bounded per-command by an
    /// already-seen set so a self-referential alias (e.g.
    /// `alias ls='ls --color'`) terminates.
    aliases: BTreeMap<String, String>,
    /// Trap action registry: signal name → command source. Names are
    /// normalised to upper-case without a `SIG` prefix
    /// (`INT`, `TERM`, `EXIT`, …). The pseudo-signals `EXIT` / `ERR`
    /// are wired to fire at the appropriate points in evaluation; the
    /// real OS signals are accepted into the table but not yet
    /// delivered (that lands with the unix-only signal layer).
    traps: BTreeMap<String, String>,
    /// Re-entrancy guard for trap actions — a trap that itself fires
    /// the same trap (e.g. `trap 'false' ERR` invoking ERR again on
    /// the `false`) would otherwise loop forever.
    in_trap: bool,
    /// Active `set -o` / short-form options.
    options: ShellOptions,
    /// When `false`, the statement loop suppresses `errexit` even if
    /// the option is on. Used while evaluating an `if` / `while` /
    /// `until` condition list — those contexts are explicitly checked
    /// and don't trigger the option per POSIX.
    errexit_active: bool,
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
            trace_output: String::new(),
            mode,
            positionals: Vec::new(),
            positionals_stack: Vec::new(),
            functions: BTreeMap::new(),
            aliases: BTreeMap::new(),
            traps: BTreeMap::new(),
            in_trap: false,
            options: ShellOptions::default(),
            errexit_active: true,
        }
    }

    /// Read-only access to the active option set.
    #[must_use]
    pub fn options(&self) -> &ShellOptions {
        &self.options
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

    #[cfg(test)]
    pub(crate) fn aliases_for_test(&self) -> &BTreeMap<String, String> {
        &self.aliases
    }

    /// Drain the accumulated output buffer, returning its contents.
    /// The internal buffer is left empty.
    pub fn take_output(&mut self) -> String {
        core::mem::take(&mut self.output)
    }

    /// Drain the accumulated trace buffer (xtrace lines), returning
    /// its contents. The internal buffer is left empty.
    pub fn take_trace_output(&mut self) -> String {
        core::mem::take(&mut self.trace_output)
    }

    /// Evaluate a full program. The returned [`Outcome`] is the *last*
    /// statement's outcome; an `exit N` short-circuits and is reported
    /// as the final outcome. The `EXIT` trap, if registered, runs
    /// before this function returns — even on error, even on
    /// `Outcome::Exit`.
    pub fn eval_program(&mut self, prog: &Program) -> Result<Outcome> {
        let result = self.eval_statements(&prog.statements);
        if let Some(cmd) = self.traps.get("EXIT").cloned() {
            // Don't let a failing EXIT trap mask the real outcome.
            let _ = self.run_trap_command(&cmd);
        }
        result
    }

    /// Run a trap action. Parses and evaluates `cmd` as a small shell
    /// program inside the current environment, guarded against
    /// re-entry (a trap that fires the same trap doesn't recurse).
    /// Errors from the trap body are swallowed — POSIX leaves trap
    /// failure mostly invisible.
    fn run_trap_command(&mut self, cmd: &str) -> Result<Outcome> {
        if self.in_trap {
            return Ok(Outcome::Status(0));
        }
        self.in_trap = true;
        let prog = match crate::parser::parse(cmd) {
            Ok(p) => p,
            Err(_) => {
                self.in_trap = false;
                return Ok(Outcome::Status(0));
            }
        };
        let res = self.eval_statements(&prog.statements);
        self.in_trap = false;
        res
    }

    fn eval_statements(&mut self, stmts: &[Statement]) -> Result<Outcome> {
        let mut outcome = Outcome::Status(0);
        for stmt in stmts {
            outcome = self.eval_statement(stmt)?;
            if outcome.is_exit_request() {
                return Ok(outcome);
            }
            // ERR trap fires on a non-zero status whenever it would
            // also trigger `errexit` — i.e. anywhere outside an
            // explicitly-checked context (condition list, etc.).
            if !outcome.success() && self.errexit_active {
                if let Some(cmd) = self.traps.get("ERR").cloned() {
                    let _ = self.run_trap_command(&cmd);
                }
            }
            if self.options.errexit && self.errexit_active && !outcome.success() {
                return Ok(Outcome::Exit(outcome.status()));
            }
        }
        Ok(outcome)
    }

    /// Run `f` with `errexit` temporarily suppressed (used for
    /// `if`/`while`/`until` condition lists, which POSIX exempts).
    fn with_errexit_inactive<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        let saved = self.errexit_active;
        self.errexit_active = false;
        let r = f(self);
        self.errexit_active = saved;
        r
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
            #[cfg(feature = "std")]
            {
                return self.run_pipeline_external(pipe);
            }
            #[cfg(not(feature = "std"))]
            {
                return Err(KashError::Runtime(
                    "multi-stage pipelines require the `std` feature".into(),
                ));
            }
        }
        self.eval_command(&pipe.stages[0])
    }

    fn eval_command(&mut self, cmd: &Command) -> Result<Outcome> {
        match cmd {
            Command::Simple(s) => self.eval_simple(s),
            Command::Compound(c) => self.eval_compound(c),
        }
    }

    // ---------- simple commands ----------

    fn eval_simple(&mut self, cmd: &SimpleCommand) -> Result<Outcome> {
        // Phase 1: assignment prefix. With no command words it persists
        // in the current scope (POSIX). With command words present the
        // POSIX rule would scope the assignments to the command's
        // environment only, but we don't exec external commands yet —
        // we just persist them, and revisit when external exec lands.
        for a in &cmd.assignments {
            let value = self.expand_word(&a.value)?;
            self.scope.assign(&a.name, Value::Scalar(value))?;
        }
        if cmd.words.is_empty() {
            if !cmd.redirects.is_empty() {
                // POSIX: a redirect with no command opens the files
                // (so e.g. `> file` truncates) but doesn't otherwise
                // run anything. We hand this off to the std-only
                // redirect helper so the file work happens in one
                // place.
                #[cfg(feature = "std")]
                {
                    return self.open_redirect_side_effects(&cmd.redirects);
                }
                #[cfg(not(feature = "std"))]
                {
                    return Err(KashError::Runtime(
                        "redirections require the `std` feature".into(),
                    ));
                }
            }
            return Ok(Outcome::Status(0));
        }
        // Phase 2: expand command name + arguments with POSIX field
        // splitting (`expand_word_to_fields` does the work).
        let mut argv: Vec<String> = Vec::with_capacity(cmd.words.len());
        for w in &cmd.words {
            argv.extend(self.expand_word_to_fields(w)?);
        }
        if argv.is_empty() {
            // All command words vanished after expansion — treat the
            // whole simple command as a successful no-op (`A=1` with
            // an empty word list lands here too).
            return Ok(Outcome::Status(0));
        }
        // Alias resolution: substitute the first slot from
        // `self.aliases`, splitting the expansion text on whitespace.
        // Loop so chained aliases work, but bound the loop with an
        // already-seen set so a self-referential entry can't recurse
        // forever.
        let mut seen: BTreeSet<String> = BTreeSet::new();
        loop {
            let head = argv[0].clone();
            if seen.contains(&head) {
                break;
            }
            let Some(body) = self.aliases.get(&head).cloned() else {
                break;
            };
            seen.insert(head);
            let parts: Vec<String> = body
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();
            if parts.is_empty() {
                break;
            }
            let tail: Vec<String> = argv.split_off(1);
            argv = parts;
            argv.extend(tail);
        }
        // xtrace emission happens after alias substitution but
        // before redirect application, so the trace shows the
        // command exactly as it will run.
        self.maybe_xtrace(&argv);
        if !cmd.redirects.is_empty() {
            #[cfg(feature = "std")]
            {
                return self.eval_with_redirects(cmd, &argv);
            }
            #[cfg(not(feature = "std"))]
            {
                return Err(KashError::Runtime(
                    "redirections require the `std` feature".into(),
                ));
            }
        }
        // Phase 3: dispatch. Function lookup first (per POSIX, regular
        // builtins lose to user functions); then builtins; then NotFound
        // until external exec lands.
        let name = argv[0].as_str();
        if self.functions.contains_key(name) {
            return self.call_function(&argv);
        }
        match name {
            ":" | "true" => Ok(Outcome::Status(0)),
            "false" => Ok(Outcome::Status(1)),
            "echo" => {
                self.builtin_echo(&argv[1..]);
                Ok(Outcome::Status(0))
            }
            "exit" => self.builtin_exit(&argv[1..]),
            "set" => self.builtin_set(&argv[1..]),
            "unset" => self.builtin_unset(&argv[1..]),
            "shift" => self.builtin_shift(&argv[1..]),
            "local" => self.builtin_local(&argv[1..]),
            "readonly" => self.builtin_readonly(&argv[1..]),
            "test" => builtin_test(false, &argv[1..]),
            "[" => builtin_test(true, &argv[1..]),
            "trap" => self.builtin_trap(&argv[1..]),
            "alias" => self.builtin_alias(&argv[1..]),
            "unalias" => self.builtin_unalias(&argv[1..]),
            _ => self.run_external(&argv),
        }
    }

    /// Run `argv[0]` as an external program. Available only under
    /// `std` — the alloc-only build collapses this into `NotFound`.
    fn run_external(&mut self, argv: &[String]) -> Result<Outcome> {
        #[cfg(feature = "std")]
        {
            self.run_external_std(argv)
        }
        #[cfg(not(feature = "std"))]
        {
            Err(KashError::NotFound(format!("command `{}`", argv[0])))
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

    /// `set` builtin: toggles shell options (`-o NAME` / `+o NAME` and
    /// the short letter flags `-e`/`-u`/etc.), then — if any
    /// positional-looking arguments remain — rebinds `$1`/`$2`/…
    fn builtin_set(&mut self, args: &[String]) -> Result<Outcome> {
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if a == "--" {
                i += 1;
                break;
            }
            if a == "-o" || a == "+o" {
                let on = a == "-o";
                i += 1;
                let Some(name) = args.get(i) else {
                    return Err(KashError::Runtime(
                        "set: -o requires an option name".into(),
                    ));
                };
                self.set_long_option(name, on)?;
                i += 1;
                continue;
            }
            if let Some(rest) = a.strip_prefix('-') {
                if rest.is_empty() {
                    // bare `-` ends option processing per POSIX, with
                    // the difference that it does NOT reset $@ — same
                    // as `--` for our purposes.
                    i += 1;
                    break;
                }
                for c in rest.chars() {
                    self.set_short_option(c, true)?;
                }
                i += 1;
                continue;
            }
            if let Some(rest) = a.strip_prefix('+') {
                for c in rest.chars() {
                    self.set_short_option(c, false)?;
                }
                i += 1;
                continue;
            }
            // First non-option argument — rebind positionals from here.
            break;
        }
        if i < args.len() {
            self.positionals = args[i..].to_vec();
        }
        Ok(Outcome::Status(0))
    }

    fn set_long_option(&mut self, name: &str, on: bool) -> Result<()> {
        match name {
            "errexit" => self.options.errexit = on,
            "nounset" => self.options.nounset = on,
            "pipefail" => self.options.pipefail = on,
            "xtrace" => self.options.xtrace = on,
            other => {
                return Err(KashError::Runtime(alloc::format!(
                    "set -o: unknown option `{other}`"
                )));
            }
        }
        Ok(())
    }

    fn set_short_option(&mut self, c: char, on: bool) -> Result<()> {
        match c {
            'e' => self.options.errexit = on,
            'u' => self.options.nounset = on,
            'x' => self.options.xtrace = on,
            other => {
                return Err(KashError::Runtime(alloc::format!(
                    "set: unknown option `-{other}`"
                )));
            }
        }
        Ok(())
    }

    /// Emit `argv` to the trace buffer if `xtrace` is on. PS4 (default
    /// `"+ "`) prefixes each line; arguments are joined with a single
    /// space. Quoting is *not* re-applied (matching bash's minimal
    /// xtrace output) — the trace is a debugging aid, not a precise
    /// re-serialisation.
    fn maybe_xtrace(&mut self, argv: &[String]) {
        if !self.options.xtrace {
            return;
        }
        let ps4 = self
            .scope
            .get("PS4")
            .map(|v| v.to_scalar_string())
            .unwrap_or_else(|| "+ ".into());
        self.trace_output.push_str(&ps4);
        for (i, a) in argv.iter().enumerate() {
            if i > 0 {
                self.trace_output.push(' ');
            }
            self.trace_output.push_str(a);
        }
        self.trace_output.push('\n');
    }

    fn builtin_unset(&mut self, args: &[String]) -> Result<Outcome> {
        // Simplified: removes the nearest binding for each name. The
        // proper `unset -v`/`-f` split (unset variable vs function)
        // lands with the full builtin surface.
        for name in args {
            if self.scope.is_readonly(name) {
                return Err(KashError::Readonly(name.clone()));
            }
            // A non-existent name returning 0 matches POSIX behaviour.
            let _ = self.scope.unset(name);
            // Allow unsetting a function as a convenience.
            self.functions.remove(name);
        }
        Ok(Outcome::Status(0))
    }

    fn builtin_local(&mut self, args: &[String]) -> Result<Outcome> {
        if !self.scope.in_function() {
            return Err(KashError::Runtime(
                "local: can only be used inside a function".into(),
            ));
        }
        for arg in args {
            let (name, value) = parse_name_eq_value(arg)?;
            self.scope.assign_local(&name, Value::Scalar(value))?;
        }
        Ok(Outcome::Status(0))
    }

    /// `alias [NAME[=VALUE] ...]` builtin.
    ///
    /// - `alias` with no args lists every entry (`alias NAME='VALUE'`,
    ///   one per line).
    /// - `alias NAME=VALUE` installs / overwrites an entry.
    /// - `alias NAME` (no `=`) prints just that entry; errors if the
    ///   name is unset.
    fn builtin_alias(&mut self, args: &[String]) -> Result<Outcome> {
        if args.is_empty() {
            for (name, value) in &self.aliases {
                self.output
                    .push_str(&alloc::format!("alias {name}='{value}'\n"));
            }
            return Ok(Outcome::Status(0));
        }
        for arg in args {
            if let Some(eq) = arg.find('=') {
                let (name, rest) = arg.split_at(eq);
                if !is_identifier(name) {
                    return Err(KashError::Runtime(alloc::format!(
                        "alias: `{name}` is not a valid identifier"
                    )));
                }
                self.aliases
                    .insert(name.to_string(), rest[1..].to_string());
            } else {
                match self.aliases.get(arg) {
                    Some(v) => self
                        .output
                        .push_str(&alloc::format!("alias {arg}='{v}'\n")),
                    None => {
                        return Err(KashError::Runtime(alloc::format!(
                            "alias: {arg}: not found"
                        )));
                    }
                }
            }
        }
        Ok(Outcome::Status(0))
    }

    /// `unalias [-a] NAME ...` builtin. `-a` removes everything.
    fn builtin_unalias(&mut self, args: &[String]) -> Result<Outcome> {
        if args.first().map(|s| s.as_str()) == Some("-a") {
            self.aliases.clear();
            return Ok(Outcome::Status(0));
        }
        for name in args {
            self.aliases.remove(name);
        }
        Ok(Outcome::Status(0))
    }

    /// `trap [ACTION] SIGNAL …` builtin.
    ///
    /// Argument forms (POSIX):
    ///
    /// - `trap` — list the currently-registered traps.
    /// - `trap ACTION SIGNAL …` — install `ACTION` for every signal.
    /// - `trap '' SIGNAL …` — install an empty action (no-op handler).
    /// - `trap - SIGNAL …` — reset / un-register.
    /// - `trap NUMBER` — treat a single numeric arg as a signal name
    ///   to reset (POSIX old form).
    ///
    /// Signal names are normalised to upper-case sans `SIG` prefix
    /// (`INT`, `TERM`, …). The pseudo-signals `EXIT` and `ERR` fire
    /// at the appropriate points in evaluation; real OS signals are
    /// recorded into the table but not yet delivered.
    fn builtin_trap(&mut self, args: &[String]) -> Result<Outcome> {
        if args.is_empty() {
            // `trap` with no args: emit the table in stable order.
            for (sig, cmd) in &self.traps {
                self.output.push_str(&alloc::format!(
                    "trap -- '{cmd}' {sig}\n"
                ));
            }
            return Ok(Outcome::Status(0));
        }
        // `trap NUMBER` — reset the named signal (POSIX old form).
        if args.len() == 1 && args[0].chars().all(|c| c.is_ascii_digit()) {
            let sig = normalize_signal(&args[0]);
            self.traps.remove(&sig);
            return Ok(Outcome::Status(0));
        }
        if args.len() < 2 {
            return Err(KashError::Runtime(
                "trap: needs an action and at least one signal".into(),
            ));
        }
        let action = &args[0];
        for sig_raw in &args[1..] {
            let sig = normalize_signal(sig_raw);
            if action == "-" {
                self.traps.remove(&sig);
            } else {
                self.traps.insert(sig, action.clone());
            }
        }
        Ok(Outcome::Status(0))
    }

    fn builtin_readonly(&mut self, args: &[String]) -> Result<Outcome> {
        for arg in args {
            if let Some(eq) = arg.find('=') {
                let (name, rest) = arg.split_at(eq);
                if !is_identifier(name) {
                    return Err(KashError::Runtime(format!(
                        "readonly: `{name}` is not a valid identifier"
                    )));
                }
                let value = rest[1..].to_string();
                self.scope.mark_readonly(name, Some(Value::Scalar(value)))?;
            } else {
                if !is_identifier(arg) {
                    return Err(KashError::Runtime(format!(
                        "readonly: `{arg}` is not a valid identifier"
                    )));
                }
                self.scope.mark_readonly(arg, None)?;
            }
        }
        Ok(Outcome::Status(0))
    }

    fn builtin_shift(&mut self, args: &[String]) -> Result<Outcome> {
        let n: usize = if let Some(a) = args.first() {
            a.parse().map_err(|_| {
                KashError::Runtime(format!("shift: numeric argument required, got `{a}`"))
            })?
        } else {
            1
        };
        if n > self.positionals.len() {
            return Ok(Outcome::Status(1));
        }
        self.positionals.drain(..n);
        Ok(Outcome::Status(0))
    }

    // ---------- function call ----------

    fn call_function(&mut self, argv: &[String]) -> Result<Outcome> {
        let entry = self
            .functions
            .get(&argv[0])
            .cloned()
            .expect("function existed at dispatch time");
        // Swap in the function's positional arguments.
        let saved = core::mem::replace(&mut self.positionals, argv[1..].to_vec());
        self.positionals_stack.push(saved);
        // Push a function frame. `static_scope = true` for ksh93
        // `function NAME`-form functions: assignments inside that
        // form's body default to local, matching the locked
        // `project_shell_function_scope.md` rule.
        let static_scope = matches!(entry.scope, FunctionScope::Static);
        self.scope.push_function_frame(static_scope);
        // Capture list enforcement (read-only by-ref binding for the
        // names in `entry.captures`) lights up when the typeset
        // attribute machinery lands — until then the parser records
        // the list, the evaluator ignores it.
        let _ = &entry.captures;
        let result = self.eval_compound(&entry.body);
        self.scope.pop();
        let restored = self.positionals_stack.pop().expect("we just pushed");
        self.positionals = restored;
        result
    }

    // ---------- compound commands ----------

    fn eval_compound(&mut self, c: &CompoundCommand) -> Result<Outcome> {
        if !c.redirects.is_empty() {
            return Err(KashError::Runtime(
                "redirections on compound commands are not yet supported".into(),
            ));
        }
        match &c.kind {
            CompoundKind::BraceGroup { body } => self.eval_statements(body),
            CompoundKind::Subshell { body } => {
                // No fork on the alloc-only build, so simulate
                // process-style isolation by snapshotting the whole
                // environment (scope, positionals, function table)
                // and restoring it on exit. A frame push alone isn't
                // enough — dynamic-scope assignments would still
                // propagate into the caller's frames otherwise.
                let saved_scope = self.scope.clone();
                let saved_positionals = self.positionals.clone();
                let saved_functions = self.functions.clone();
                let result = self.eval_statements(body);
                self.scope = saved_scope;
                self.positionals = saved_positionals;
                self.functions = saved_functions;
                result
            }
            CompoundKind::If {
                branches,
                else_body,
            } => self.eval_if(branches, else_body.as_deref()),
            CompoundKind::While { cond, body } => self.eval_while(cond, body, false),
            CompoundKind::Until { cond, body } => self.eval_while(cond, body, true),
            CompoundKind::For { name, words, body } => self.eval_for(name, words.as_deref(), body),
            CompoundKind::Case { subject, items } => self.eval_case(subject, items),
            CompoundKind::DoubleBracket { tokens } => {
                let mut args: Vec<String> = Vec::with_capacity(tokens.len());
                for t in tokens {
                    args.push(self.expand_word(t)?);
                }
                let ok = eval_double_bracket(&args)?;
                Ok(Outcome::Status(if ok { 0 } else { 1 }))
            }
            CompoundKind::FunctionDef {
                name,
                scope,
                captures,
                body,
            } => {
                self.functions.insert(
                    name.clone(),
                    FunctionEntry {
                        scope: *scope,
                        captures: captures.clone(),
                        body: body.clone(),
                    },
                );
                Ok(Outcome::Status(0))
            }
        }
    }

    fn eval_if(
        &mut self,
        branches: &[IfBranch],
        else_body: Option<&[Statement]>,
    ) -> Result<Outcome> {
        for branch in branches {
            let cond_outcome =
                self.with_errexit_inactive(|s| s.eval_statements(&branch.cond))?;
            if cond_outcome.is_exit_request() {
                return Ok(cond_outcome);
            }
            if cond_outcome.success() {
                return self.eval_statements(&branch.body);
            }
        }
        if let Some(body) = else_body {
            return self.eval_statements(body);
        }
        Ok(Outcome::Status(0))
    }

    fn eval_while(
        &mut self,
        cond: &[Statement],
        body: &[Statement],
        invert: bool,
    ) -> Result<Outcome> {
        let mut outcome = Outcome::Status(0);
        loop {
            let cond_outcome = self.with_errexit_inactive(|s| s.eval_statements(cond))?;
            if cond_outcome.is_exit_request() {
                return Ok(cond_outcome);
            }
            let should_run = if invert {
                !cond_outcome.success()
            } else {
                cond_outcome.success()
            };
            if !should_run {
                return Ok(outcome);
            }
            outcome = self.eval_statements(body)?;
            if outcome.is_exit_request() {
                return Ok(outcome);
            }
        }
    }

    fn eval_for(
        &mut self,
        name: &str,
        words: Option<&[Word]>,
        body: &[Statement],
    ) -> Result<Outcome> {
        let items: Vec<String> = match words {
            Some(ws) => {
                // `for x in $LIST` should expand `$LIST` with field
                // splitting — that's what gives `for w in $ws` its
                // word-by-word iteration semantics.
                let mut out = Vec::with_capacity(ws.len());
                for w in ws {
                    out.extend(self.expand_word_to_fields(w)?);
                }
                out
            }
            // Omitted `in` clause iterates positional parameters.
            None => self.positionals.clone(),
        };
        let mut outcome = Outcome::Status(0);
        for item in items {
            self.scope.assign(name, Value::Scalar(item))?;
            outcome = self.eval_statements(body)?;
            if outcome.is_exit_request() {
                return Ok(outcome);
            }
        }
        Ok(outcome)
    }

    fn eval_case(&mut self, subject: &Word, items: &[CaseItem]) -> Result<Outcome> {
        let subject_str = self.expand_word(subject)?;
        let mut outcome = Outcome::Status(0);
        let mut force_run_next = false;
        for item in items {
            let did_match = if force_run_next {
                true
            } else {
                let mut hit = false;
                for p in &item.patterns {
                    let pat = self.expand_word(p)?;
                    if glob_match(&pat, &subject_str) {
                        hit = true;
                        break;
                    }
                }
                hit
            };
            if !did_match {
                continue;
            }
            outcome = self.eval_statements(&item.body)?;
            if outcome.is_exit_request() {
                return Ok(outcome);
            }
            force_run_next = false;
            match item.fallthrough {
                CaseFallthrough::Stop => return Ok(outcome),
                CaseFallthrough::Continue => {
                    // `;&` — fall through and run the very next arm
                    // unconditionally, then resume normal matching.
                    force_run_next = true;
                }
                CaseFallthrough::MatchNext => {
                    // `;;&` — per the locked design (ast.rs), stop on
                    // a successful body, otherwise keep matching.
                    if outcome.success() {
                        return Ok(outcome);
                    }
                }
            }
        }
        Ok(outcome)
    }

    // ---------- word / parameter expansion ----------

    /// Expand a [`Word`] to a *single* string, gluing every segment's
    /// expansion together with no field splitting. Used wherever the
    /// shell wants exactly one value: assignment right-hand sides,
    /// `case` subjects, redirect targets, modifier-word bodies.
    fn expand_word(&mut self, w: &Word) -> Result<String> {
        let mut out = String::new();
        for seg in &w.segments {
            match seg {
                WordSegment::Bare(s) | WordSegment::DoubleQuoted(s) => {
                    self.expand_dollar(s, &mut out)?;
                }
                WordSegment::SingleQuoted(s) | WordSegment::AnsiC(s) => {
                    // SingleQuoted: verbatim. AnsiC: the escape pass
                    // (`\n`, `\xHH`, …) lands with the full expansion
                    // story; for the skeleton we treat the body as
                    // verbatim. That's wrong but it's also harmless
                    // for strings without escapes.
                    out.push_str(s);
                }
            }
        }
        Ok(out)
    }

    /// Expand a [`Word`] to *zero or more* fields, honouring POSIX
    /// field splitting on `IFS`. Used when building argv for a simple
    /// command, the iteration set of a `for` loop, etc.
    ///
    /// Splitting only applies to the *value* of an unquoted parameter
    /// expansion — literal bare-segment bytes go into the current
    /// field as-is, and any segment that is single-quoted, AnsiC, or
    /// double-quoted is non-splitting (the double-quoted body still
    /// gets `$VAR` substituted, just without splitting). A word with
    /// at least one quoted segment always produces at least one
    /// field, even if everything inside expanded to empty.
    fn expand_word_to_fields(&mut self, w: &Word) -> Result<Vec<String>> {
        let ifs = self.lookup_ifs();
        let mut fields: Vec<String> = alloc::vec![String::new()];
        for seg in &w.segments {
            match seg {
                WordSegment::Bare(s) => {
                    self.expand_into_fields(s, &mut fields, Some(&ifs))?;
                }
                WordSegment::DoubleQuoted(s) => {
                    self.expand_into_fields(s, &mut fields, None)?;
                }
                WordSegment::SingleQuoted(s) | WordSegment::AnsiC(s) => {
                    fields.last_mut().expect("fields invariant").push_str(s);
                }
            }
        }
        if !word_has_quoted_segment(w)
            && fields.len() == 1
            && fields[0].is_empty()
        {
            return Ok(Vec::new());
        }
        Ok(fields)
    }

    /// Walk `text` (a single segment's payload) and append it to
    /// `fields`. `split_ifs` is `Some(IFS)` to make `$expansion`
    /// results IFS-splittable; `None` keeps everything in the current
    /// field (used for double-quoted segments).
    fn expand_into_fields(
        &mut self,
        text: &str,
        fields: &mut Vec<String>,
        split_ifs: Option<&str>,
    ) -> Result<()> {
        // A preceding `"$@"` with empty positionals may have popped
        // the in-progress field — re-seed it so this segment has
        // somewhere to write.
        if fields.is_empty() {
            fields.push(String::new());
        }
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '`' {
                let body = read_backtick_body(&mut chars)?;
                let value = self.run_command_substitution(&body)?;
                match split_ifs {
                    Some(ifs) => append_split(&value, ifs, fields),
                    None => fields
                        .last_mut()
                        .expect("fields invariant")
                        .push_str(&value),
                }
                continue;
            }
            if c != '$' {
                fields.last_mut().expect("fields invariant").push(c);
                continue;
            }
            // `$` followed by an expansion form. Read the expanded
            // value into `value`, then append it with or without
            // splitting depending on `split_ifs`.
            let Some(&next) = chars.peek() else {
                fields.last_mut().expect("fields invariant").push('$');
                continue;
            };
            // `$@` / `$*` are special: they expand to multiple fields
            // in the splittable path and can't be flattened to a
            // single `value` string. Handle them up front and `continue`
            // past the per-value aggregator below.
            if next == '@' || next == '*' {
                chars.next();
                self.expand_at_or_star_into_fields(next == '@', split_ifs, fields);
                continue;
            }
            let value = if next == '(' {
                chars.next();
                if chars.peek() == Some(&'(') {
                    chars.next();
                    let body = read_arith_body(&mut chars)?;
                    let v = self.eval_arith(&body)?;
                    alloc::format!("{v}")
                } else {
                    let body = read_paren_body(&mut chars)?;
                    self.run_command_substitution(&body)?
                }
            } else if next == '{' {
                chars.next();
                let mut depth = 1usize;
                let mut body = String::new();
                for c in chars.by_ref() {
                    if c == '{' {
                        depth += 1;
                        body.push(c);
                    } else if c == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                        body.push(c);
                    } else {
                        body.push(c);
                    }
                }
                if depth != 0 {
                    return Err(KashError::Parse(
                        "unterminated `${...}` parameter expansion".into(),
                    ));
                }
                self.expand_braced(&body)?
            } else if next == '?' {
                chars.next();
                self.last_status.to_string()
            } else if next == '#' {
                chars.next();
                self.positionals.len().to_string()
            } else if next == '$' {
                chars.next();
                "0".into()
            } else if next.is_ascii_digit() {
                chars.next();
                let n = next.to_digit(10).expect("ascii digit") as usize;
                if n == 0 {
                    String::new()
                } else {
                    self.positionals.get(n - 1).cloned().unwrap_or_default()
                }
            } else if is_name_start(next) {
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    if is_name_continue(c) {
                        name.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                self.lookup_param(&name)?
            } else {
                // Bare `$` — emit verbatim.
                fields.last_mut().expect("fields invariant").push('$');
                continue;
            };
            match split_ifs {
                Some(ifs) => append_split(&value, ifs, fields),
                None => fields.last_mut().expect("fields invariant").push_str(&value),
            }
        }
        Ok(())
    }

    /// Expand `$@` / `$*` straight into a `fields` accumulator. The
    /// quoted-vs-unquoted distinction reaches us through
    /// `split_ifs`: `Some` means we're inside a `Bare` segment (or
    /// equivalent), `None` means we're inside a `DoubleQuoted` one.
    ///
    /// Rules implemented (POSIX):
    ///
    /// - `$@`, *quoted* (`"$@"`): each positional is its own
    ///   field. The first positional is folded into the field
    ///   already in progress; the rest start fresh fields. No
    ///   internal splitting.
    /// - `$@`, *unquoted*: same multi-field shape, plus each value
    ///   is subjected to IFS field splitting.
    /// - `$*`, *quoted* (`"$*"`): all positionals are joined with
    ///   the first character of `IFS` (space when `IFS` is unset)
    ///   into a single field.
    /// - `$*`, *unquoted*: the joined string then gets the standard
    ///   IFS split treatment.
    ///
    /// `expand_dollar` (the single-string path) collapses both forms
    /// to the joined-by-first-IFS-char string; the multi-field
    /// semantics only fire here.
    fn expand_at_or_star_into_fields(
        &self,
        is_at: bool,
        split_ifs: Option<&str>,
        fields: &mut Vec<String>,
    ) {
        if is_at {
            // $@ — one field per positional (in the quoted form);
            // splittable in the unquoted form.
            let mut iter = self.positionals.iter();
            let Some(first) = iter.next() else {
                // POSIX: empty "$@" contributes no field at all.
                // Drop the in-progress empty field so the surrounding
                // word ends up with one fewer slot. If the slot
                // already has content from earlier text the pop is
                // skipped — leave that field as-is.
                if fields.last().map(|s| s.is_empty()).unwrap_or(false)
                    && fields.len() == 1
                {
                    fields.pop();
                }
                return;
            };
            match split_ifs {
                Some(ifs) => append_split(first, ifs, fields),
                None => fields
                    .last_mut()
                    .expect("fields invariant")
                    .push_str(first),
            }
            for p in iter {
                fields.push(String::new());
                match split_ifs {
                    Some(ifs) => append_split(p, ifs, fields),
                    None => fields
                        .last_mut()
                        .expect("fields invariant")
                        .push_str(p),
                }
            }
        } else {
            // $* — join with first char of IFS.
            let sep = first_ifs_char(&self.lookup_ifs());
            let joined = self.positionals.join(&sep);
            match split_ifs {
                Some(ifs) => append_split(&joined, ifs, fields),
                None => fields
                    .last_mut()
                    .expect("fields invariant")
                    .push_str(&joined),
            }
        }
    }

    /// Current value of `IFS`. Falls back to the POSIX default
    /// `" \t\n"` if `IFS` is unset.
    fn lookup_ifs(&self) -> String {
        match self.scope.get("IFS") {
            Some(v) => v.to_scalar_string(),
            None => " \t\n".into(),
        }
    }

    /// Evaluate a POSIX integer arithmetic expression. `$VAR`-style
    /// references inside the body are expanded *before* the parser
    /// runs (so e.g. `$((`X` + `$X`))` both work); bare names are
    /// looked up directly during parsing.
    fn eval_arith(&mut self, src: &str) -> Result<i64> {
        let mut expanded = String::new();
        self.expand_dollar(src, &mut expanded)?;
        let mut parser = ArithParser {
            src: &expanded,
            pos: 0,
            ev: self,
        };
        let v = parser.parse_expr()?;
        parser.expect_end()?;
        Ok(v)
    }

    /// Parse `src` as kash source, run it in a fresh subshell-style
    /// context (environment snapshot + isolated output buffer), then
    /// return the captured stdout with trailing newlines stripped.
    /// POSIX defines command substitution as a subshell, so this
    /// snapshots the scope / positionals / function table just like
    /// `( ... )` does.
    fn run_command_substitution(&mut self, src: &str) -> Result<String> {
        let prog = crate::parser::parse(src)?;
        let saved_scope = self.scope.clone();
        let saved_positionals = self.positionals.clone();
        let saved_functions = self.functions.clone();
        let saved_output = core::mem::take(&mut self.output);
        let result = self.eval_program(&prog);
        let captured = core::mem::replace(&mut self.output, saved_output);
        self.scope = saved_scope;
        self.positionals = saved_positionals;
        self.functions = saved_functions;
        result?;
        let mut s = captured;
        while s.ends_with('\n') {
            s.pop();
        }
        Ok(s)
    }

    /// Walk `text` and append it to `out`, substituting `$NAME`,
    /// `${…}`, and the specials (`$?`, `$#`, `$0`-`$9`, `$$`) along
    /// the way. Used for `Bare` and `DoubleQuoted` segments.
    fn expand_dollar(&mut self, text: &str, out: &mut String) -> Result<()> {
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '`' {
                let body = read_backtick_body(&mut chars)?;
                let value = self.run_command_substitution(&body)?;
                out.push_str(&value);
                continue;
            }
            if c != '$' {
                out.push(c);
                continue;
            }
            // Peek the byte right after `$`.
            let Some(&next) = chars.peek() else {
                out.push('$');
                continue;
            };
            if next == '(' {
                chars.next();
                if chars.peek() == Some(&'(') {
                    chars.next();
                    let body = read_arith_body(&mut chars)?;
                    let v = self.eval_arith(&body)?;
                    out.push_str(&alloc::format!("{v}"));
                } else {
                    let body = read_paren_body(&mut chars)?;
                    let value = self.run_command_substitution(&body)?;
                    out.push_str(&value);
                }
            } else if next == '{' {
                chars.next(); // consume `{`
                let mut depth = 1usize;
                let mut body = String::new();
                for c in chars.by_ref() {
                    if c == '{' {
                        depth += 1;
                        body.push(c);
                    } else if c == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                        body.push(c);
                    } else {
                        body.push(c);
                    }
                }
                if depth != 0 {
                    return Err(KashError::Parse(
                        "unterminated `${...}` parameter expansion".into(),
                    ));
                }
                let val = self.expand_braced(&body)?;
                out.push_str(&val);
            } else if next == '?' {
                chars.next();
                out.push_str(&self.last_status.to_string());
            } else if next == '#' {
                chars.next();
                out.push_str(&self.positionals.len().to_string());
            } else if next == '$' {
                chars.next();
                // Process ID — stable PID source needs `std::process::id`.
                // The skeleton emits a placeholder.
                out.push('0');
            } else if next == '@' || next == '*' {
                // In a single-string context (no field splitting),
                // both `$@` and `$*` collapse to the IFS-joined
                // positionals. Field-splitting contexts override.
                chars.next();
                let sep = first_ifs_char(&self.lookup_ifs());
                out.push_str(&self.positionals.join(&sep));
            } else if next.is_ascii_digit() {
                chars.next();
                let n = next.to_digit(10).expect("ascii digit") as usize;
                if n == 0 {
                    // `$0` — script / shell name. Skeleton: empty.
                } else if let Some(arg) = self.positionals.get(n - 1) {
                    out.push_str(arg);
                }
            } else if is_name_start(next) {
                // `$NAME` — read a bare identifier.
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    if is_name_continue(c) {
                        name.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let v = self.lookup_param(&name)?;
                out.push_str(&v);
            } else {
                // Standalone `$` followed by something else — emit
                // the dollar verbatim.
                out.push('$');
            }
        }
        Ok(())
    }

    /// Handle a `${...}` body. Currently supports:
    ///
    /// - `${NAME}` — plain lookup
    /// - `${#NAME}` — string length (character count of scalar form)
    /// - `${NAME-WORD}` / `${NAME:-WORD}` — default
    /// - `${NAME=WORD}` / `${NAME:=WORD}` — assign default
    /// - `${NAME?WORD}` / `${NAME:?WORD}` — error if unset/null
    /// - `${NAME+WORD}` / `${NAME:+WORD}` — alternate
    fn expand_braced(&mut self, body: &str) -> Result<String> {
        // `${#NAME}` — length form. Has to be checked before the
        // operator-split because `#` here is *not* a modifier-op.
        if let Some(rest) = body.strip_prefix('#') {
            if rest.is_empty() {
                // `${#}` — argc.
                return Ok(self.positionals.len().to_string());
            }
            if !is_valid_param_name(rest) {
                return Err(KashError::Parse(format!(
                    "invalid `${{#{rest}}}` length expansion"
                )));
            }
            let len = match self.scope.get(rest) {
                Some(v) => v.to_scalar_string().chars().count(),
                None => 0,
            };
            return Ok(len.to_string());
        }

        // Find the parameter name (run of identifier bytes). The first
        // non-name byte after the name is either the end of the
        // expansion or the start of an operator suffix.
        let bytes = body.as_bytes();
        let mut idx = 0;
        while idx < bytes.len() {
            let b = bytes[idx];
            if idx == 0 {
                if !(b == b'_' || b.is_ascii_alphabetic() || b.is_ascii_digit() || b == b'?' || b == b'#') {
                    break;
                }
            } else if !(b == b'_' || b.is_ascii_alphanumeric()) {
                break;
            }
            idx += 1;
            // `$?` / `$#` are special-cased above; here we only allow
            // single-byte digit / question / hash names.
            if idx == 1 && (bytes[0].is_ascii_digit() || bytes[0] == b'?' || bytes[0] == b'#') {
                break;
            }
        }
        if idx == 0 {
            return Err(KashError::Parse(format!(
                "empty parameter name in `${{{body}}}`"
            )));
        }
        let name = &body[..idx];
        let rest = &body[idx..];

        // Bare `${NAME}` with no operator — honours nounset.
        if rest.is_empty() {
            return self.lookup_param(name);
        }

        // Parse the modifier prefix: optional `:`, then one of `-=?+`.
        let (test_null, op_char, word) = if let Some(after_colon) = rest.strip_prefix(':') {
            let mut it = after_colon.chars();
            let op = it
                .next()
                .ok_or_else(|| KashError::Parse(format!("dangling `:` in `${{{body}}}`")))?;
            let rest = &after_colon[op.len_utf8()..];
            (true, op, rest)
        } else {
            let mut it = rest.chars();
            let op = it.next().expect("rest is non-empty");
            let rest = &rest[op.len_utf8()..];
            (false, op, rest)
        };

        // Modifier forms handle "unset" themselves, so look up the
        // raw value without firing `nounset` here.
        let current_present = self.scope.get(name).is_some();
        let current_value = self.lookup_param_raw(name);
        let trigger = if test_null {
            !current_present || current_value.is_empty()
        } else {
            !current_present
        };

        match op_char {
            '-' => {
                if trigger {
                    self.expand_inline(word)
                } else {
                    Ok(current_value)
                }
            }
            '=' => {
                if trigger {
                    let v = self.expand_inline(word)?;
                    self.scope.assign(name, Value::Scalar(v.clone()))?;
                    Ok(v)
                } else {
                    Ok(current_value)
                }
            }
            '?' => {
                if trigger {
                    let msg = self.expand_inline(word)?;
                    let msg = if msg.is_empty() {
                        format!("{name}: parameter null or not set")
                    } else {
                        format!("{name}: {msg}")
                    };
                    Err(KashError::Runtime(msg))
                } else {
                    Ok(current_value)
                }
            }
            '+' => {
                if trigger {
                    Ok(String::new())
                } else {
                    self.expand_inline(word)
                }
            }
            other => Err(KashError::Parse(format!(
                "unsupported modifier `{other}` in `${{{body}}}`"
            ))),
        }
    }

    /// Like [`lookup_param`](Self::lookup_param) but never triggers
    /// `nounset`. Used by modifier forms (`${VAR:-…}`, `${VAR:+…}`,
    /// …) that explicitly handle the unset case themselves.
    fn lookup_param_raw(&self, name: &str) -> String {
        if name == "?" {
            return self.last_status.to_string();
        }
        if name == "#" {
            return self.positionals.len().to_string();
        }
        if name.len() == 1 {
            if let Some(d) = name.chars().next().and_then(|c| c.to_digit(10)) {
                let n = d as usize;
                if n == 0 {
                    return String::new();
                }
                return self
                    .positionals
                    .get(n - 1)
                    .cloned()
                    .unwrap_or_default();
            }
        }
        self.scope
            .get(name)
            .map(|v| v.to_scalar_string())
            .unwrap_or_default()
    }

    /// Look up `name` and return its scalar form, or empty for unset.
    /// Honours `nounset`: a plain `$NAME` / `${NAME}` lookup against
    /// an unset name raises [`KashError::Runtime`] when the option is
    /// on. Specials (`?`, `#`, `$`, `!`) and positional `$0`-`$9` are
    /// always considered set.
    fn lookup_param(&self, name: &str) -> Result<String> {
        // Specials are always present.
        if name == "?" {
            return Ok(self.last_status.to_string());
        }
        if name == "#" {
            return Ok(self.positionals.len().to_string());
        }
        if name.len() == 1 {
            if let Some(d) = name.chars().next().and_then(|c| c.to_digit(10)) {
                let n = d as usize;
                if n == 0 {
                    return Ok(String::new());
                }
                return Ok(self
                    .positionals
                    .get(n - 1)
                    .cloned()
                    .unwrap_or_default());
            }
        }
        match self.scope.get(name) {
            Some(v) => Ok(v.to_scalar_string()),
            None => {
                if self.options.nounset {
                    Err(KashError::Runtime(alloc::format!(
                        "{name}: parameter not set"
                    )))
                } else {
                    Ok(String::new())
                }
            }
        }
    }

    /// Expand `text` (a raw modifier word) by treating it as a `Bare`
    /// segment — `$NAME` / `${...}` references work, quote markers do
    /// not (the modifier-word body is already past quote-stripping by
    /// the time it reaches us).
    fn expand_inline(&mut self, text: &str) -> Result<String> {
        let mut out = String::new();
        self.expand_dollar(text, &mut out)?;
        Ok(out)
    }
}

impl Default for Evaluator {
    fn default() -> Self {
        Self::new()
    }
}

// ===== std-only: external process exec + multi-stage pipeline =====

ifstd!({
    impl Evaluator {
        /// Open the files named by a list of redirects without
        /// running any command. Used for the POSIX no-command form
        /// (`> file` truncates, `< file` opens-and-discards, …).
        fn open_redirect_side_effects(
            &mut self,
            redirects: &[crate::ast::Redirect],
        ) -> Result<Outcome> {
            use crate::ast::RedirectKind;
            for r in redirects {
                match r.kind {
                    RedirectKind::HereString
                    | RedirectKind::HereDoc { .. }
                    | RedirectKind::DupOutput
                    | RedirectKind::DupInput => {
                        // Inline-body and fd-dup redirects with no
                        // command name have nothing to feed to —
                        // POSIX says they simply succeed.
                    }
                    _ => {
                        let path = self.expand_word(&r.target)?;
                        self.open_redirect_file(r.kind, &path)?;
                    }
                }
            }
            Ok(Outcome::Status(0))
        }

        /// Open `path` according to `kind`. Centralised so the simple-
        /// command path and the no-command-side-effects path agree on
        /// flags and error reporting.
        fn open_redirect_file(
            &self,
            kind: crate::ast::RedirectKind,
            path: &str,
        ) -> Result<std::fs::File> {
            use crate::ast::RedirectKind;
            use std::fs::OpenOptions;
            let result = match kind {
                RedirectKind::Output | RedirectKind::OutputBoth => OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .create(true)
                    .open(path),
                RedirectKind::Append | RedirectKind::AppendBoth => OpenOptions::new()
                    .write(true)
                    .append(true)
                    .create(true)
                    .open(path),
                RedirectKind::Input => OpenOptions::new().read(true).open(path),
                RedirectKind::HereString
                | RedirectKind::HereDoc { .. }
                | RedirectKind::DupOutput
                | RedirectKind::DupInput => {
                    // Caller is expected to route inline-body and
                    // fd-dup redirects through their own paths, not
                    // the file path.
                    return Err(KashError::Runtime(
                        "internal: open_redirect_file called for a non-file redirect".into(),
                    ));
                }
            };
            result.map_err(|e| KashError::Runtime(alloc::format!("open `{path}`: {e}")))
        }

        /// Run a simple command with one or more redirects applied.
        ///
        /// Builtins/functions get their output captured: the routine
        /// remembers the current length of `self.output`, runs the
        /// builtin / function, then writes the new tail of the buffer
        /// to whichever file the redirects selected (truncating the
        /// buffer afterwards so it doesn't double-emit to the host).
        ///
        /// External commands receive the opened files / inline-body
        /// pipes as their `Stdio`, so the kernel does the work
        /// directly.
        fn eval_with_redirects(
            &mut self,
            cmd: &SimpleCommand,
            argv: &[String],
        ) -> Result<Outcome> {
            use crate::ast::RedirectKind;
            use std::io::{Read, Write};
            use std::process::{Command, Stdio};
            // Resolved per-fd routing. Built up by walking the
            // redirect list in source order: each entry either opens
            // a file, redirects an inline payload, or duplicates one
            // of the standard streams onto another.
            let mut stdout_file: Option<std::fs::File> = None;
            let mut stderr_file: Option<std::fs::File> = None;
            let mut in_file: Option<std::fs::File> = None;
            let mut in_inline: Option<alloc::vec::Vec<u8>> = None;
            // Cross-stream dup flags. `stderr_follows_stdout` covers
            // `2>&1` *and* the `&>` / `&>>` forms; `stdout_follows_stderr`
            // covers `1>&2`. The order of writes through the redirect
            // list resets these as needed.
            let mut stderr_follows_stdout: bool = false;
            let mut stdout_follows_stderr: bool = false;
            for r in &cmd.redirects {
                let fd_hint = r.fd.unwrap_or_else(|| default_fd_for(r.kind));
                match r.kind {
                    RedirectKind::Input => {
                        let path = self.expand_word(&r.target)?;
                        let f = self.open_redirect_file(r.kind, &path)?;
                        if fd_hint != 0 {
                            return Err(KashError::Runtime(alloc::format!(
                                "redirecting fd {fd_hint} for input isn't supported yet"
                            )));
                        }
                        in_file = Some(f);
                        in_inline = None;
                    }
                    RedirectKind::Output | RedirectKind::Append => {
                        let path = self.expand_word(&r.target)?;
                        let f = self.open_redirect_file(r.kind, &path)?;
                        match fd_hint {
                            1 => {
                                stdout_file = Some(f);
                                stdout_follows_stderr = false;
                            }
                            2 => {
                                stderr_file = Some(f);
                                stderr_follows_stdout = false;
                            }
                            other => {
                                return Err(KashError::Runtime(alloc::format!(
                                    "redirecting fd {other} isn't supported yet"
                                )));
                            }
                        }
                    }
                    RedirectKind::OutputBoth | RedirectKind::AppendBoth => {
                        let path = self.expand_word(&r.target)?;
                        let f = self.open_redirect_file(r.kind, &path)?;
                        stdout_file = Some(f);
                        stderr_follows_stdout = true;
                        stdout_follows_stderr = false;
                    }
                    RedirectKind::DupOutput => {
                        let target = self.expand_word(&r.target)?;
                        if target == "-" {
                            // `[n]>&-` close — collapse the sink to
                            // /dev/null by clearing any file routing.
                            // Approximate: route to inherit, which is
                            // visually identical for most uses.
                            match fd_hint {
                                1 => {
                                    stdout_file = None;
                                    stdout_follows_stderr = false;
                                }
                                2 => {
                                    stderr_file = None;
                                    stderr_follows_stdout = false;
                                }
                                other => {
                                    return Err(KashError::Runtime(alloc::format!(
                                        "closing fd {other} isn't supported yet"
                                    )));
                                }
                            }
                            continue;
                        }
                        let src_fd: i32 = target.parse().map_err(|_| {
                            KashError::Runtime(alloc::format!(
                                "`{target}` is not a valid file descriptor"
                            ))
                        })?;
                        match (fd_hint, src_fd) {
                            (2, 1) => {
                                stderr_follows_stdout = true;
                                stderr_file = None;
                            }
                            (1, 2) => {
                                stdout_follows_stderr = true;
                                stdout_file = None;
                            }
                            (a, b) if a == b => { /* self-dup is a no-op */ }
                            _ => {
                                return Err(KashError::Runtime(alloc::format!(
                                    "fd dup {fd_hint}>&{src_fd} isn't supported yet"
                                )));
                            }
                        }
                    }
                    RedirectKind::DupInput => {
                        return Err(KashError::Runtime(
                            "input-side fd duplication isn't supported yet".into(),
                        ));
                    }
                    RedirectKind::HereString => {
                        let text = self.expand_word(&r.target)?;
                        let mut bytes = text.into_bytes();
                        bytes.push(b'\n');
                        in_file = None;
                        in_inline = Some(bytes);
                    }
                    RedirectKind::HereDoc { strip_tabs: _ } => {
                        let text = self.expand_word(&r.target)?;
                        let bytes = text.into_bytes();
                        in_file = None;
                        in_inline = Some(bytes);
                    }
                }
            }
            // Compatibility shim with the older two-flag layout the
            // rest of this function used: `out_file` / `both` from the
            // pre-fd-routing world.
            let out_file = stdout_file;
            let both = stderr_follows_stdout;
            let stderr_routed_file = stderr_file;

            let name = argv[0].as_str();
            let is_function = self.functions.contains_key(name);
            let is_builtin = is_builtin_name(name);
            if is_function || is_builtin {
                // Capture the builtin's output buffer fragment.
                let old_len = self.output.len();
                let outcome = if is_function {
                    self.call_function(argv)?
                } else {
                    self.dispatch_builtin(argv)?
                };
                if let Some(mut f) = out_file {
                    let chunk = self.output[old_len..].as_bytes().to_vec();
                    f.write_all(&chunk).map_err(|e| {
                        KashError::Runtime(alloc::format!("write: {e}"))
                    })?;
                    self.output.truncate(old_len);
                }
                let _ = in_file;
                let _ = in_inline;
                let _ = both;
                Ok(outcome)
            } else {
                // External command — let the kernel handle stdin/out
                // straight from the opened file descriptors. Inline
                // stdin (`<<<` / `<<DELIM`) is fed via a piped stdin
                // we write to after spawn.
                let mut c = Command::new(&argv[0]);
                c.args(&argv[1..]);
                let needs_inline_write = in_inline.is_some();
                if let Some(f) = in_file {
                    c.stdin(Stdio::from(f));
                } else if needs_inline_write {
                    c.stdin(Stdio::piped());
                } else {
                    c.stdin(Stdio::inherit());
                }
                // Resolve stdout / stderr sinks from the fd-routing
                // state we built up above.
                let has_out = out_file.is_some();
                let stderr_file_clone = stderr_routed_file
                    .as_ref()
                    .map(|f| {
                        f.try_clone()
                            .map_err(|e| KashError::Runtime(alloc::format!("dup: {e}")))
                    })
                    .transpose()?;
                match out_file {
                    Some(f) => {
                        if both {
                            let f2 = f.try_clone().map_err(|e| {
                                KashError::Runtime(alloc::format!("dup: {e}"))
                            })?;
                            c.stdout(Stdio::from(f));
                            c.stderr(Stdio::from(f2));
                        } else {
                            c.stdout(Stdio::from(f));
                            // stderr follows whatever its own routing says.
                            if let Some(ef) = stderr_routed_file {
                                c.stderr(Stdio::from(ef));
                            } else {
                                c.stderr(Stdio::inherit());
                            }
                        }
                    }
                    None => {
                        if stdout_follows_stderr {
                            // `1>&2` with no stdout file routing.
                            // If stderr was sent to a file, send
                            // stdout to a clone of that handle;
                            // otherwise fall back to inheriting (real
                            // dup of the terminal — both end up at
                            // the same tty).
                            if let Some(ef) = stderr_file_clone {
                                c.stdout(Stdio::from(ef));
                            } else {
                                c.stdout(Stdio::inherit());
                            }
                            if let Some(ef) = stderr_routed_file {
                                c.stderr(Stdio::from(ef));
                            } else {
                                c.stderr(Stdio::inherit());
                            }
                        } else {
                            // No stdout file routing — capture into
                            // the evaluator's output buffer.
                            c.stdout(Stdio::piped());
                            if let Some(ef) = stderr_routed_file {
                                c.stderr(Stdio::from(ef));
                            } else {
                                c.stderr(Stdio::inherit());
                            }
                        }
                    }
                }
                let mut child = c.spawn().map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        KashError::NotFound(alloc::format!("command `{}`", argv[0]))
                    } else {
                        KashError::Runtime(alloc::format!("exec: {e}"))
                    }
                })?;
                if let Some(bytes) = in_inline {
                    if let Some(mut si) = child.stdin.take() {
                        si.write_all(&bytes).map_err(|e| {
                            KashError::Runtime(alloc::format!("write stdin: {e}"))
                        })?;
                        // Dropping `si` closes the pipe so the child
                        // sees EOF.
                    }
                }
                if !has_out {
                    if let Some(mut so) = child.stdout.take() {
                        let mut buf = alloc::vec::Vec::<u8>::new();
                        so.read_to_end(&mut buf).map_err(|e| {
                            KashError::Runtime(alloc::format!("read: {e}"))
                        })?;
                        self.output.push_str(&String::from_utf8_lossy(&buf));
                    }
                }
                let status = child
                    .wait()
                    .map_err(|e| KashError::Runtime(alloc::format!("wait: {e}")))?;
                Ok(Outcome::Status(status.code().unwrap_or(128)))
            }
        }

        /// Dispatch a builtin given its already-expanded argv. Used
        /// from the redirect-handling path; mirrors the dispatch arm
        /// in `eval_simple`.
        fn dispatch_builtin(&mut self, argv: &[String]) -> Result<Outcome> {
            let name = argv[0].as_str();
            match name {
                ":" | "true" => Ok(Outcome::Status(0)),
                "false" => Ok(Outcome::Status(1)),
                "echo" => {
                    self.builtin_echo(&argv[1..]);
                    Ok(Outcome::Status(0))
                }
                "exit" => self.builtin_exit(&argv[1..]),
                "set" => self.builtin_set(&argv[1..]),
                "unset" => self.builtin_unset(&argv[1..]),
                "shift" => self.builtin_shift(&argv[1..]),
                "local" => self.builtin_local(&argv[1..]),
                "readonly" => self.builtin_readonly(&argv[1..]),
                "test" => builtin_test(false, &argv[1..]),
                "[" => builtin_test(true, &argv[1..]),
                "trap" => self.builtin_trap(&argv[1..]),
                "alias" => self.builtin_alias(&argv[1..]),
                "unalias" => self.builtin_unalias(&argv[1..]),
                other => Err(KashError::Runtime(alloc::format!(
                    "internal: dispatch_builtin called for `{other}`"
                ))),
            }
        }

        /// Spawn `argv[0]` as an external process. The child inherits
        /// our stdin/stderr; its stdout is captured and appended to
        /// the evaluator's output buffer.
        fn run_external_std(&mut self, argv: &[String]) -> Result<Outcome> {
            use std::io::Read;
            use std::process::{Command, Stdio};
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]);
            cmd.stdin(Stdio::inherit());
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::inherit());
            let mut child = cmd.spawn().map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    KashError::NotFound(alloc::format!("command `{}`", argv[0]))
                } else {
                    KashError::Runtime(alloc::format!("exec `{}`: {e}", argv[0]))
                }
            })?;
            let mut stdout_buf = alloc::vec::Vec::<u8>::new();
            if let Some(mut so) = child.stdout.take() {
                so.read_to_end(&mut stdout_buf)
                    .map_err(|e| KashError::Runtime(alloc::format!("read stdout: {e}")))?;
            }
            self.output
                .push_str(&String::from_utf8_lossy(&stdout_buf));
            let status = child
                .wait()
                .map_err(|e| KashError::Runtime(alloc::format!("wait: {e}")))?;
            Ok(Outcome::Status(status.code().unwrap_or(128)))
        }

        /// Spawn an N-stage pipeline of external commands. Stages
        /// that resolve to a builtin or function are rejected — the
        /// in-process / cross-process bridge for those lands later.
        fn run_pipeline_external(&mut self, pipe: &Pipeline) -> Result<Outcome> {
            use std::io::Read;
            use std::process::{Child, Command, Stdio};

            // Resolve every stage's argv up front. If any stage is a
            // builtin / function / compound, bail before we spawn
            // anything.
            let mut argvs: alloc::vec::Vec<alloc::vec::Vec<String>> =
                alloc::vec::Vec::with_capacity(pipe.stages.len());
            for stage in &pipe.stages {
                let simple = match stage {
                    crate::ast::Command::Simple(s) => s,
                    crate::ast::Command::Compound(_) => {
                        return Err(KashError::Runtime(
                            "compound commands in pipeline stages are not yet supported".into(),
                        ));
                    }
                };
                if !simple.redirects.is_empty() {
                    return Err(KashError::Runtime(
                        "redirections in pipeline stages are not yet supported".into(),
                    ));
                }
                if !simple.assignments.is_empty() {
                    return Err(KashError::Runtime(
                        "assignment prefixes in pipeline stages are not yet supported".into(),
                    ));
                }
                let mut argv = alloc::vec::Vec::with_capacity(simple.words.len());
                for w in &simple.words {
                    argv.extend(self.expand_word_to_fields(w)?);
                }
                if argv.is_empty() {
                    return Err(KashError::Runtime(
                        "pipeline stage expanded to nothing".into(),
                    ));
                }
                let name = argv[0].as_str();
                if self.functions.contains_key(name) || is_builtin_name(name) {
                    return Err(KashError::Runtime(alloc::format!(
                        "builtin or function `{name}` in a multi-stage pipeline is not yet supported"
                    )));
                }
                argvs.push(argv);
            }

            // Spawn each stage, wiring stdin to the previous stage's
            // stdout. The very last stage's stdout is captured into
            // the evaluator's output buffer.
            let n = argvs.len();
            let mut children: alloc::vec::Vec<Child> = alloc::vec::Vec::with_capacity(n);
            for (i, argv) in argvs.iter().enumerate() {
                let mut cmd = Command::new(&argv[0]);
                cmd.args(&argv[1..]);
                if i == 0 {
                    cmd.stdin(Stdio::inherit());
                } else {
                    let prev_stdout = children[i - 1]
                        .stdout
                        .take()
                        .expect("previous stage was spawned with piped stdout");
                    cmd.stdin(Stdio::from(prev_stdout));
                }
                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::inherit());
                let child = cmd.spawn().map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        KashError::NotFound(alloc::format!("command `{}`", argv[0]))
                    } else {
                        KashError::Runtime(alloc::format!("spawn `{}`: {e}", argv[0]))
                    }
                })?;
                children.push(child);
            }

            // Drain the last child's stdout before reaping anyone,
            // so a producer that writes more than a pipe-buffer's
            // worth doesn't deadlock waiting for us to read.
            let last = n - 1;
            let mut last_stdout = children[last]
                .stdout
                .take()
                .expect("last stage was spawned with piped stdout");
            let mut buf = alloc::vec::Vec::<u8>::new();
            last_stdout
                .read_to_end(&mut buf)
                .map_err(|e| KashError::Runtime(alloc::format!("read pipeline stdout: {e}")))?;
            self.output.push_str(&String::from_utf8_lossy(&buf));

            // Reap every stage. Pipeline exit status is the last
            // stage's (POSIX default). With `pipefail`, take the
            // *right-most* non-zero status instead, falling back to
            // zero only when every stage succeeded.
            let mut last_status = 0;
            let mut last_nonzero = 0;
            for (i, child) in children.iter_mut().enumerate() {
                let st = child
                    .wait()
                    .map_err(|e| KashError::Runtime(alloc::format!("wait: {e}")))?;
                let code = st.code().unwrap_or(128);
                if code != 0 {
                    last_nonzero = code;
                }
                if i == last {
                    last_status = code;
                }
            }
            let final_status = if self.options.pipefail {
                if last_nonzero != 0 {
                    last_nonzero
                } else {
                    0
                }
            } else {
                last_status
            };
            Ok(Outcome::Status(final_status))
        }
    }
});

/// Default file descriptor for a redirect whose operator doesn't
/// carry an explicit `N>` prefix. POSIX:
/// stdout for output-side ops, stdin for input-side ops, stderr for
/// `2>&1`-shaped dups that omit their fd (the dup target stays in
/// the right-hand-side word).
const fn default_fd_for(kind: crate::ast::RedirectKind) -> i32 {
    use crate::ast::RedirectKind::*;
    match kind {
        Input | HereString | HereDoc { .. } | DupInput => 0,
        Output | Append | OutputBoth | AppendBoth => 1,
        DupOutput => 1,
    }
}

fn is_builtin_name(name: &str) -> bool {
    matches!(
        name,
        ":" | "true"
            | "false"
            | "echo"
            | "exit"
            | "set"
            | "unset"
            | "shift"
            | "local"
            | "readonly"
            | "test"
            | "["
            | "trap"
            | "alias"
            | "unalias"
    )
}

/// Normalise a signal name to upper-case without a `SIG` prefix.
/// Numeric inputs pass through unchanged.
fn normalize_signal(s: &str) -> String {
    let upper = s.to_ascii_uppercase();
    if let Some(rest) = upper.strip_prefix("SIG") {
        rest.into()
    } else {
        upper
    }
}

/// POSIX `test` / `[` builtin. The `bracket` flag indicates the
/// invocation form (`[ ... ]` requires a closing `]`; `test ...` does
/// not). The supported operator surface in this commit:
///
/// - 0 args → false (exit 1).
/// - 1 arg → `STR` is non-empty? (POSIX 2.4).
/// - 2 args:
///     - `-z STR` / `-n STR`,
///     - `! STR` (negate the 1-arg form),
///     - `-e/-f/-d/-r/-w/-x FILE` (filesystem tests; std-only).
/// - 3 args:
///     - `STR1 = STR2` / `STR1 != STR2`,
///     - `N1 -eq/-ne/-lt/-le/-gt/-ge N2`,
///     - `! UNARY ARG` (negate a 2-arg test).
/// - 4 args: `! UNARY ARG OTHER` or `! BIN STR1 STR2`.
fn builtin_test(bracket: bool, raw: &[String]) -> Result<Outcome> {
    let mut args: Vec<&str> = raw.iter().map(|s| s.as_str()).collect();
    if bracket {
        match args.last() {
            Some(&"]") => {
                args.pop();
            }
            _ => {
                return Err(KashError::Runtime(
                    "[: missing `]`".into(),
                ));
            }
        }
    }
    let ok = test_eval(&args)?;
    Ok(Outcome::Status(if ok { 0 } else { 1 }))
}

fn test_eval(args: &[&str]) -> Result<bool> {
    match args.len() {
        0 => Ok(false),
        1 => Ok(!args[0].is_empty()),
        2 => {
            if args[0] == "!" {
                let inner = test_eval(&args[1..])?;
                return Ok(!inner);
            }
            test_unary(args[0], args[1])
        }
        3 => {
            if args[0] == "!" {
                let inner = test_eval(&args[1..])?;
                return Ok(!inner);
            }
            test_binary(args[0], args[1], args[2])
        }
        4 if args[0] == "!" => {
            let inner = test_eval(&args[1..])?;
            Ok(!inner)
        }
        _ => Err(KashError::Runtime(format!(
            "test: unexpected argument count ({})",
            args.len()
        ))),
    }
}

fn test_unary(op: &str, arg: &str) -> Result<bool> {
    Ok(match op {
        "-z" => arg.is_empty(),
        "-n" => !arg.is_empty(),
        #[cfg(feature = "std")]
        "-e" => std::path::Path::new(arg).exists(),
        #[cfg(feature = "std")]
        "-f" => std::fs::metadata(arg).map(|m| m.is_file()).unwrap_or(false),
        #[cfg(feature = "std")]
        "-d" => std::fs::metadata(arg).map(|m| m.is_dir()).unwrap_or(false),
        #[cfg(feature = "std")]
        "-r" => std::fs::metadata(arg).is_ok(),
        #[cfg(feature = "std")]
        "-w" => match std::fs::metadata(arg) {
            Ok(m) => !m.permissions().readonly(),
            Err(_) => false,
        },
        #[cfg(feature = "std")]
        "-x" => std::fs::metadata(arg).is_ok(),
        #[cfg(not(feature = "std"))]
        "-e" | "-f" | "-d" | "-r" | "-w" | "-x" => {
            return Err(KashError::Runtime(format!(
                "test: filesystem operator `{op}` requires the `std` feature"
            )));
        }
        other => {
            return Err(KashError::Runtime(format!(
                "test: unknown unary operator `{other}`"
            )));
        }
    })
}

fn test_binary(lhs: &str, op: &str, rhs: &str) -> Result<bool> {
    match op {
        "=" => Ok(lhs == rhs),
        "!=" => Ok(lhs != rhs),
        "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => {
            let a: i64 = lhs.parse().map_err(|_| {
                KashError::Runtime(format!("test: `{lhs}` is not an integer"))
            })?;
            let b: i64 = rhs.parse().map_err(|_| {
                KashError::Runtime(format!("test: `{rhs}` is not an integer"))
            })?;
            Ok(match op {
                "-eq" => a == b,
                "-ne" => a != b,
                "-lt" => a < b,
                "-le" => a <= b,
                "-gt" => a > b,
                "-ge" => a >= b,
                _ => unreachable!(),
            })
        }
        other => Err(KashError::Runtime(format!(
            "test: unknown binary operator `{other}`"
        ))),
    }
}

/// Evaluate the body of a `[[ … ]]` block. Supports everything
/// `test` does plus the bracket-only operators:
///
/// - `==` / `!=` — RHS is a glob pattern (matched via `glob_match`).
/// - `=~` — RHS is a POSIX ERE-subset regex (see [`regex_match`]).
/// - `<` / `>` — lexical comparison.
/// - `!`, `&&`, `||`, `( … )` — logical composition with
///   short-circuit, evaluated by a small recursive matcher.
fn eval_double_bracket(args: &[String]) -> Result<bool> {
    let strs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut p = BracketParser { args: &strs, pos: 0 };
    let v = p.parse_or()?;
    if p.pos != p.args.len() {
        return Err(KashError::Runtime(alloc::format!(
            "[[: unexpected token `{}`",
            p.args[p.pos]
        )));
    }
    Ok(v)
}

struct BracketParser<'a> {
    args: &'a [&'a str],
    pos: usize,
}

impl<'a> BracketParser<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.args.get(self.pos).copied()
    }

    fn eat(&mut self) -> Option<&'a str> {
        let v = self.peek();
        if v.is_some() {
            self.pos += 1;
        }
        v
    }

    fn parse_or(&mut self) -> Result<bool> {
        let mut lhs = self.parse_and()?;
        while self.peek() == Some("||") {
            self.eat();
            let rhs = self.parse_and()?;
            lhs = lhs || rhs;
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<bool> {
        let mut lhs = self.parse_unary()?;
        while self.peek() == Some("&&") {
            self.eat();
            let rhs = self.parse_unary()?;
            lhs = lhs && rhs;
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<bool> {
        if self.peek() == Some("!") {
            self.eat();
            let v = self.parse_unary()?;
            return Ok(!v);
        }
        if self.peek() == Some("(") {
            self.eat();
            let v = self.parse_or()?;
            if self.peek() != Some(")") {
                return Err(KashError::Runtime("[[: expected `)`".into()));
            }
            self.eat();
            return Ok(v);
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<bool> {
        // Up to three argv-shaped slots, mirroring `test`. Within
        // `[[…]]` we additionally recognise `==`/`!=` (glob match),
        // `=~` (regex), and lexical `<` / `>` as binary ops.
        let remaining = self.args.len() - self.pos;
        // Look ahead for binary operator at args[pos+1].
        if remaining >= 3 {
            let mid = self.args[self.pos + 1];
            if matches!(
                mid,
                "==" | "!="
                    | "=~"
                    | "="
                    | "<"
                    | ">"
                    | "-eq"
                    | "-ne"
                    | "-lt"
                    | "-le"
                    | "-gt"
                    | "-ge"
            ) {
                let lhs = self.args[self.pos];
                let rhs = self.args[self.pos + 2];
                self.pos += 3;
                return bracket_binary(lhs, mid, rhs);
            }
        }
        if remaining >= 2 {
            let head = self.args[self.pos];
            if head.starts_with('-') && head.len() == 2 {
                let arg = self.args[self.pos + 1];
                self.pos += 2;
                return test_unary(head, arg);
            }
        }
        if remaining >= 1 {
            let v = self.args[self.pos];
            self.pos += 1;
            return Ok(!v.is_empty());
        }
        // `[[ ]]` empty is false (matches the empty-test rule).
        Ok(false)
    }
}

fn bracket_binary(lhs: &str, op: &str, rhs: &str) -> Result<bool> {
    match op {
        "==" | "=" => Ok(glob_match(rhs, lhs)),
        "!=" => Ok(!glob_match(rhs, lhs)),
        "=~" => Ok(regex_match(rhs, lhs)),
        "<" => Ok(lhs < rhs),
        ">" => Ok(lhs > rhs),
        _ => test_binary(lhs, op, rhs),
    }
}

/// Match `text` against a POSIX-ERE-subset `pattern`. Supports:
///
/// - byte literals,
/// - `.` — any single byte,
/// - `*` / `+` / `?` — repetition of the previous atom,
/// - `^` / `$` — start / end anchors,
/// - `[abc]` / `[^abc]` / `[a-z]` — character class,
/// - `\X` — literal escape (`\.` matches `.`, etc.).
///
/// Not yet wired: alternation (`|`), grouping (`( … )`), backreferences,
/// non-greedy quantifiers. Implements anchored matching internally and
/// tries every starting position when the pattern doesn't lead with
/// `^`. Operates byte-by-byte.
pub fn regex_match(pattern: &str, text: &str) -> bool {
    let pat = pattern.as_bytes();
    let t = text.as_bytes();
    if pat.first() == Some(&b'^') {
        return re_match_here(&pat[1..], t);
    }
    let mut i = 0;
    loop {
        if re_match_here(pat, &t[i..]) {
            return true;
        }
        if i >= t.len() {
            return false;
        }
        i += 1;
    }
}

fn re_match_here(pat: &[u8], text: &[u8]) -> bool {
    if pat.is_empty() {
        return true;
    }
    if pat[0] == b'$' && pat.len() == 1 {
        return text.is_empty();
    }
    // Pull out the next atom + a possible repetition suffix.
    let (atom_len, atom_match): (usize, ReAtom) = re_lex_atom(pat);
    let rest_after_atom = &pat[atom_len..];
    let suffix = rest_after_atom.first().copied();
    match suffix {
        Some(b'*') => re_repeat(&atom_match, &rest_after_atom[1..], text, 0),
        Some(b'+') => re_repeat(&atom_match, &rest_after_atom[1..], text, 1),
        Some(b'?') => {
            // 0 or 1
            if !text.is_empty() && atom_match.matches(text[0])
                && re_match_here(&rest_after_atom[1..], &text[1..])
            {
                return true;
            }
            re_match_here(&rest_after_atom[1..], text)
        }
        _ => {
            if !text.is_empty() && atom_match.matches(text[0]) {
                return re_match_here(rest_after_atom, &text[1..]);
            }
            false
        }
    }
}

#[derive(Clone, Debug)]
enum ReAtom<'a> {
    Any,
    Literal(u8),
    Class { body: &'a [u8], negated: bool },
}

impl<'a> ReAtom<'a> {
    fn matches(&self, byte: u8) -> bool {
        match self {
            Self::Any => true,
            Self::Literal(b) => *b == byte,
            Self::Class { body, negated } => {
                let hit = class_matches(body, byte);
                hit != *negated
            }
        }
    }
}

/// Lex one regex atom off the front of `pat`. Returns the byte count
/// the atom occupies plus a matcher for a single byte.
fn re_lex_atom(pat: &[u8]) -> (usize, ReAtom<'_>) {
    match pat[0] {
        b'.' => (1, ReAtom::Any),
        b'\\' if pat.len() > 1 => (2, ReAtom::Literal(pat[1])),
        b'[' => {
            if let Some(close) = find_re_class_close(pat) {
                let body_start = if matches!(pat.get(1), Some(b'^' | b'!')) {
                    2
                } else {
                    1
                };
                let negated = matches!(pat.get(1), Some(b'^' | b'!'));
                (
                    close + 1,
                    ReAtom::Class {
                        body: &pat[body_start..close],
                        negated,
                    },
                )
            } else {
                // No `]` ever — treat `[` as a literal.
                (1, ReAtom::Literal(b'['))
            }
        }
        b => (1, ReAtom::Literal(b)),
    }
}

fn find_re_class_close(pat: &[u8]) -> Option<usize> {
    let mut i = 1;
    if matches!(pat.get(i), Some(b'^' | b'!')) {
        i += 1;
    }
    if pat.get(i) == Some(&b']') {
        i += 1; // leading `]` is a literal member
    }
    while i < pat.len() {
        if pat[i] == b']' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn re_repeat(atom: &ReAtom<'_>, rest: &[u8], text: &[u8], min: usize) -> bool {
    // Greedy match; backtrack to the shortest-acceptable length.
    let mut max = 0;
    while max < text.len() && atom.matches(text[max]) {
        max += 1;
    }
    let mut count = max;
    loop {
        if count >= min && re_match_here(rest, &text[count..]) {
            return true;
        }
        if count == 0 {
            return false;
        }
        count -= 1;
    }
}


// ===== helpers =====

const fn is_name_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

const fn is_name_continue(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

/// Parse `NAME` or `NAME=VALUE` for `local` / `readonly`. The `VALUE`
/// half is treated as a literal (no further expansion) — that matches
/// the `local FOO=bar` shorthand most carefully.
fn parse_name_eq_value(arg: &str) -> Result<(alloc::string::String, alloc::string::String)> {
    use alloc::string::ToString;
    if let Some(eq) = arg.find('=') {
        let (name, rest) = arg.split_at(eq);
        if !is_identifier(name) {
            return Err(KashError::Runtime(format!(
                "`{name}` is not a valid identifier"
            )));
        }
        Ok((name.to_string(), rest[1..].to_string()))
    } else {
        if !is_identifier(arg) {
            return Err(KashError::Runtime(format!(
                "`{arg}` is not a valid identifier"
            )));
        }
        Ok((arg.to_string(), alloc::string::String::new()))
    }
}

/// True iff `s` is a POSIX shell identifier (`_` or letter, then
/// `_` / letters / digits).
/// Read the body of an arithmetic expansion `$((…))` up to and
/// including the matching `))`. The caller has already consumed the
/// leading `$((`. Tracks balanced inner parens so that
/// `$((a + (b - c)))` reads `a + (b - c)` for the body.
fn read_arith_body(chars: &mut core::iter::Peekable<core::str::Chars<'_>>) -> Result<String> {
    let mut depth = 0usize;
    let mut body = String::new();
    while let Some(c) = chars.next() {
        if c == '(' {
            depth += 1;
            body.push(c);
        } else if c == ')' {
            if depth > 0 {
                depth -= 1;
                body.push(c);
            } else if chars.peek() == Some(&')') {
                chars.next();
                return Ok(body);
            } else {
                return Err(KashError::Parse(
                    "expected `))` to close `$((`".into(),
                ));
            }
        } else {
            body.push(c);
        }
    }
    Err(KashError::Parse(
        "unterminated `$((...))` arithmetic expansion".into(),
    ))
}

/// Recursive-descent arithmetic parser. Operates on a string buffer
/// (already through `$VAR` substitution) and reads / writes bare
/// identifiers via the evaluator's scope.
///
/// Supported surface (POSIX baseline + the C-style extensions every
/// modern shell ships):
///
/// - integer literals: decimal, octal (`0NNN`), hex (`0xNNN`),
/// - bare identifiers (looked up in scope; unset/empty → 0),
/// - parenthesised groups,
/// - prefix `++` / `--` and postfix `++` / `--` (lvalue required),
/// - unary `+ - ! ~`,
/// - binary `* / %`, `+ -`, `<< >>`, `< <= > >=`, `== !=`,
///   `&`, `^`, `|`, `&&`, `||`,
/// - ternary `cond ? a : b` (right-associative),
/// - assignment `= += -= *= /= %= &= |= ^= <<= >>=` (right-assoc;
///   LHS must be a bare identifier).
///
/// Not yet wired: the comma operator. The full kash-extended numeric
/// surface (floats, complex, big integers) per
/// `project_shell_arithmetic.md` is its own commit.
struct ArithParser<'a, 'e> {
    src: &'a str,
    pos: usize,
    ev: &'e mut Evaluator,
}

#[derive(Clone, Copy, Debug)]
enum AssignOp {
    Plain,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

impl<'a, 'e> ArithParser<'a, 'e> {
    fn parse_expr(&mut self) -> Result<i64> {
        self.parse_assign()
    }

    fn parse_assign(&mut self) -> Result<i64> {
        self.skip_ws();
        let save = self.pos;
        if let Some(name) = self.try_read_identifier() {
            self.skip_ws();
            if let Some(op) = self.try_consume_assign_op() {
                let rhs = self.parse_assign()?;
                return self.apply_assign(&name, op, rhs);
            }
            self.pos = save;
        }
        self.parse_ternary()
    }

    fn try_consume_assign_op(&mut self) -> Option<AssignOp> {
        let pairs = [
            ("<<=", AssignOp::Shl),
            (">>=", AssignOp::Shr),
            ("+=", AssignOp::Add),
            ("-=", AssignOp::Sub),
            ("*=", AssignOp::Mul),
            ("/=", AssignOp::Div),
            ("%=", AssignOp::Mod),
            ("&=", AssignOp::BitAnd),
            ("|=", AssignOp::BitOr),
            ("^=", AssignOp::BitXor),
        ];
        for (sym, op) in pairs {
            if self.src[self.pos..].starts_with(sym) {
                self.pos += sym.len();
                return Some(op);
            }
        }
        if self.src[self.pos..].starts_with('=')
            && !self.src[self.pos..].starts_with("==")
        {
            self.pos += 1;
            return Some(AssignOp::Plain);
        }
        None
    }

    fn apply_assign(&mut self, name: &str, op: AssignOp, rhs: i64) -> Result<i64> {
        let current = self.read_named(name)?;
        let new = match op {
            AssignOp::Plain => rhs,
            AssignOp::Add => current
                .checked_add(rhs)
                .ok_or_else(|| KashError::Runtime("arithmetic overflow".into()))?,
            AssignOp::Sub => current
                .checked_sub(rhs)
                .ok_or_else(|| KashError::Runtime("arithmetic overflow".into()))?,
            AssignOp::Mul => current
                .checked_mul(rhs)
                .ok_or_else(|| KashError::Runtime("arithmetic overflow".into()))?,
            AssignOp::Div => {
                if rhs == 0 {
                    return Err(KashError::Runtime("arithmetic: divide by zero".into()));
                }
                current / rhs
            }
            AssignOp::Mod => {
                if rhs == 0 {
                    return Err(KashError::Runtime("arithmetic: modulo by zero".into()));
                }
                current % rhs
            }
            AssignOp::BitAnd => current & rhs,
            AssignOp::BitOr => current | rhs,
            AssignOp::BitXor => current ^ rhs,
            AssignOp::Shl => current.wrapping_shl(rhs as u32),
            AssignOp::Shr => current.wrapping_shr(rhs as u32),
        };
        self.ev
            .scope
            .assign(name, Value::Scalar(alloc::format!("{new}")))?;
        Ok(new)
    }

    fn parse_ternary(&mut self) -> Result<i64> {
        let cond = self.parse_or()?;
        self.skip_ws();
        if self.try_consume_exact("?") {
            let then_val = self.parse_assign()?;
            self.skip_ws();
            if !self.try_consume_exact(":") {
                return Err(KashError::Parse(
                    "arithmetic: expected `:` after `?`".into(),
                ));
            }
            let else_val = self.parse_assign()?;
            Ok(if cond != 0 { then_val } else { else_val })
        } else {
            Ok(cond)
        }
    }

    fn parse_or(&mut self) -> Result<i64> {
        let mut lhs = self.parse_and()?;
        while self.try_consume_exact("||") {
            let rhs = self.parse_and()?;
            lhs = (lhs != 0 || rhs != 0) as i64;
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<i64> {
        let mut lhs = self.parse_bit_or()?;
        while self.try_consume_exact("&&") {
            let rhs = self.parse_bit_or()?;
            lhs = (lhs != 0 && rhs != 0) as i64;
        }
        Ok(lhs)
    }

    fn parse_bit_or(&mut self) -> Result<i64> {
        let mut lhs = self.parse_bit_xor()?;
        while self.try_consume_single('|') {
            let rhs = self.parse_bit_xor()?;
            lhs |= rhs;
        }
        Ok(lhs)
    }

    fn parse_bit_xor(&mut self) -> Result<i64> {
        let mut lhs = self.parse_bit_and()?;
        while self.try_consume_single('^') {
            let rhs = self.parse_bit_and()?;
            lhs ^= rhs;
        }
        Ok(lhs)
    }

    fn parse_bit_and(&mut self) -> Result<i64> {
        let mut lhs = self.parse_eq()?;
        while self.try_consume_single('&') {
            let rhs = self.parse_eq()?;
            lhs &= rhs;
        }
        Ok(lhs)
    }

    fn parse_eq(&mut self) -> Result<i64> {
        let mut lhs = self.parse_rel()?;
        loop {
            if self.try_consume_exact("==") {
                let rhs = self.parse_rel()?;
                lhs = (lhs == rhs) as i64;
            } else if self.try_consume_exact("!=") {
                let rhs = self.parse_rel()?;
                lhs = (lhs != rhs) as i64;
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_rel(&mut self) -> Result<i64> {
        let mut lhs = self.parse_shift()?;
        loop {
            if self.try_consume_exact("<=") {
                let rhs = self.parse_shift()?;
                lhs = (lhs <= rhs) as i64;
            } else if self.try_consume_exact(">=") {
                let rhs = self.parse_shift()?;
                lhs = (lhs >= rhs) as i64;
            } else if self.try_consume_single('<') {
                let rhs = self.parse_shift()?;
                lhs = (lhs < rhs) as i64;
            } else if self.try_consume_single('>') {
                let rhs = self.parse_shift()?;
                lhs = (lhs > rhs) as i64;
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_shift(&mut self) -> Result<i64> {
        let mut lhs = self.parse_add()?;
        loop {
            if self.try_consume_exact("<<") {
                let rhs = self.parse_add()?;
                lhs = lhs.wrapping_shl(rhs as u32);
            } else if self.try_consume_exact(">>") {
                let rhs = self.parse_add()?;
                lhs = lhs.wrapping_shr(rhs as u32);
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_add(&mut self) -> Result<i64> {
        let mut lhs = self.parse_mul()?;
        loop {
            if self.try_consume_single('+') {
                let rhs = self.parse_mul()?;
                lhs = lhs
                    .checked_add(rhs)
                    .ok_or_else(|| KashError::Runtime("arithmetic overflow".into()))?;
            } else if self.try_consume_single('-') {
                let rhs = self.parse_mul()?;
                lhs = lhs
                    .checked_sub(rhs)
                    .ok_or_else(|| KashError::Runtime("arithmetic overflow".into()))?;
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<i64> {
        let mut lhs = self.parse_unary()?;
        loop {
            if self.try_consume_single('*') {
                let rhs = self.parse_unary()?;
                lhs = lhs
                    .checked_mul(rhs)
                    .ok_or_else(|| KashError::Runtime("arithmetic overflow".into()))?;
            } else if self.try_consume_single('/') {
                let rhs = self.parse_unary()?;
                if rhs == 0 {
                    return Err(KashError::Runtime("arithmetic: divide by zero".into()));
                }
                lhs /= rhs;
            } else if self.try_consume_single('%') {
                let rhs = self.parse_unary()?;
                if rhs == 0 {
                    return Err(KashError::Runtime("arithmetic: modulo by zero".into()));
                }
                lhs %= rhs;
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<i64> {
        self.skip_ws();
        if self.try_consume_exact("++") {
            let name = self.try_read_identifier().ok_or_else(|| {
                KashError::Parse("arithmetic: `++` requires an lvalue".into())
            })?;
            let new = self
                .read_named(&name)?
                .checked_add(1)
                .ok_or_else(|| KashError::Runtime("arithmetic overflow".into()))?;
            self.ev
                .scope
                .assign(&name, Value::Scalar(alloc::format!("{new}")))?;
            return Ok(new);
        }
        if self.try_consume_exact("--") {
            let name = self.try_read_identifier().ok_or_else(|| {
                KashError::Parse("arithmetic: `--` requires an lvalue".into())
            })?;
            let new = self
                .read_named(&name)?
                .checked_sub(1)
                .ok_or_else(|| KashError::Runtime("arithmetic overflow".into()))?;
            self.ev
                .scope
                .assign(&name, Value::Scalar(alloc::format!("{new}")))?;
            return Ok(new);
        }
        if self.try_consume_single('+') {
            return self.parse_unary();
        }
        if self.try_consume_single('-') {
            let v = self.parse_unary()?;
            return v
                .checked_neg()
                .ok_or_else(|| KashError::Runtime("arithmetic overflow".into()));
        }
        if self.try_consume_single('!') {
            let v = self.parse_unary()?;
            return Ok((v == 0) as i64);
        }
        if self.try_consume_single('~') {
            let v = self.parse_unary()?;
            return Ok(!v);
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<i64> {
        self.skip_ws();
        if self.try_consume_exact("(") {
            let v = self.parse_expr()?;
            self.skip_ws();
            if !self.try_consume_exact(")") {
                return Err(KashError::Parse(
                    "arithmetic: expected `)`".into(),
                ));
            }
            return Ok(v);
        }
        if let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                return self.parse_number();
            }
            if c == '_' || c.is_ascii_alphabetic() {
                let name = self
                    .try_read_identifier()
                    .expect("just peeked an identifier start");
                self.skip_ws();
                if self.try_consume_exact("++") {
                    let current = self.read_named(&name)?;
                    let new = current
                        .checked_add(1)
                        .ok_or_else(|| KashError::Runtime("arithmetic overflow".into()))?;
                    self.ev
                        .scope
                        .assign(&name, Value::Scalar(alloc::format!("{new}")))?;
                    return Ok(current);
                }
                if self.try_consume_exact("--") {
                    let current = self.read_named(&name)?;
                    let new = current
                        .checked_sub(1)
                        .ok_or_else(|| KashError::Runtime("arithmetic overflow".into()))?;
                    self.ev
                        .scope
                        .assign(&name, Value::Scalar(alloc::format!("{new}")))?;
                    return Ok(current);
                }
                return self.read_named(&name);
            }
        }
        Err(KashError::Parse(alloc::format!(
            "arithmetic: unexpected character at position {}",
            self.pos
        )))
    }

    fn parse_number(&mut self) -> Result<i64> {
        let start = self.pos;
        if self.peek() == Some('0') && matches!(self.peek_at(1), Some('x' | 'X')) {
            self.advance();
            self.advance();
            let digits_start = self.pos;
            while let Some(c) = self.peek() {
                if c.is_ascii_hexdigit() {
                    self.advance();
                } else {
                    break;
                }
            }
            let lit = &self.src[digits_start..self.pos];
            if lit.is_empty() {
                return Err(KashError::Parse(
                    "arithmetic: empty hex literal".into(),
                ));
            }
            return i64::from_str_radix(lit, 16).map_err(|_| {
                KashError::Parse(alloc::format!("arithmetic: invalid hex literal `0x{lit}`"))
            });
        }
        if self.peek() == Some('0')
            && matches!(self.peek_at(1), Some('0'..='7'))
        {
            self.advance();
            let digits_start = self.pos;
            while let Some(c) = self.peek() {
                if matches!(c, '0'..='7') {
                    self.advance();
                } else {
                    break;
                }
            }
            let lit = &self.src[digits_start..self.pos];
            return i64::from_str_radix(lit, 8).map_err(|_| {
                KashError::Parse(alloc::format!("arithmetic: invalid octal literal `0{lit}`"))
            });
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }
        let lit = &self.src[start..self.pos];
        lit.parse::<i64>().map_err(|_| {
            KashError::Parse(alloc::format!("arithmetic: invalid integer `{lit}`"))
        })
    }

    fn try_read_identifier(&mut self) -> Option<String> {
        self.skip_ws();
        let start = self.pos;
        let Some(c) = self.peek() else { return None };
        if !(c == '_' || c.is_ascii_alphabetic()) {
            return None;
        }
        let mut name = String::new();
        while let Some(c) = self.peek() {
            if c == '_' || c.is_ascii_alphanumeric() {
                name.push(c);
                self.advance();
            } else {
                break;
            }
        }
        if name.is_empty() {
            self.pos = start;
            None
        } else {
            Some(name)
        }
    }

    fn read_named(&self, name: &str) -> Result<i64> {
        let value = self
            .ev
            .scope
            .get(name)
            .map(|v| v.to_scalar_string())
            .unwrap_or_default();
        if value.is_empty() {
            return Ok(0);
        }
        value.trim().parse::<i64>().map_err(|_| {
            KashError::Runtime(alloc::format!(
                "arithmetic: `{name}`'s value `{value}` is not a number"
            ))
        })
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn try_consume_exact(&mut self, s: &str) -> bool {
        self.skip_ws();
        if self.src[self.pos..].starts_with(s) {
            self.pos += s.len();
            true
        } else {
            false
        }
    }

    fn try_consume_single(&mut self, c: char) -> bool {
        self.skip_ws();
        if self.peek() != Some(c) {
            return false;
        }
        if matches!(c, '&' | '|' | '<' | '>' | '+' | '-')
            && self.peek_at(1) == Some(c)
        {
            return false;
        }
        if matches!(c, '+' | '-' | '*' | '/' | '%' | '&' | '|' | '^' | '<' | '>')
            && self.peek_at(1) == Some('=')
        {
            return false;
        }
        self.advance();
        true
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek_at(&self, off: usize) -> Option<char> {
        self.src[self.pos..].chars().nth(off)
    }

    fn advance(&mut self) {
        if let Some(c) = self.peek() {
            self.pos += c.len_utf8();
        }
    }

    fn expect_end(&mut self) -> Result<()> {
        self.skip_ws();
        if self.pos < self.src.len() {
            return Err(KashError::Parse(alloc::format!(
                "arithmetic: trailing input `{}`",
                &self.src[self.pos..]
            )));
        }
        Ok(())
    }
}

/// Read a `$( … )` body up to and including the matching `)`. The
/// leading `$(` is expected to have already been consumed. Returns
/// the raw body between the parens (without the parens themselves).
/// Nested parens are tracked so e.g. `$(echo (sub))` works.
fn read_paren_body(chars: &mut core::iter::Peekable<core::str::Chars<'_>>) -> Result<String> {
    let mut depth = 1usize;
    let mut body = String::new();
    for c in chars.by_ref() {
        if c == '(' {
            depth += 1;
            body.push(c);
        } else if c == ')' {
            depth -= 1;
            if depth == 0 {
                return Ok(body);
            }
            body.push(c);
        } else {
            body.push(c);
        }
    }
    Err(KashError::Parse(
        "unterminated `$(...)` command substitution".into(),
    ))
}

/// Read a backtick body up to and including the matching backtick.
/// The leading backtick is expected to have already been consumed.
/// Inside a backtick body, `\\` escapes the next byte (the POSIX
/// rule); other characters are passed through verbatim.
fn read_backtick_body(chars: &mut core::iter::Peekable<core::str::Chars<'_>>) -> Result<String> {
    let mut body = String::new();
    while let Some(c) = chars.next() {
        if c == '`' {
            return Ok(body);
        }
        if c == '\\' {
            if let Some(&n) = chars.peek() {
                if matches!(n, '$' | '`' | '\\') {
                    chars.next();
                    body.push(n);
                    continue;
                }
            }
            body.push('\\');
            continue;
        }
        body.push(c);
    }
    Err(KashError::Parse(
        "unterminated backtick command substitution".into(),
    ))
}

/// Return the first character of `ifs` as an owned string, or an
/// empty string when `IFS` is empty. POSIX uses this as the join
/// separator for `"$*"`.
fn first_ifs_char(ifs: &str) -> String {
    match ifs.chars().next() {
        Some(c) => {
            let mut s = String::new();
            s.push(c);
            s
        }
        None => String::new(),
    }
}

/// Append `value` to `fields`, splitting on IFS bytes. Matches the
/// POSIX rule "unquoted expansion results undergo field splitting"
/// with a minimal-but-correct-for-the-common-case implementation:
///
/// - An empty `value` produces no fields (the unquoted empty
///   expansion vanishes).
/// - Otherwise the value is split on any byte in `ifs`, and runs of
///   empty fields are dropped. That matches the POSIX "whitespace
///   IFS chars are collapsed" rule for the default IFS of
///   `" \t\n"`; non-whitespace IFS chars don't yet get their strict-
///   separator treatment.
/// - The first non-empty part is appended to the current field; each
///   subsequent part starts a new field.
fn append_split(value: &str, ifs: &str, fields: &mut Vec<String>) {
    if value.is_empty() {
        return;
    }
    if ifs.is_empty() {
        fields
            .last_mut()
            .expect("fields invariant")
            .push_str(value);
        return;
    }
    let parts: Vec<&str> = value
        .split(|c| ifs.contains(c))
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return;
    }
    fields
        .last_mut()
        .expect("fields invariant")
        .push_str(parts[0]);
    for p in &parts[1..] {
        fields.push((*p).into());
    }
}

/// True iff `w` has at least one quoted segment. A quoted segment
/// (even when its body is empty) survives POSIX field splitting as a
/// literal empty argument.
fn word_has_quoted_segment(w: &Word) -> bool {
    w.segments.iter().any(|s| {
        matches!(
            s,
            WordSegment::SingleQuoted(_)
                | WordSegment::DoubleQuoted(_)
                | WordSegment::AnsiC(_)
        )
    })
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_name_start(first) {
        return false;
    }
    chars.all(is_name_continue)
}

fn is_valid_param_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_name_start(first) {
        return false;
    }
    chars.all(is_name_continue)
}

/// POSIX glob match. Recognises:
///
/// - `*` — any (possibly empty) byte run,
/// - `?` — exactly one byte,
/// - `\X` — literal `X` (any meta-character can be escaped this way),
/// - `[abc]` / `[a-z]` / `[!abc]` (and `[^abc]`) — character class,
/// - `[[:alpha:]]` and the other POSIX character classes inside `[]`:
///   `alpha`, `digit`, `alnum`, `upper`, `lower`, `space`, `xdigit`,
///   `cntrl`, `print`, `punct`, `graph`, `blank`.
///
/// `*` / `?` / `[` lose their special meaning when prefixed with `\\`
/// in the pattern. The matcher operates byte-by-byte so any pattern
/// containing only ASCII meta-characters works correctly on UTF-8
/// input; non-ASCII patterns inside `[…]` are still byte-level which
/// is good enough for the cases ksh93 / bash also handle.
fn glob_match(pat: &str, s: &str) -> bool {
    glob_match_bytes(pat.as_bytes(), s.as_bytes())
}

fn glob_match_bytes(pat: &[u8], s: &[u8]) -> bool {
    let (p0, s0) = (pat.first().copied(), s.first().copied());
    // ksh93 / bash extglob: `?(p)` / `*(p)` / `+(p)` / `@(p)` / `!(p)`.
    if matches!(p0, Some(b'?' | b'*' | b'+' | b'@' | b'!'))
        && pat.get(1) == Some(&b'(')
    {
        if let Some((inner, rest_off)) = extglob_split(pat) {
            let head = pat[0];
            let rest = &pat[rest_off..];
            return extglob_match(head, &inner, rest, s);
        }
    }
    match (p0, s0) {
        (None, None) => true,
        (None, _) => false,
        (Some(b'\\'), _) if pat.len() > 1 => {
            // `\X` — the next pattern byte matches itself literally.
            match s0 {
                Some(c) if c == pat[1] => glob_match_bytes(&pat[2..], &s[1..]),
                _ => false,
            }
        }
        (Some(b'*'), _) => {
            for i in 0..=s.len() {
                if glob_match_bytes(&pat[1..], &s[i..]) {
                    return true;
                }
            }
            false
        }
        (Some(b'?'), Some(_)) => glob_match_bytes(&pat[1..], &s[1..]),
        (Some(b'['), Some(c)) => {
            let Some((class_end, _)) = find_class_close(pat) else {
                // Unclosed `[` — match literally.
                return s0 == Some(b'[') && glob_match_bytes(&pat[1..], &s[1..]);
            };
            let class = &pat[1..class_end];
            let (negate, class) =
                if let Some(rest) = class.strip_prefix(b"!").or_else(|| class.strip_prefix(b"^")) {
                    (true, rest)
                } else {
                    (false, class)
                };
            let hit = class_matches(class, c);
            if hit == negate {
                return false;
            }
            glob_match_bytes(&pat[class_end + 1..], &s[1..])
        }
        (Some(p), Some(c)) if p == c => glob_match_bytes(&pat[1..], &s[1..]),
        _ => false,
    }
}

/// Split an extglob construct `X(p1|p2|...)` (where `X` is one of
/// `?`, `*`, `+`, `@`, `!`) off the front of `pat`. Returns the body
/// (between `(` and the matching `)`) plus the offset just past the
/// closing `)`. None if the parens aren't balanced or the leader
/// doesn't look like an extglob start.
fn extglob_split(pat: &[u8]) -> Option<(Vec<u8>, usize)> {
    if pat.len() < 3 {
        return None;
    }
    if pat[1] != b'(' {
        return None;
    }
    let mut depth = 1usize;
    let mut i = 2;
    while i < pat.len() {
        match pat[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    let body = pat[2..i].to_vec();
                    return Some((body, i + 1));
                }
            }
            // `\X` — skip the escape pair so a `\)` doesn't break us.
            b'\\' if i + 1 < pat.len() => {
                i += 2;
                continue;
            }
            // Nested `[...]` shouldn't disturb our paren tracking.
            b'[' => {
                if let Some(close) = pat[i..].iter().position(|&b| b == b']') {
                    i += close + 1;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split an extglob inner body on top-level `|` characters,
/// respecting nested `( … )` and `[ … ]`.
fn extglob_alternatives(body: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;
    while i < body.len() {
        let b = body[i];
        match b {
            b'(' => {
                depth += 1;
                current.push(b);
            }
            b')' => {
                if depth > 0 {
                    depth -= 1;
                }
                current.push(b);
            }
            b'[' => {
                if let Some(close) = body[i..].iter().position(|&c| c == b']') {
                    current.extend_from_slice(&body[i..=i + close]);
                    i += close + 1;
                    continue;
                }
                current.push(b);
            }
            b'\\' if i + 1 < body.len() => {
                current.push(b);
                current.push(body[i + 1]);
                i += 2;
                continue;
            }
            b'|' if depth == 0 => {
                out.push(core::mem::take(&mut current));
                i += 1;
                continue;
            }
            _ => current.push(b),
        }
        i += 1;
    }
    out.push(current);
    out
}

fn extglob_match(head: u8, inner: &[u8], rest: &[u8], s: &[u8]) -> bool {
    let alts = extglob_alternatives(inner);
    // Try to consume some prefix of `s` according to the head's
    // repetition semantics and then match `rest` against what's left.
    match head {
        b'?' => {
            // 0 or 1 occurrence of any alternative.
            if glob_match_bytes(rest, s) {
                return true;
            }
            for alt in &alts {
                if let Some(after) = consume_once(alt, s) {
                    if glob_match_bytes(rest, after) {
                        return true;
                    }
                }
            }
            false
        }
        b'@' => {
            // Exactly one occurrence.
            for alt in &alts {
                if let Some(after) = consume_once(alt, s) {
                    if glob_match_bytes(rest, after) {
                        return true;
                    }
                }
            }
            false
        }
        b'*' => extglob_repeat(&alts, rest, s, 0),
        b'+' => extglob_repeat(&alts, rest, s, 1),
        b'!' => {
            // Everything except: prefixes of `s` that don't match any
            // alternative *and* allow the rest to consume the
            // remainder.
            for split in 0..=s.len() {
                let prefix = &s[..split];
                let after = &s[split..];
                let matches_any = alts
                    .iter()
                    .any(|alt| glob_match_bytes(alt, prefix));
                if !matches_any && glob_match_bytes(rest, after) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// Match `alt` against the whole of `s`; on success return what was
/// consumed (we only try the full-length consumption, because that
/// matches typical extglob usage). Returns `None` if no full match.
fn consume_once<'a>(alt: &[u8], s: &'a [u8]) -> Option<&'a [u8]> {
    // Try every prefix of `s` and see which one fully matches `alt`.
    // (`alt` itself is a glob pattern.)
    for end in (0..=s.len()).rev() {
        if glob_match_bytes(alt, &s[..end]) {
            return Some(&s[end..]);
        }
    }
    None
}

fn extglob_repeat(alts: &[Vec<u8>], rest: &[u8], s: &[u8], min: usize) -> bool {
    // Try consuming `count` occurrences, starting from the greediest
    // viable and backtracking down.
    fn helper(alts: &[Vec<u8>], rest: &[u8], s: &[u8], min: usize, count: usize) -> bool {
        if count >= min && glob_match_bytes(rest, s) {
            return true;
        }
        if s.is_empty() {
            return false;
        }
        // Try every starting alternative + every consume length.
        for alt in alts {
            for end in 1..=s.len() {
                if glob_match_bytes(alt, &s[..end])
                    && helper(alts, rest, &s[end..], min, count + 1)
                {
                    return true;
                }
            }
        }
        false
    }
    helper(alts, rest, s, min, 0)
}

/// Find the position of the `]` that closes a character class
/// starting at `pat[0] == '['`. Handles `[[:name:]…]` correctly by
/// scanning past nested `[:...:]` POSIX classes (which contain `]`
/// inside `:]`). A leading `]` immediately after `[` (or after `[!`/
/// `[^`) is treated as a literal `]` member, per POSIX.
fn find_class_close(pat: &[u8]) -> Option<(usize, ())> {
    if pat.first() != Some(&b'[') {
        return None;
    }
    let mut i = 1;
    // Skip a leading `!` / `^` (negation marker).
    if matches!(pat.get(i), Some(b'!' | b'^')) {
        i += 1;
    }
    // Allow `]` as the very first class member.
    if pat.get(i) == Some(&b']') {
        i += 1;
    }
    while i < pat.len() {
        match pat[i] {
            b']' => return Some((i, ())),
            b'[' if pat.get(i + 1) == Some(&b':') => {
                // Skip a `[:name:]` POSIX class.
                let mut j = i + 2;
                while j + 1 < pat.len() {
                    if pat[j] == b':' && pat[j + 1] == b']' {
                        i = j + 2;
                        break;
                    }
                    j += 1;
                }
                if i < j + 2 {
                    // Unterminated `[:` — bail out, treat outer `[` as
                    // literal upstream.
                    return None;
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// True iff `c` matches the body of a character class
/// (between `[` and `]`, with the leading negation already stripped).
fn class_matches(class: &[u8], c: u8) -> bool {
    let mut i = 0;
    while i < class.len() {
        // `[:name:]` form.
        if class[i] == b'[' && class.get(i + 1) == Some(&b':') {
            let start = i + 2;
            if let Some(off) = class[start..]
                .windows(2)
                .position(|w| w == b":]")
            {
                let name = &class[start..start + off];
                if posix_class_matches(name, c) {
                    return true;
                }
                i = start + off + 2;
                continue;
            }
            // Unterminated `[:` — treat the `[` as literal.
        }
        // `X-Y` range.
        if i + 2 < class.len() && class[i + 1] == b'-' && class[i + 2] != b']' {
            if c >= class[i] && c <= class[i + 2] {
                return true;
            }
            i += 3;
            continue;
        }
        if class[i] == c {
            return true;
        }
        i += 1;
    }
    false
}

fn posix_class_matches(name: &[u8], c: u8) -> bool {
    match name {
        b"alpha" => c.is_ascii_alphabetic(),
        b"digit" => c.is_ascii_digit(),
        b"alnum" => c.is_ascii_alphanumeric(),
        b"upper" => c.is_ascii_uppercase(),
        b"lower" => c.is_ascii_lowercase(),
        b"space" => c.is_ascii_whitespace(),
        b"xdigit" => c.is_ascii_hexdigit(),
        b"cntrl" => c.is_ascii_control(),
        b"print" => (0x20..=0x7e).contains(&c),
        b"punct" => c.is_ascii_punctuation(),
        b"graph" => c.is_ascii_graphic(),
        b"blank" => c == b' ' || c == b'\t',
        _ => false,
    }
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

    // ===== baseline (carried over from the previous commit) =====

    #[test]
    fn colon_returns_zero() {
        let (o, out, _) = run(":");
        assert_eq!(o, Outcome::Status(0));
        assert!(out.is_empty());
    }

    #[test]
    fn echo_writes_to_output_buffer() {
        let (_, out, _) = run("echo hello world");
        assert_eq!(out, "hello world\n");
    }

    #[test]
    fn assignment_persists_in_scope() {
        let (_, _, ev) = run("FOO=bar");
        assert_eq!(ev.scope().get("FOO").unwrap().to_scalar_string(), "bar");
    }

    #[test]
    fn exit_propagates_outcome() {
        let (o, _, _) = run("exit 7");
        assert_eq!(o, Outcome::Exit(7));
    }

    // ===== parameter expansion =====

    #[test]
    fn bare_dollar_var_expands() {
        let (_, out, _) = run("FOO=bar; echo $FOO");
        assert_eq!(out, "bar\n");
    }

    #[test]
    fn double_quoted_dollar_expands() {
        let (_, out, _) = run("FOO=bar; echo \"hi $FOO\"");
        assert_eq!(out, "hi bar\n");
    }

    #[test]
    fn single_quoted_dollar_does_not_expand() {
        let (_, out, _) = run("FOO=bar; echo 'hi $FOO'");
        assert_eq!(out, "hi $FOO\n");
    }

    #[test]
    fn braced_dollar_var_expands() {
        let (_, out, _) = run("FOO=bar; echo ${FOO}");
        assert_eq!(out, "bar\n");
    }

    #[test]
    fn unset_var_is_empty() {
        let (_, out, _) = run("echo a$NOPE b");
        assert_eq!(out, "a b\n");
    }

    #[test]
    fn default_value_colon_dash() {
        let (_, out, _) = run("echo ${X:-fallback}");
        assert_eq!(out, "fallback\n");
    }

    #[test]
    fn default_value_returns_existing_when_set() {
        let (_, out, _) = run("X=set; echo ${X:-fallback}");
        assert_eq!(out, "set\n");
    }

    #[test]
    fn assign_default_writes_back() {
        let (_, out, ev) = run("echo ${X:=fallback}; echo $X");
        assert_eq!(out, "fallback\nfallback\n");
        assert_eq!(ev.scope().get("X").unwrap().to_scalar_string(), "fallback");
    }

    #[test]
    fn alternate_form_returns_alt_when_set() {
        let (_, out, _) = run("X=y; echo ${X:+alt}");
        assert_eq!(out, "alt\n");
    }

    #[test]
    fn alternate_form_empty_when_unset() {
        let (_, out, _) = run("echo a${X:+alt}b");
        assert_eq!(out, "ab\n");
    }

    #[test]
    fn error_form_raises_when_unset() {
        let prog = parse("echo ${X:?missing}").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert!(format!("{err}").contains("missing"), "got: {err}");
    }

    #[test]
    fn length_form_counts_chars() {
        let (_, out, _) = run("X=hello; echo ${#X}");
        assert_eq!(out, "5\n");
    }

    #[test]
    fn dollar_last_status() {
        let (_, out, _) = run("false; echo $?");
        assert_eq!(out, "1\n");
    }

    #[test]
    fn unmatched_dollar_emits_literal() {
        let (_, out, _) = run("echo $");
        assert_eq!(out, "$\n");
    }

    // ===== compound: brace / subshell =====

    #[test]
    fn brace_group_runs_in_current_scope() {
        let (_, out, ev) = run("{ X=inside; echo $X; }; echo $X");
        assert_eq!(out, "inside\ninside\n");
        assert_eq!(ev.scope().get("X").unwrap().to_scalar_string(), "inside");
    }

    #[test]
    fn subshell_isolates_variable_writes() {
        let (_, out, ev) = run("X=outer; ( X=inner; echo $X ); echo $X");
        assert_eq!(out, "inner\nouter\n");
        assert_eq!(ev.scope().get("X").unwrap().to_scalar_string(), "outer");
    }

    // ===== compound: if =====

    #[test]
    fn if_true_runs_body() {
        let (_, out, _) = run("if true; then echo yes; fi");
        assert_eq!(out, "yes\n");
    }

    #[test]
    fn if_false_runs_else() {
        let (_, out, _) = run("if false; then echo yes; else echo no; fi");
        assert_eq!(out, "no\n");
    }

    #[test]
    fn if_elif_takes_first_match() {
        let (_, out, _) = run(
            "if false; then echo a; elif true; then echo b; elif true; then echo c; fi",
        );
        assert_eq!(out, "b\n");
    }

    // ===== compound: while / until =====

    #[test]
    fn while_runs_until_cond_fails() {
        // Without a working `test`/`[`, route the condition through
        // `case` so we get explicit success/failure branches.
        let (_, out, _) = run(
            "N=2; while case $N in 0) false;; *) true;; esac; do echo $N; N=0; done",
        );
        assert_eq!(out, "2\n");
    }

    #[test]
    fn until_runs_until_cond_succeeds() {
        let (_, out, _) = run(
            "N=0; until case $N in 0) false;; *) true;; esac; do echo loop; N=1; done",
        );
        assert_eq!(out, "loop\n");
    }

    // ===== compound: for =====

    #[test]
    fn for_in_iterates_words() {
        let (_, out, _) = run("for x in a b c; do echo $x; done");
        assert_eq!(out, "a\nb\nc\n");
    }

    #[test]
    fn for_without_in_iterates_positionals() {
        let prog = parse("for x; do echo $x; done").unwrap();
        let mut ev = Evaluator::new();
        ev.positionals = alloc::vec!["one".into(), "two".into()];
        ev.eval_program(&prog).unwrap();
        assert_eq!(ev.take_output(), "one\ntwo\n");
    }

    // ===== compound: case =====

    #[test]
    fn case_matches_literal() {
        let (_, out, _) = run("X=b; case $X in a) echo aa;; b) echo bb;; esac");
        assert_eq!(out, "bb\n");
    }

    #[test]
    fn case_matches_pipe_alternatives() {
        let (_, out, _) = run("X=c; case $X in a|b|c) echo abc;; esac");
        assert_eq!(out, "abc\n");
    }

    #[test]
    fn case_glob_star_pattern() {
        let (_, out, _) = run("X=foobar; case $X in foo*) echo prefix;; esac");
        assert_eq!(out, "prefix\n");
    }

    #[test]
    fn case_glob_question_pattern() {
        let (_, out, _) = run("X=ab; case $X in '??') echo two;; esac");
        assert_eq!(out, "two\n");
    }

    #[test]
    fn case_class_pattern() {
        let (_, out, _) = run("X=z; case $X in [a-z]) echo lower;; esac");
        assert_eq!(out, "lower\n");
    }

    #[test]
    fn case_continue_runs_next_arm_unconditionally() {
        let (_, out, _) = run(
            "X=a; case $X in a) echo first;& b) echo second;; c) echo third;; esac",
        );
        assert_eq!(out, "first\nsecond\n");
    }

    // ===== functions =====

    #[test]
    fn posix_function_callable() {
        let (_, out, _) = run("greet() { echo hi; }; greet");
        assert_eq!(out, "hi\n");
    }

    #[test]
    fn function_sees_positional_args() {
        let (_, out, _) = run("greet() { echo \"hi $1\"; }; greet world");
        assert_eq!(out, "hi world\n");
    }

    #[test]
    fn function_argc_dollar_hash() {
        let (_, out, _) = run("count() { echo $#; }; count a b c");
        assert_eq!(out, "3\n");
    }

    #[test]
    fn posix_function_assignment_propagates_to_caller() {
        // POSIX `name()` form is dynamic-scoped: a bare assignment
        // inside the body modifies the caller's binding (or creates a
        // global if none exists).
        let (_, _, ev) = run("setit() { X=inside; }; setit");
        assert_eq!(ev.scope().get("X").unwrap().to_scalar_string(), "inside");
    }

    #[test]
    fn ksh_function_assignment_stays_local() {
        // ksh93 `function NAME` form is statically scoped: bare
        // assignments in the body act as `local` by default.
        let (_, _, ev) = run("function setit { X=inside; }; setit");
        assert!(ev.scope().get("X").is_none());
    }

    #[test]
    fn local_builtin_shadows_caller_binding() {
        let (_, out, ev) = run(
            "X=outer; setit() { local X=inner; echo $X; }; setit; echo $X",
        );
        assert_eq!(out, "inner\nouter\n");
        assert_eq!(ev.scope().get("X").unwrap().to_scalar_string(), "outer");
    }

    #[test]
    fn local_outside_function_errors() {
        let prog = parse("local X=foo").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert!(format!("{err}").contains("inside a function"), "got: {err}");
    }

    #[test]
    fn readonly_blocks_subsequent_assignment() {
        let prog = parse("readonly X=fixed; X=other").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert!(matches!(err, KashError::Readonly(_)));
    }

    #[test]
    fn readonly_allows_first_value_then_locks() {
        let (_, out, _) = run("readonly X=fixed; echo $X");
        assert_eq!(out, "fixed\n");
    }

    #[test]
    fn readonly_propagates_through_function() {
        let prog = parse("readonly X=fixed; foo() { X=changed; }; foo").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert!(matches!(err, KashError::Readonly(_)));
    }

    #[test]
    fn unset_removes_binding() {
        let (_, _, ev) = run("X=foo; unset X");
        assert!(ev.scope().get("X").is_none());
    }

    #[test]
    fn unset_refuses_readonly() {
        let prog = parse("readonly X=v; unset X").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert!(matches!(err, KashError::Readonly(_)));
    }

    #[test]
    fn ksh_function_definition_callable() {
        let (_, out, _) = run("function f { echo k; }; f");
        assert_eq!(out, "k\n");
    }

    #[test]
    fn function_recursion_via_positionals() {
        // No `[`/`test` builtin yet, so route the bounded recursion
        // through `case` instead.
        let (_, out, _) = run(
            "rec() { echo $1; case $1 in 0) :;; 1) rec 0;; 2) rec 1;; esac; }; rec 2",
        );
        assert_eq!(out, "2\n1\n0\n");
    }

    // ===== [[ ... ]] extended test + regex + extglob =====

    #[test]
    fn double_bracket_string_equality() {
        let (_, _, _) = run("[[ foo == foo ]]");
        let (o, _, _) = run("[[ foo == foo ]]");
        assert_eq!(o.status(), 0);
        let (o, _, _) = run("[[ foo == bar ]]");
        assert_eq!(o.status(), 1);
    }

    #[test]
    fn double_bracket_glob_pattern_match() {
        let (o, _, _) = run("[[ foobar == foo* ]]");
        assert_eq!(o.status(), 0);
        let (o, _, _) = run("[[ baz == foo* ]]");
        assert_eq!(o.status(), 1);
        let (o, _, _) = run("[[ baz != foo* ]]");
        assert_eq!(o.status(), 0);
    }

    #[test]
    fn double_bracket_unary_predicates() {
        let (o, _, _) = run("[[ -z '' ]]");
        assert_eq!(o.status(), 0);
        let (o, _, _) = run("[[ -n foo ]]");
        assert_eq!(o.status(), 0);
    }

    #[test]
    fn double_bracket_negation_and_short_circuit() {
        let (o, _, _) = run("[[ ! foo == bar ]]");
        assert_eq!(o.status(), 0);
        let (o, _, _) = run("[[ foo == foo && 1 -lt 2 ]]");
        assert_eq!(o.status(), 0);
        let (o, _, _) = run("[[ foo == foo || foo == bar ]]");
        assert_eq!(o.status(), 0);
        let (o, _, _) = run("[[ foo == bar && foo == foo ]]");
        assert_eq!(o.status(), 1);
    }

    #[test]
    fn double_bracket_lexical_compare() {
        let (o, _, _) = run("[[ apple < banana ]]");
        assert_eq!(o.status(), 0);
        let (o, _, _) = run("[[ banana < apple ]]");
        assert_eq!(o.status(), 1);
    }

    #[test]
    fn double_bracket_drives_if() {
        let (_, out, _) = run(
            "X=hello; if [[ $X == h*o ]]; then echo yep; else echo nope; fi",
        );
        assert_eq!(out, "yep\n");
    }

    #[test]
    fn double_bracket_regex_match() {
        let (o, _, _) = run("[[ hello =~ ^h.l ]]");
        assert_eq!(o.status(), 0);
        let (o, _, _) = run("[[ hello =~ x.*y ]]");
        assert_eq!(o.status(), 1);
    }

    #[test]
    fn double_bracket_regex_anchors_and_classes() {
        let (o, _, _) = run("[[ abc123 =~ ^[a-z]+[0-9]+$ ]]");
        assert_eq!(o.status(), 0);
        let (o, _, _) = run("[[ abc =~ ^[a-z]+[0-9]+$ ]]");
        assert_eq!(o.status(), 1);
    }

    #[test]
    fn double_bracket_regex_repetition() {
        let (o, _, _) = run("[[ aaaa =~ a+ ]]");
        assert_eq!(o.status(), 0);
        let (o, _, _) = run("[[ '' =~ a* ]]");
        assert_eq!(o.status(), 0);
        let (o, _, _) = run("[[ abc =~ a?b?c? ]]");
        assert_eq!(o.status(), 0);
    }

    // ===== extglob =====

    #[test]
    fn extglob_question_zero_or_one() {
        let (_, out, _) = run("X=color; case $X in colo?(u)r) echo hit;; *) echo miss;; esac");
        assert_eq!(out, "hit\n");
        let (_, out, _) = run("X=colour; case $X in colo?(u)r) echo hit;; *) echo miss;; esac");
        assert_eq!(out, "hit\n");
        let (_, out, _) = run("X=coloUr; case $X in colo?(u)r) echo hit;; *) echo miss;; esac");
        assert_eq!(out, "miss\n");
    }

    #[test]
    fn extglob_plus_one_or_more() {
        let (_, out, _) = run("X=aaa; case $X in +(a)) echo hit;; *) echo miss;; esac");
        assert_eq!(out, "hit\n");
        let (_, out, _) = run("X=''; case $X in +(a)) echo hit;; *) echo miss;; esac");
        assert_eq!(out, "miss\n");
    }

    #[test]
    fn extglob_star_zero_or_more() {
        let (_, out, _) = run("X=''; case $X in *(a)) echo hit;; esac");
        assert_eq!(out, "hit\n");
        let (_, out, _) = run("X=aaaa; case $X in *(a)) echo hit;; esac");
        assert_eq!(out, "hit\n");
    }

    #[test]
    fn extglob_at_exactly_one() {
        let (_, out, _) = run("X=apple; case $X in @(apple|orange)) echo fruit;; *) echo other;; esac");
        assert_eq!(out, "fruit\n");
        let (_, out, _) = run("X=banana; case $X in @(apple|orange)) echo fruit;; *) echo other;; esac");
        assert_eq!(out, "other\n");
    }

    #[test]
    fn extglob_bang_anything_except() {
        let (_, out, _) = run("X=foo; case $X in !(bar)) echo not_bar;; esac");
        assert_eq!(out, "not_bar\n");
        let (_, out, _) = run("X=bar; case $X in !(bar)) echo not_bar;; *) echo bar;; esac");
        assert_eq!(out, "bar\n");
    }

    // ===== xtrace (-x / set -o xtrace) =====

    #[test]
    fn xtrace_emits_command_to_trace_buffer() {
        let prog = parse("set -x; echo hi").unwrap();
        let mut ev = Evaluator::new();
        ev.eval_program(&prog).unwrap();
        let trace = ev.take_trace_output();
        assert!(trace.contains("+ echo hi"), "got: {trace:?}");
        assert_eq!(ev.take_output(), "hi\n");
    }

    #[test]
    fn xtrace_off_after_plus_x() {
        let prog = parse("set -x; echo a; set +x; echo b").unwrap();
        let mut ev = Evaluator::new();
        ev.eval_program(&prog).unwrap();
        let trace = ev.take_trace_output();
        assert!(trace.contains("+ echo a"));
        assert!(!trace.contains("+ echo b"), "trace = {trace:?}");
    }

    #[test]
    fn xtrace_traces_every_command_including_builtins() {
        let prog = parse("set -x; X=1; true; echo done").unwrap();
        let mut ev = Evaluator::new();
        ev.eval_program(&prog).unwrap();
        let trace = ev.take_trace_output();
        // `X=1` is an assignment-only command with no words; nothing
        // to trace. `true` and `echo done` should show up.
        assert!(trace.contains("+ true"), "got: {trace:?}");
        assert!(trace.contains("+ echo done"), "got: {trace:?}");
    }

    #[test]
    fn xtrace_honours_custom_ps4() {
        let prog = parse("PS4='> '; set -x; echo go").unwrap();
        let mut ev = Evaluator::new();
        ev.eval_program(&prog).unwrap();
        let trace = ev.take_trace_output();
        assert!(trace.contains("> echo go"), "got: {trace:?}");
    }

    #[test]
    fn xtrace_via_set_o_xtrace() {
        let prog = parse("set -o xtrace; echo on; set +o xtrace; echo off").unwrap();
        let mut ev = Evaluator::new();
        ev.eval_program(&prog).unwrap();
        let trace = ev.take_trace_output();
        assert!(trace.contains("+ echo on"));
        assert!(!trace.contains("+ echo off"));
    }

    // ===== alias / unalias =====

    #[test]
    fn alias_substitutes_first_word() {
        let (_, out, _) = run("alias greet='echo hello'; greet");
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn alias_preserves_trailing_args() {
        let (_, out, _) = run("alias say='echo hi'; say world");
        assert_eq!(out, "hi world\n");
    }

    #[test]
    fn alias_chains_through_other_aliases() {
        let (_, out, _) = run("alias a='echo first'; alias b=a; b");
        assert_eq!(out, "first\n");
    }

    #[test]
    fn alias_self_reference_terminates() {
        // `alias true=true` would loop forever without the seen-set
        // guard.
        let (o, _, _) = run("alias true=true; true");
        assert_eq!(o, Outcome::Status(0));
    }

    #[test]
    fn unalias_removes_entry() {
        let prog = parse("alias foo='echo hi'; unalias foo; foo").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert_eq!(err.exit_code(), 127);
    }

    #[test]
    fn unalias_a_removes_everything() {
        let (_, _, ev) = run("alias x=y; alias p=q; unalias -a");
        assert!(ev.aliases_for_test().is_empty());
    }

    #[test]
    fn alias_listing_emits_quoted_form() {
        let (_, out, _) = run("alias greet='echo hi'; alias");
        assert!(out.contains("alias greet='echo hi'"), "got: {out:?}");
    }

    // ===== trap / exit handler =====

    #[test]
    fn exit_trap_fires_on_program_end() {
        let (_, out, _) = run("trap 'echo bye' EXIT; echo hi");
        assert_eq!(out, "hi\nbye\n");
    }

    #[test]
    fn exit_trap_fires_on_exit_request() {
        let (o, out, _) = run("trap 'echo cleanup' EXIT; exit 2");
        assert_eq!(o, Outcome::Exit(2));
        assert_eq!(out, "cleanup\n");
    }

    #[test]
    fn err_trap_fires_on_failed_command() {
        let (_, out, _) = run("trap 'echo trap_fired' ERR; false");
        assert_eq!(out, "trap_fired\n");
    }

    #[test]
    fn err_trap_does_not_fire_in_condition() {
        let (_, out, _) = run("trap 'echo trap_fired' ERR; if false; then :; fi; echo done");
        assert_eq!(out, "done\n");
    }

    #[test]
    fn trap_reset_with_dash_removes_handler() {
        let (_, out, _) = run("trap 'echo a' EXIT; trap - EXIT; echo done");
        assert_eq!(out, "done\n");
    }

    #[test]
    fn trap_listing_emits_registered_handlers() {
        let (_, out, _) = run("trap 'echo bye' EXIT; trap 'echo err' ERR; trap");
        assert!(out.contains("trap -- 'echo bye' EXIT"), "got: {out:?}");
        assert!(out.contains("trap -- 'echo err' ERR"), "got: {out:?}");
    }

    #[test]
    fn trap_sig_prefix_normalised() {
        let (_, out, _) = run("trap 'echo got' SIGINT; trap");
        // The SIG prefix is stripped — the listing shows just `INT`.
        assert!(out.contains(" INT\n"), "got: {out:?}");
    }

    #[test]
    fn trap_does_not_recurse_on_itself() {
        // ERR trap calling `false` would otherwise infinitely recurse.
        let (_, out, _) = run("trap 'echo err; false' ERR; false");
        assert_eq!(out, "err\n");
    }

    // ===== set options: errexit / nounset / pipefail =====

    #[test]
    fn errexit_aborts_on_first_failure() {
        let (o, out, _) = run("set -e; echo a; false; echo b");
        // 'echo a' prints, then `false` returns 1 and -e fires.
        assert_eq!(o, Outcome::Exit(1));
        assert_eq!(out, "a\n");
    }

    #[test]
    fn errexit_off_does_not_abort() {
        let (_, out, _) = run("echo a; false; echo b");
        assert_eq!(out, "a\nb\n");
    }

    #[test]
    fn errexit_suppressed_in_if_condition() {
        // `false` in an `if` condition must not trip -e.
        let (o, out, _) = run("set -e; if false; then echo a; else echo b; fi; echo done");
        assert_eq!(o.status(), 0);
        assert_eq!(out, "b\ndone\n");
    }

    #[test]
    fn errexit_suppressed_in_while_condition() {
        // The cond that finally returns non-zero stops the loop but
        // doesn't trip -e.
        let (_, out, _) = run(
            "set -e; N=0; while case $N in 0) false;; *) true;; esac; do echo run; N=1; done; echo done",
        );
        assert_eq!(out, "done\n");
    }

    #[test]
    fn nounset_errors_on_plain_dollar_var() {
        let prog = parse("set -u; echo $NOPE").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert!(format!("{err}").contains("not set"), "got: {err}");
    }

    #[test]
    fn nounset_does_not_error_on_default_modifier() {
        let (_, out, _) = run("set -u; echo ${NOPE:-fallback}");
        assert_eq!(out, "fallback\n");
    }

    #[test]
    fn nounset_does_not_error_on_set_var() {
        let (_, out, _) = run("set -u; X=hi; echo $X");
        assert_eq!(out, "hi\n");
    }

    #[test]
    fn set_o_named_options_toggle() {
        let (_, _, ev) = run("set -o errexit; set -o nounset; set -o pipefail");
        let opts = ev.options();
        assert!(opts.errexit);
        assert!(opts.nounset);
        assert!(opts.pipefail);
    }

    #[test]
    fn plus_o_disables_named_options() {
        let (_, _, ev) = run("set -e -u; set +e +u");
        let opts = ev.options();
        assert!(!opts.errexit);
        assert!(!opts.nounset);
    }

    #[test]
    fn set_unknown_option_errors() {
        let prog = parse("set -o nosuchoption").unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_err());
    }

    #[cfg(feature = "std")]
    #[test]
    fn pipefail_picks_up_first_stage_failure() {
        use std::path::Path;
        if !Path::new("/bin/false").exists() || !Path::new("/bin/cat").exists() {
            return;
        }
        // Without pipefail the pipeline's status is /bin/cat's (0).
        let prog = parse("/bin/false | /bin/cat").unwrap();
        let mut ev = Evaluator::new();
        assert_eq!(ev.eval_program(&prog).unwrap().status(), 0);
        // With pipefail, the upstream non-zero is reported.
        let prog = parse("set -o pipefail; /bin/false | /bin/cat").unwrap();
        let mut ev = Evaluator::new();
        assert_ne!(ev.eval_program(&prog).unwrap().status(), 0);
    }

    // ===== fd-prefixed redirects + fd dups =====

    #[cfg(feature = "std")]
    mod fd_redirect_tests {
        use super::*;
        use std::fs;
        use std::path::{Path, PathBuf};

        fn have(p: &str) -> bool {
            Path::new(p).exists()
        }

        fn tmp(name: &str) -> PathBuf {
            let mut p = std::env::temp_dir();
            p.push(alloc::format!(
                "kash-fd-{}-{}",
                std::process::id(),
                name
            ));
            p
        }

        #[test]
        fn fd_prefix_2_redirects_stderr() {
            // /bin/sh -c 'echo err 1>&2' writes "err" to stderr.
            // Redirecting fd 2 to a file should capture it; stdout
            // should be empty.
            if !have("/bin/sh") {
                return;
            }
            let path = tmp("a.err");
            let src = alloc::format!(
                "/bin/sh -c 'echo err 1>&2' 2> {}",
                path.display()
            );
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert!(ev.take_output().is_empty());
            assert_eq!(fs::read_to_string(&path).unwrap(), "err\n");
            let _ = fs::remove_file(&path);
        }

        #[test]
        fn stderr_to_stdout_dup_then_file() {
            if !have("/bin/sh") {
                return;
            }
            // `cmd > file 2>&1` — both streams routed to `file`.
            let path = tmp("b.both");
            let src = alloc::format!(
                "/bin/sh -c 'echo out; echo err 1>&2' > {} 2>&1",
                path.display()
            );
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            let body = fs::read_to_string(&path).unwrap();
            assert!(body.contains("out\n"), "got: {body:?}");
            assert!(body.contains("err\n"), "got: {body:?}");
            let _ = fs::remove_file(&path);
        }

        #[test]
        fn fd_prefix_1_explicit_stdout_redirect() {
            if !have("/bin/echo") {
                return;
            }
            let path = tmp("c.out");
            let src = alloc::format!("/bin/echo explicit 1> {}", path.display());
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "explicit\n");
            let _ = fs::remove_file(&path);
        }

        #[test]
        fn fd_prefix_2_append() {
            if !have("/bin/sh") {
                return;
            }
            let path = tmp("d.append");
            fs::write(&path, "previous\n").unwrap();
            let src = alloc::format!(
                "/bin/sh -c 'echo err 1>&2' 2>> {}",
                path.display()
            );
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "previous\nerr\n");
            let _ = fs::remove_file(&path);
        }
    }

    // ===== glob enhancements =====

    #[test]
    fn glob_backslash_escapes_meta() {
        // `\*` matches a literal `*`.
        let (_, out, _) = run("X='*'; case $X in '\\*') echo lit;; *) echo other;; esac");
        assert_eq!(out, "lit\n");
        // And the literal star does NOT match a non-star.
        let (_, out, _) = run("X=abc; case $X in '\\*') echo lit;; *) echo other;; esac");
        assert_eq!(out, "other\n");
    }

    #[test]
    fn glob_posix_class_alpha() {
        let (_, out, _) = run("X=q; case $X in [[:alpha:]]) echo letter;; esac");
        assert_eq!(out, "letter\n");
    }

    #[test]
    fn glob_posix_class_digit() {
        let (_, out, _) = run("X=5; case $X in [[:digit:]]) echo digit;; esac");
        assert_eq!(out, "digit\n");
        let (_, out, _) = run("X=q; case $X in [[:digit:]]) echo digit;; *) echo other;; esac");
        assert_eq!(out, "other\n");
    }

    #[test]
    fn glob_posix_class_combined_with_literals() {
        // `[[:alpha:]0]` matches letter or `0`.
        let (_, out, _) = run("X=0; case $X in [[:alpha:]0]) echo hit;; esac");
        assert_eq!(out, "hit\n");
        let (_, out, _) = run("X=a; case $X in [[:alpha:]0]) echo hit;; esac");
        assert_eq!(out, "hit\n");
        let (_, out, _) = run("X=9; case $X in [[:alpha:]0]) echo hit;; *) echo nope;; esac");
        assert_eq!(out, "nope\n");
    }

    #[test]
    fn glob_negated_class_with_posix() {
        let (_, out, _) = run("X=q; case $X in [![:digit:]]) echo not_digit;; esac");
        assert_eq!(out, "not_digit\n");
    }

    #[test]
    fn glob_xdigit_class() {
        for ch in ["0", "9", "a", "f", "A", "F"] {
            let src = alloc::format!(
                "X={ch}; case $X in [[:xdigit:]]) echo hex;; *) echo no;; esac"
            );
            let (_, out, _) = run(&src);
            assert_eq!(out, "hex\n", "ch = {ch}");
        }
        let (_, out, _) = run("X=g; case $X in [[:xdigit:]]) echo hex;; *) echo no;; esac");
        assert_eq!(out, "no\n");
    }

    #[test]
    fn glob_leading_close_bracket_in_class() {
        // `[]abc]` includes `]` as a member (POSIX rule).
        let (_, out, _) = run("X=']'; case $X in []abc]) echo hit;; esac");
        assert_eq!(out, "hit\n");
    }

    // ===== here-doc / here-string =====

    #[cfg(feature = "std")]
    mod heredoc_tests {
        use super::*;
        use std::path::Path;

        fn have(p: &str) -> bool {
            Path::new(p).exists()
        }

        #[test]
        fn here_string_feeds_external_stdin() {
            if !have("/bin/cat") {
                return;
            }
            let prog = parse("/bin/cat <<<hello").unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "hello\n");
        }

        #[test]
        fn here_string_expands_dollar_var() {
            if !have("/bin/cat") {
                return;
            }
            let prog = parse("X=world; /bin/cat <<<\"hi $X\"").unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "hi world\n");
        }

        #[test]
        fn here_doc_feeds_external_stdin() {
            if !have("/bin/cat") {
                return;
            }
            let src = "/bin/cat <<EOF\nline one\nline two\nEOF\n";
            let prog = parse(src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "line one\nline two\n");
        }

        #[test]
        fn here_doc_expands_dollar_var_by_default() {
            if !have("/bin/cat") {
                return;
            }
            let src = "X=world; /bin/cat <<EOF\nhi $X\nEOF\n";
            let prog = parse(src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "hi world\n");
        }

        #[test]
        fn here_doc_with_quoted_delim_is_verbatim() {
            if !have("/bin/cat") {
                return;
            }
            // Single-quoted delimiter disables expansion.
            let src = "X=world; /bin/cat <<'EOF'\nhi $X\nEOF\n";
            let prog = parse(src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "hi $X\n");
        }

        #[test]
        fn here_doc_dash_strips_leading_tabs() {
            if !have("/bin/cat") {
                return;
            }
            let src = "/bin/cat <<-EOF\n\t\tindented\n\tmid\nEOF\n";
            let prog = parse(src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "indented\nmid\n");
        }

        #[test]
        fn here_doc_unterminated_errors() {
            // No closing `EOF` line — should fail at parse time.
            let res = parse("/bin/cat <<EOF\nbody\n");
            assert!(res.is_err());
        }
    }

    // ===== arithmetic expansion =====

    #[test]
    fn arith_basic_add() {
        let (_, out, _) = run("echo $((1 + 2))");
        assert_eq!(out, "3\n");
    }

    #[test]
    fn arith_precedence() {
        let (_, out, _) = run("echo $((2 + 3 * 4))");
        assert_eq!(out, "14\n");
        let (_, out, _) = run("echo $(((2 + 3) * 4))");
        assert_eq!(out, "20\n");
    }

    #[test]
    fn arith_division_and_modulo() {
        let (_, out, _) = run("echo $((10 / 3))");
        assert_eq!(out, "3\n");
        let (_, out, _) = run("echo $((10 % 3))");
        assert_eq!(out, "1\n");
    }

    #[test]
    fn arith_unary_minus_and_negation() {
        let (_, out, _) = run("echo $((-5))");
        assert_eq!(out, "-5\n");
        let (_, out, _) = run("echo $((!0))");
        assert_eq!(out, "1\n");
        let (_, out, _) = run("echo $((!7))");
        assert_eq!(out, "0\n");
    }

    #[test]
    fn arith_comparisons() {
        let (_, out, _) = run("echo $((3 < 5)) $((3 > 5)) $((5 == 5)) $((5 != 5))");
        assert_eq!(out, "1 0 1 0\n");
    }

    #[test]
    fn arith_logical_ops() {
        let (_, out, _) = run("echo $((1 && 0)) $((1 && 1)) $((0 || 0)) $((0 || 3))");
        assert_eq!(out, "0 1 0 1\n");
    }

    #[test]
    fn arith_reads_bare_name_from_scope() {
        let (_, out, _) = run("N=5; echo $((N + 1))");
        assert_eq!(out, "6\n");
    }

    #[test]
    fn arith_reads_dollar_var_from_scope() {
        let (_, out, _) = run("N=5; echo $(($N + 1))");
        assert_eq!(out, "6\n");
    }

    #[test]
    fn arith_unset_var_is_zero() {
        let (_, out, _) = run("echo $((MISSING + 7))");
        assert_eq!(out, "7\n");
    }

    #[test]
    fn arith_non_numeric_var_errors() {
        let prog = parse("X=hello; echo $((X + 1))").unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_err());
    }

    #[test]
    fn arith_divide_by_zero_errors() {
        let prog = parse("echo $((1 / 0))").unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_err());
    }

    #[test]
    fn arith_drives_for_loop_counter() {
        let (_, out, _) = run(
            "N=3; while [ $N -gt 0 ]; do echo $N; N=$((N - 1)); done",
        );
        assert_eq!(out, "3\n2\n1\n");
    }

    // ===== $@ / $* quoting semantics =====

    fn run_with_args(src: &str, args: &[&str]) -> (Outcome, String, Evaluator) {
        let prog = parse(src).expect("parse");
        let mut ev = Evaluator::new();
        ev.positionals = args.iter().map(|s| (*s).into()).collect();
        let outcome = ev.eval_program(&prog).expect("eval");
        let out = ev.take_output();
        (outcome, out, ev)
    }

    #[test]
    fn unquoted_dollar_at_splits_into_fields() {
        let (_, out, _) = run_with_args(
            "for x in $@; do echo $x; done",
            &["one", "two three", "four"],
        );
        // "two three" gets IFS-split → "two" and "three".
        assert_eq!(out, "one\ntwo\nthree\nfour\n");
    }

    #[test]
    fn quoted_dollar_at_preserves_each_positional() {
        let (_, out, _) = run_with_args(
            "for x in \"$@\"; do echo $x; done",
            &["one", "two three", "four"],
        );
        // Quoted "$@" keeps each positional intact.
        assert_eq!(out, "one\ntwo three\nfour\n");
    }

    #[test]
    fn quoted_dollar_star_joins_with_first_ifs_char() {
        let (_, out, _) = run_with_args(
            "for x in \"$*\"; do echo $x; done",
            &["one", "two", "three"],
        );
        // "$*" is a single field made from joining positionals with
        // the first character of IFS (default ' ').
        assert_eq!(out, "one two three\n");
    }

    #[test]
    fn custom_ifs_changes_dollar_star_join() {
        // `echo $x` would IFS-split the iteration variable again, so
        // quote it to see the unsplit joined string from "$*".
        let (_, out, _) = run_with_args(
            "IFS=,; for x in \"$*\"; do echo \"$x\"; done",
            &["a", "b", "c"],
        );
        assert_eq!(out, "a,b,c\n");
    }

    #[test]
    fn dollar_at_inside_concatenated_word() {
        let (_, out, _) = run_with_args(
            "for x in \"prefix $@ suffix\"; do echo $x; done",
            &["a", "b", "c"],
        );
        // POSIX: first positional folds into the prefix, last folds
        // into the suffix, middle ones are their own fields.
        assert_eq!(out, "prefix a\nb\nc suffix\n");
    }

    #[test]
    fn empty_quoted_dollar_at_emits_nothing() {
        let (_, out, _) = run_with_args("echo before \"$@\" after", &[]);
        // Empty positionals → "$@" expands to no fields at all, so
        // echo sees just "before" and "after".
        assert_eq!(out, "before after\n");
    }

    #[test]
    fn dollar_hash_reflects_argc() {
        let (_, out, _) = run_with_args("echo $#", &["a", "b", "c"]);
        assert_eq!(out, "3\n");
    }

    // ===== arithmetic extensions =====

    #[test]
    fn arith_octal_and_hex_literals() {
        let (_, out, _) = run("echo $((010)) $((0x10)) $((0xff))");
        assert_eq!(out, "8 16 255\n");
    }

    #[test]
    fn arith_bitwise_ops() {
        let (_, out, _) = run("echo $((5 & 3)) $((5 | 3)) $((5 ^ 3)) $((~0))");
        assert_eq!(out, "1 7 6 -1\n");
    }

    #[test]
    fn arith_shift_ops() {
        let (_, out, _) = run("echo $((1 << 4)) $((256 >> 3))");
        assert_eq!(out, "16 32\n");
    }

    #[test]
    fn arith_ternary() {
        let (_, out, _) = run("echo $((1 < 2 ? 10 : 20)) $((1 > 2 ? 10 : 20))");
        assert_eq!(out, "10 20\n");
    }

    #[test]
    fn arith_assign_persists_in_scope() {
        let (_, _, ev) = run(": $((X = 7))");
        assert_eq!(ev.scope().get("X").unwrap().to_scalar_string(), "7");
    }

    #[test]
    fn arith_assign_returns_value() {
        let (_, out, _) = run("echo $((X = 7))");
        assert_eq!(out, "7\n");
    }

    #[test]
    fn arith_compound_assign() {
        let (_, out, ev) = run("X=10; echo $((X += 3)); echo $((X *= 2))");
        assert_eq!(out, "13\n26\n");
        assert_eq!(ev.scope().get("X").unwrap().to_scalar_string(), "26");
    }

    #[test]
    fn arith_pre_increment() {
        let (_, out, ev) = run("X=5; echo $((++X)); echo $X");
        assert_eq!(out, "6\n6\n");
        assert_eq!(ev.scope().get("X").unwrap().to_scalar_string(), "6");
    }

    #[test]
    fn arith_post_increment() {
        let (_, out, ev) = run("X=5; echo $((X++)); echo $X");
        assert_eq!(out, "5\n6\n");
        assert_eq!(ev.scope().get("X").unwrap().to_scalar_string(), "6");
    }

    #[test]
    fn arith_pre_decrement() {
        let (_, out, ev) = run("X=5; echo $((--X))");
        assert_eq!(out, "4\n");
        assert_eq!(ev.scope().get("X").unwrap().to_scalar_string(), "4");
    }

    #[test]
    fn arith_drives_counter_with_compound_assign() {
        let (_, out, _) = run(
            "N=3; while [ $N -gt 0 ]; do echo $N; : $((N -= 1)); done",
        );
        assert_eq!(out, "3\n2\n1\n");
    }

    #[test]
    fn arith_chained_assignment() {
        let (_, _, ev) = run(": $((X = Y = 5))");
        assert_eq!(ev.scope().get("X").unwrap().to_scalar_string(), "5");
        assert_eq!(ev.scope().get("Y").unwrap().to_scalar_string(), "5");
    }

    // ===== command substitution =====

    #[test]
    fn dollar_paren_substitution_basic() {
        let (_, out, _) = run("echo $(echo hi)");
        assert_eq!(out, "hi\n");
    }

    #[test]
    fn backtick_substitution_basic() {
        let (_, out, _) = run("echo `echo hi`");
        assert_eq!(out, "hi\n");
    }

    #[test]
    fn substitution_strips_trailing_newlines() {
        let (_, out, _) = run("X=$(echo hi); echo [$X]");
        assert_eq!(out, "[hi]\n");
    }

    #[test]
    fn substitution_in_double_quotes_preserves_content() {
        let (_, out, _) = run("echo \"$(echo one two)\"");
        // Inside `"..."`, splitting doesn't fire, so the spaces in
        // `one two` survive into a single arg.
        assert_eq!(out, "one two\n");
    }

    #[test]
    fn substitution_unquoted_splits_on_ifs() {
        let (_, out, _) = run("for w in $(echo a b c); do echo $w; done");
        assert_eq!(out, "a\nb\nc\n");
    }

    #[test]
    fn substitution_runs_in_subshell() {
        // Assignments inside the substitution body must not leak.
        let (_, _, ev) = run("Y=$(X=inner; echo $X)");
        assert!(ev.scope().get("X").is_none());
        assert_eq!(ev.scope().get("Y").unwrap().to_scalar_string(), "inner");
    }

    #[test]
    fn nested_dollar_paren_substitution() {
        let (_, out, _) = run("echo $(echo $(echo hi))");
        assert_eq!(out, "hi\n");
    }

    #[test]
    fn substitution_in_assignment_rhs() {
        let (_, _, ev) = run("X=$(echo computed); :");
        assert_eq!(ev.scope().get("X").unwrap().to_scalar_string(), "computed");
    }

    #[test]
    fn substitution_propagates_runtime_error() {
        let prog = parse("X=$(false; nope_not_a_real_cmd_xyzzy)").unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_err());
    }

    // ===== IFS field splitting =====

    #[test]
    fn unquoted_expansion_splits_on_default_ifs() {
        let (_, out, _) = run("X='a b c'; for w in $X; do echo $w; done");
        assert_eq!(out, "a\nb\nc\n");
    }

    #[test]
    fn quoted_expansion_does_not_split() {
        let (_, out, _) = run("X='a b c'; for w in \"$X\"; do echo $w; done");
        assert_eq!(out, "a b c\n");
    }

    #[test]
    fn double_quoted_dollar_keeps_internal_spaces() {
        let (_, out, _) = run("X='multi word'; echo \"$X\"");
        assert_eq!(out, "multi word\n");
    }

    #[test]
    fn argv_field_split_passes_three_args() {
        let (_, out, _) = run("X='a b c'; echo $X");
        // `echo` with 3 args joined with one space.
        assert_eq!(out, "a b c\n");
    }

    #[test]
    fn custom_ifs_splits_on_comma() {
        let (_, out, _) = run("IFS=,; X=a,b,c; for w in $X; do echo $w; done");
        assert_eq!(out, "a\nb\nc\n");
    }

    #[test]
    fn empty_unquoted_expansion_yields_no_field() {
        // The empty `$X` between `cat` and `dog` should disappear so
        // we end up calling echo with two args.
        let (_, out, _) = run("X=; echo cat $X dog");
        assert_eq!(out, "cat dog\n");
    }

    #[test]
    fn empty_quoted_expansion_keeps_a_field() {
        // `"$X"` is a single (empty) field even when X is empty, so
        // echo sees three args.
        let (_, out, _) = run("X=; echo cat \"$X\" dog");
        assert_eq!(out, "cat  dog\n");
    }

    #[test]
    fn assignment_value_does_not_split() {
        let (_, _, ev) = run("Y='one two three'; X=$Y");
        assert_eq!(ev.scope().get("X").unwrap().to_scalar_string(), "one two three");
    }

    // ===== test / [ =====

    #[test]
    fn test_empty_args_is_false() {
        let (o, _, _) = run("test");
        assert_eq!(o.status(), 1);
        let (o, _, _) = run("[ ]");
        assert_eq!(o.status(), 1);
    }

    #[test]
    fn test_single_arg_truth() {
        assert_eq!(run("test foo").0.status(), 0);
        assert_eq!(run("[ foo ]").0.status(), 0);
    }

    #[test]
    fn test_z_n_unary() {
        assert_eq!(run("test -z ''").0.status(), 0);
        assert_eq!(run("test -z foo").0.status(), 1);
        assert_eq!(run("test -n foo").0.status(), 0);
        assert_eq!(run("test -n ''").0.status(), 1);
    }

    #[test]
    fn test_string_equality() {
        assert_eq!(run("[ foo = foo ]").0.status(), 0);
        assert_eq!(run("[ foo = bar ]").0.status(), 1);
        assert_eq!(run("[ foo != bar ]").0.status(), 0);
    }

    #[test]
    fn test_integer_comparisons() {
        assert_eq!(run("[ 3 -eq 3 ]").0.status(), 0);
        assert_eq!(run("[ 3 -ne 4 ]").0.status(), 0);
        assert_eq!(run("[ 3 -lt 4 ]").0.status(), 0);
        assert_eq!(run("[ 4 -le 4 ]").0.status(), 0);
        assert_eq!(run("[ 5 -gt 4 ]").0.status(), 0);
        assert_eq!(run("[ 4 -ge 4 ]").0.status(), 0);
        assert_eq!(run("[ 3 -gt 4 ]").0.status(), 1);
    }

    #[test]
    fn test_bang_negation() {
        assert_eq!(run("[ ! -z foo ]").0.status(), 0);
        assert_eq!(run("[ ! foo = bar ]").0.status(), 0);
        assert_eq!(run("[ ! foo = foo ]").0.status(), 1);
    }

    #[test]
    fn test_used_in_if() {
        let (_, out, _) = run("if [ -z '' ]; then echo empty; else echo full; fi");
        assert_eq!(out, "empty\n");
    }

    #[test]
    fn test_drives_while_loop() {
        // No `$((…))` arithmetic yet; cascade `if/elif` to step the
        // counter manually so the test exercises the `[ … ]` driver.
        let (_, out, _) = run(
            "N=3; while [ $N -ne 0 ]; do echo $N; if [ $N -eq 3 ]; then N=2; elif [ $N -eq 2 ]; then N=1; else N=0; fi; done",
        );
        assert_eq!(out, "3\n2\n1\n");
    }

    // ===== redirects (env-dependent) =====

    #[cfg(feature = "std")]
    mod redirect_tests {
        use super::*;
        use std::fs;
        use std::io::Write;
        use std::path::PathBuf;

        fn tmp_path(name: &str) -> PathBuf {
            let mut p = std::env::temp_dir();
            // Add a per-process suffix so parallel test runs don't collide.
            p.push(alloc::format!("kash-test-{}-{}", std::process::id(), name));
            p
        }

        #[test]
        fn builtin_output_redirect_writes_to_file() {
            let path = tmp_path("a");
            let src = alloc::format!("echo hello > {}", path.display());
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert!(ev.take_output().is_empty(), "stdout should have been redirected");
            let body = fs::read_to_string(&path).unwrap();
            assert_eq!(body, "hello\n");
            let _ = fs::remove_file(&path);
        }

        #[test]
        fn builtin_append_redirect_concatenates() {
            let path = tmp_path("b");
            {
                let mut f = fs::File::create(&path).unwrap();
                f.write_all(b"first\n").unwrap();
            }
            let src = alloc::format!("echo second >> {}", path.display());
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "first\nsecond\n");
            let _ = fs::remove_file(&path);
        }

        #[test]
        fn no_command_redirect_truncates_file() {
            let path = tmp_path("c");
            fs::write(&path, "previous\n").unwrap();
            let src = alloc::format!("> {}", path.display());
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "");
            let _ = fs::remove_file(&path);
        }

        #[test]
        fn input_redirect_feeds_external_command() {
            let path = tmp_path("d");
            fs::write(&path, "piped via file\n").unwrap();
            if !std::path::Path::new("/bin/cat").exists() {
                let _ = fs::remove_file(&path);
                return;
            }
            let src = alloc::format!("/bin/cat < {}", path.display());
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "piped via file\n");
            let _ = fs::remove_file(&path);
        }

        #[test]
        fn external_output_redirect_writes_to_file() {
            let path = tmp_path("e");
            if !std::path::Path::new("/bin/echo").exists() {
                return;
            }
            let src = alloc::format!("/bin/echo external > {}", path.display());
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert!(ev.take_output().is_empty());
            assert_eq!(fs::read_to_string(&path).unwrap(), "external\n");
            let _ = fs::remove_file(&path);
        }

        #[test]
        fn missing_input_file_errors() {
            let path = tmp_path("does-not-exist");
            let _ = fs::remove_file(&path);
            let src = alloc::format!("echo hi < {}", path.display());
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            assert!(ev.eval_program(&prog).is_err());
        }
    }

    // ===== multistage pipeline + external exec (env-dependent) =====

    #[cfg(not(feature = "std"))]
    #[test]
    fn multistage_pipeline_unsupported_in_alloc_only() {
        let prog = parse("echo a | true").unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_err());
    }

    #[cfg(not(feature = "std"))]
    #[test]
    fn external_command_unknown_in_alloc_only() {
        let prog = parse("definitely_not_a_real_command").unwrap();
        let mut ev = Evaluator::new();
        assert_eq!(
            ev.eval_program(&prog).unwrap_err().exit_code(),
            127
        );
    }

    #[cfg(feature = "std")]
    mod std_tests {
        use super::*;
        use std::path::Path;

        /// Skip the test if the named binary isn't on the dev host
        /// (some sandboxes / minimal images don't ship `/bin/echo`
        /// etc.). Returns `true` if the binary exists.
        fn have(p: &str) -> bool {
            Path::new(p).exists()
        }

        #[test]
        fn external_echo_captures_stdout() {
            if !have("/bin/echo") {
                return;
            }
            let prog = parse("/bin/echo hello world").unwrap();
            let mut ev = Evaluator::new();
            let o = ev.eval_program(&prog).unwrap();
            assert_eq!(o, Outcome::Status(0));
            assert_eq!(ev.take_output(), "hello world\n");
        }

        #[test]
        fn external_true_returns_zero() {
            if !have("/bin/true") {
                return;
            }
            let prog = parse("/bin/true").unwrap();
            let mut ev = Evaluator::new();
            assert_eq!(ev.eval_program(&prog).unwrap(), Outcome::Status(0));
        }

        #[test]
        fn external_false_returns_nonzero() {
            if !have("/bin/false") {
                return;
            }
            let prog = parse("/bin/false").unwrap();
            let mut ev = Evaluator::new();
            assert_eq!(ev.eval_program(&prog).unwrap().status(), 1);
        }

        #[test]
        fn external_unknown_is_not_found() {
            let prog = parse("definitely_not_a_real_command_xyzzy_42").unwrap();
            let mut ev = Evaluator::new();
            let err = ev.eval_program(&prog).unwrap_err();
            assert_eq!(err.exit_code(), 127);
        }

        #[test]
        fn andor_with_external_status() {
            if !have("/bin/false") || !have("/bin/echo") {
                return;
            }
            let prog = parse("/bin/false || /bin/echo backup").unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "backup\n");
        }

        #[test]
        fn two_stage_pipeline_captures_through() {
            if !have("/bin/echo") || !have("/bin/cat") {
                return;
            }
            let prog = parse("/bin/echo hello | /bin/cat").unwrap();
            let mut ev = Evaluator::new();
            let o = ev.eval_program(&prog).unwrap();
            assert_eq!(o.status(), 0);
            assert_eq!(ev.take_output(), "hello\n");
        }

        #[test]
        fn three_stage_pipeline_preserves_data() {
            if !have("/bin/echo") || !have("/bin/cat") {
                return;
            }
            let prog = parse("/bin/echo data | /bin/cat | /bin/cat").unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "data\n");
        }

        #[test]
        fn pipeline_status_is_last_stage() {
            if !have("/bin/true") || !have("/bin/false") {
                return;
            }
            // true | false → exit status 1 (last stage's).
            let prog = parse("/bin/true | /bin/false").unwrap();
            let mut ev = Evaluator::new();
            assert_eq!(ev.eval_program(&prog).unwrap().status(), 1);
            // false | true → 0.
            let prog = parse("/bin/false | /bin/true").unwrap();
            let mut ev = Evaluator::new();
            assert_eq!(ev.eval_program(&prog).unwrap().status(), 0);
        }

        #[test]
        fn pipeline_rejects_builtin_stage() {
            // `echo` is an in-process builtin — using it as a pipeline
            // stage isn't supported yet.
            let prog = parse("echo a | /bin/cat").unwrap();
            let mut ev = Evaluator::new();
            assert!(ev.eval_program(&prog).is_err());
        }
    }
}
