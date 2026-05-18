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
use alloc::collections::BTreeMap;
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
            positionals: Vec::new(),
            positionals_stack: Vec::new(),
            functions: BTreeMap::new(),
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
        self.eval_statements(&prog.statements)
    }

    fn eval_statements(&mut self, stmts: &[Statement]) -> Result<Outcome> {
        let mut outcome = Outcome::Status(0);
        for stmt in stmts {
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
        // Phase 2: expand command name + arguments. POSIX field
        // splitting drops words that expand to nothing (the proper
        // "did this come from an unquoted expansion" distinction lands
        // when the expansion machinery grows up).
        let mut argv: Vec<String> = Vec::with_capacity(cmd.words.len());
        for w in &cmd.words {
            let expanded = self.expand_word(w)?;
            if !expanded.is_empty() {
                argv.push(expanded);
            }
        }
        if argv.is_empty() {
            // All command words vanished after expansion — treat the
            // whole simple command as a successful no-op (`A=1` with
            // an empty word list lands here too).
            return Ok(Outcome::Status(0));
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

    /// `set arg ...` rebinds the positional parameters. Other `set -o`
    /// forms light up later — for now anything starting with `-`/`+`
    /// is an unsupported-option error.
    fn builtin_set(&mut self, args: &[String]) -> Result<Outcome> {
        if let Some(a) = args.first() {
            if a.starts_with('-') || a.starts_with('+') {
                return Err(KashError::Runtime(
                    "set: options not yet supported".into(),
                ));
            }
        }
        self.positionals = args.to_vec();
        Ok(Outcome::Status(0))
    }

    fn builtin_unset(&mut self, args: &[String]) -> Result<Outcome> {
        // Simplified: removes the topmost binding for each name. The
        // proper `unset -v`/`-f` split lands with the full builtin
        // surface.
        for name in args {
            // Scope doesn't yet expose an `unset` API; emulate by
            // overwriting with `Empty` (lookups still return Some, but
            // is_empty() reports true). Good enough for the skeleton.
            self.scope.set(name.clone(), Value::Empty);
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
        // Save & swap positionals + push a frame.
        let saved = core::mem::replace(&mut self.positionals, argv[1..].to_vec());
        self.positionals_stack.push(saved);
        // Lexical capture (static scope) doesn't fully apply until the
        // scope module distinguishes static vs dynamic frames — until
        // then both scope flavours behave the same way (dynamic).
        let _ = entry.scope;
        let _ = &entry.captures;
        self.scope.push();
        let result = self.eval_compound(&entry.body);
        self.scope.pop();
        let restored = self
            .positionals_stack
            .pop()
            .expect("we just pushed");
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
                // alloc-only target — no fork. Simulate isolation with
                // a fresh frame so the subshell's variable writes can't
                // leak. (Real fork lands with external exec.)
                self.scope.push();
                let result = self.eval_statements(body);
                self.scope.pop();
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
            let cond_outcome = self.eval_statements(&branch.cond)?;
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
            let cond_outcome = self.eval_statements(cond)?;
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
                let mut out = Vec::with_capacity(ws.len());
                for w in ws {
                    out.push(self.expand_word(w)?);
                }
                out
            }
            // Omitted `in` clause iterates positional parameters.
            None => self.positionals.clone(),
        };
        let mut outcome = Outcome::Status(0);
        for item in items {
            self.scope.set(name.to_string(), Value::Scalar(item));
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
                    // verbatim. That's wrong but it's also harmless for
                    // strings without escapes.
                    out.push_str(s);
                }
            }
        }
        Ok(out)
    }

    /// Walk `text` and append it to `out`, substituting `$NAME`,
    /// `${…}`, and the specials (`$?`, `$#`, `$0`-`$9`, `$$`) along
    /// the way. Used for `Bare` and `DoubleQuoted` segments.
    fn expand_dollar(&mut self, text: &str, out: &mut String) -> Result<()> {
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '$' {
                out.push(c);
                continue;
            }
            // Peek the byte right after `$`.
            let Some(&next) = chars.peek() else {
                out.push('$');
                continue;
            };
            if next == '{' {
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
                if let Some(v) = self.scope.get(&name) {
                    out.push_str(&v.to_scalar_string());
                }
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

        // Bare `${NAME}` with no operator.
        if rest.is_empty() {
            return Ok(self.lookup_param(name));
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

        let current_present = self.scope.get(name).is_some();
        let current_value = self.lookup_param(name);
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
                    self.scope.set(name.to_string(), Value::Scalar(v.clone()));
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

    /// Look up `name` and return its scalar form, or empty for unset.
    fn lookup_param(&self, name: &str) -> String {
        // Specials.
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
        match self.scope.get(name) {
            Some(v) => v.to_scalar_string(),
            None => String::new(),
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

// ===== helpers =====

const fn is_name_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

const fn is_name_continue(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
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

/// Minimal POSIX glob: `?` matches one char, `*` matches any run of
/// chars, `[abc]` / `[!abc]` / `[a-z]` are character classes. Anything
/// that doesn't look like a glob metacharacter matches literally.
fn glob_match(pat: &str, s: &str) -> bool {
    glob_match_bytes(pat.as_bytes(), s.as_bytes())
}

fn glob_match_bytes(pat: &[u8], s: &[u8]) -> bool {
    let (p0, s0) = (pat.first().copied(), s.first().copied());
    match (p0, s0) {
        (None, None) => true,
        (None, _) => false,
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
            let Some(close) = pat[1..].iter().position(|&b| b == b']') else {
                // Unclosed `[` — match literally.
                return p0 == s0 && glob_match_bytes(&pat[1..], &s[1..]);
            };
            let class = &pat[1..1 + close];
            let (negate, class) =
                if let Some(rest) = class.strip_prefix(b"!").or_else(|| class.strip_prefix(b"^")) {
                    (true, rest)
                } else {
                    (false, class)
                };
            let mut hit = false;
            let mut i = 0;
            while i < class.len() {
                if i + 2 < class.len() && class[i + 1] == b'-' {
                    if c >= class[i] && c <= class[i + 2] {
                        hit = true;
                    }
                    i += 3;
                } else {
                    if class[i] == c {
                        hit = true;
                    }
                    i += 1;
                }
            }
            if hit == negate {
                return false;
            }
            glob_match_bytes(&pat[2 + close..], &s[1..])
        }
        (Some(p), Some(c)) if p == c => glob_match_bytes(&pat[1..], &s[1..]),
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
    fn function_local_assignment_persists_in_current_skeleton() {
        // The skeleton's `Scope::push` only allocates a fresh frame
        // but doesn't yet implement the static-vs-dynamic distinction
        // — assignments inside the function still target the function
        // frame and disappear after return.
        let (_, _, ev) = run("setit() { X=inside; }; setit");
        // X was set inside the function frame, which is popped on
        // return. So at top level X should still be unset.
        assert!(ev.scope().get("X").is_none());
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

    // ===== sanity: multistage pipeline still stubbed =====

    #[test]
    fn multistage_pipeline_unsupported_yields_runtime_error() {
        let prog = parse("echo a | true").unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_err());
    }
}
