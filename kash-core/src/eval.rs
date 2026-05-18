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
            let value = if next == '(' {
                chars.next();
                let body = read_paren_body(&mut chars)?;
                self.run_command_substitution(&body)?
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
                match self.scope.get(&name) {
                    Some(v) => v.to_scalar_string(),
                    None => String::new(),
                }
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

    /// Current value of `IFS`. Falls back to the POSIX default
    /// `" \t\n"` if `IFS` is unset.
    fn lookup_ifs(&self) -> String {
        match self.scope.get("IFS") {
            Some(v) => v.to_scalar_string(),
            None => " \t\n".into(),
        }
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
                let body = read_paren_body(&mut chars)?;
                let value = self.run_command_substitution(&body)?;
                out.push_str(&value);
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
            for r in redirects {
                let path = self.expand_word(&r.target)?;
                self.open_redirect_file(r.kind, &path)?;
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
        /// External commands receive the opened files as their
        /// `Stdio`, so the kernel does the work directly.
        fn eval_with_redirects(
            &mut self,
            cmd: &SimpleCommand,
            argv: &[String],
        ) -> Result<Outcome> {
            use crate::ast::RedirectKind;
            use std::io::{Read, Write};
            use std::process::{Command, Stdio};
            // Resolve all redirect targets up front (so e.g. an
            // unreadable input file fails before we run anything).
            let mut out_file: Option<std::fs::File> = None;
            let mut both: bool = false;
            let mut in_file: Option<std::fs::File> = None;
            for r in &cmd.redirects {
                let path = self.expand_word(&r.target)?;
                let f = self.open_redirect_file(r.kind, &path)?;
                match r.kind {
                    RedirectKind::Input => in_file = Some(f),
                    RedirectKind::Output | RedirectKind::Append => {
                        out_file = Some(f);
                        both = false;
                    }
                    RedirectKind::OutputBoth | RedirectKind::AppendBoth => {
                        out_file = Some(f);
                        both = true;
                    }
                }
            }

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
                let _ = in_file; // builtins/functions don't consume stdin yet
                let _ = both; // stderr-redirect on builtins is a no-op (we don't model stderr)
                Ok(outcome)
            } else {
                // External command — let the kernel handle stdin/out
                // straight from the opened file descriptors.
                let mut c = Command::new(&argv[0]);
                c.args(&argv[1..]);
                if let Some(f) = in_file {
                    c.stdin(Stdio::from(f));
                } else {
                    c.stdin(Stdio::inherit());
                }
                let has_out = out_file.is_some();
                if let Some(f) = out_file {
                    if both {
                        let f2 = f.try_clone().map_err(|e| {
                            KashError::Runtime(alloc::format!("dup: {e}"))
                        })?;
                        c.stdout(Stdio::from(f));
                        c.stderr(Stdio::from(f2));
                    } else {
                        c.stdout(Stdio::from(f));
                        c.stderr(Stdio::inherit());
                    }
                } else {
                    c.stdout(Stdio::piped());
                    c.stderr(Stdio::inherit());
                }
                let mut child = c.spawn().map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        KashError::NotFound(alloc::format!("command `{}`", argv[0]))
                    } else {
                        KashError::Runtime(alloc::format!("exec: {e}"))
                    }
                })?;
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

            // Reap every stage. Pipeline exit status = last stage's
            // (POSIX). `pipefail` lands with the set-options pass.
            let mut final_status = 0;
            for (i, child) in children.iter_mut().enumerate() {
                let st = child
                    .wait()
                    .map_err(|e| KashError::Runtime(alloc::format!("wait: {e}")))?;
                if i == last {
                    final_status = st.code().unwrap_or(128);
                }
            }
            Ok(Outcome::Status(final_status))
        }
    }
});

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
    )
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
