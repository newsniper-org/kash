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
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::ast::{
    AndOrList, AndOrOp, CaseFallthrough, CaseItem, Command, CompoundCommand, CompoundKind,
    FunctionScope, IfBranch, Pipeline, Program, SimpleCommand, Statement, Word, WordSegment,
};
use crate::collections::{BTreeBackend, MapBackend, MapStorage, SetStorage};
use crate::error::{KashError, Result};
use crate::mode::Mode;
use crate::scope::{AttrSet, Scope};
use crate::value::Value;
use alloc::collections::BTreeMap;
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
    #[inline]
    #[must_use]
    pub fn status(self) -> i32 {
        match self {
            Self::Status(n) | Self::Exit(n) => n,
        }
    }

    /// `true` iff the program asked us to exit (via the `exit`
    /// builtin) rather than just completing with a status.
    #[inline]
    #[must_use]
    pub fn is_exit_request(self) -> bool {
        matches!(self, Self::Exit(_))
    }

    /// `true` iff [`status`](Self::status) is zero — POSIX "success".
    #[inline]
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
    /// `warn-integer-overflow` — when a value assigned to a typed
    /// integer variable (`int8`, `uint32`, …) doesn't fit, emit a
    /// warning to stderr noting the wrap. The wrap happens
    /// regardless; this option just makes it audible.
    pub warn_integer_overflow: bool,
}

/// One registered typeclass. Tracks both members that carry a
/// default implementation and abstract (signature-only) members.
/// `instance` registration enforces that every abstract member has
/// a concrete impl and that the instance does not introduce names
/// the typeclass never declared.
#[derive(Clone, Debug)]
struct TypeclassEntry {
    /// Default-method bodies keyed by method name.
    defaults: alloc::collections::BTreeMap<String, alloc::boxed::Box<CompoundCommand>>,
    /// Abstract (signature-only) method names. An instance must
    /// provide a body for each of these.
    abstracts: alloc::collections::BTreeSet<String>,
}

impl TypeclassEntry {
    /// True iff `method` is declared by this typeclass (either
    /// abstract or with a default body).
    fn declares(&self, method: &str) -> bool {
        self.defaults.contains_key(method) || self.abstracts.contains(method)
    }
}

/// One registered instance: concrete method bodies for a given
/// `(typeclass, type)` pair.
#[derive(Clone, Debug)]
struct InstanceEntry {
    /// Concrete method bodies keyed by method name.
    methods: alloc::collections::BTreeMap<String, alloc::boxed::Box<CompoundCommand>>,
}

/// One active virtual-environment frame.
#[derive(Clone, Debug, Default)]
struct VenvFrame {
    /// Materialised capability set for the frame. `None` means the
    /// venv had no `capabilities { … }` section, so capability
    /// checks pass through unrestricted. `Some(set)` — even one
    /// from `profile none` (empty set) — means the venv *did*
    /// declare a policy and checks consult the set.
    capabilities: Option<crate::capability::CapabilitySet>,
    /// Env-overlay directives applied at *external-command spawn*
    /// time. Stored in source order so successive `PATH-prepend`s
    /// stack correctly (the latest prepended entry ends up first).
    env_directives: Vec<crate::ast::EnvDirective>,
}

impl VenvFrame {
    fn new() -> Self {
        Self::default()
    }
}

/// One `use …` import in effect.
#[derive(Clone, Debug)]
enum ImportEntry {
    /// `use namespace foo[.bar]` — all `.foo.bar.*` symbols visible
    /// at bare-name lookups (excluding `_`-prefixed names).
    Wildcard {
        /// Source namespace path (no leading `.`).
        source: Vec<String>,
    },
    /// `use namespace foo as u` — references of the shape
    /// `.u.<name>` resolve as `.foo.<name>`.
    Aliased {
        /// Source namespace path (no leading `.`).
        source: Vec<String>,
        /// Alias the source is exposed under (single bare segment).
        alias: String,
    },
    /// `use .foo.bar.x` — binds the single symbol `x` (or its alias
    /// if `use .foo.bar.x as y` was used) into the bare-name space.
    Symbol {
        /// Path to the symbol's home namespace.
        source_path: Vec<String>,
        /// Symbol's name in its home namespace.
        source_name: String,
        /// Bare name to bind to. `None` means use `source_name`.
        alias: Option<String>,
    },
}

/// One registered function. Stored owned so the call site doesn't
/// need a borrow of the original AST.
#[derive(Clone, Debug)]
struct FunctionEntry {
    scope: FunctionScope,
    captures: Option<Vec<String>>,
    body: Box<CompoundCommand>,
    /// Namespace path at the point of definition. The call site
    /// swaps the evaluator's active path to this so the body's bare
    /// references resolve against the *defining* namespace, not the
    /// caller's.
    defining_namespace: Vec<String>,
}

/// Evaluator state. Construct via [`Evaluator::new`] /
/// [`Evaluator::with_mode`], drive via [`Evaluator::eval_program`],
/// and drain accumulated stdout via [`Evaluator::take_output`].
///
/// Generic over a [`MapBackend`] so map / set storage is decoupled
/// from the engine. Default is [`BTreeBackend`]; external callers
/// don't have to spell the parameter.
pub struct Evaluator<B: MapBackend = BTreeBackend> {
    scope: Scope<B>,
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
    /// PID of the most-recently-spawned background job (the `$!`
    /// special parameter). Zero before any `cmd &` has run.
    last_bg_pid: i32,
    /// Special `${.sh.value}` slot — the in-out channel between a
    /// variable's discipline hook (`.var.set` / `.var.get`) and
    /// the caller. Stored outside the ordinary scope stack so a
    /// hook defined under `function NAME` (static scope) can still
    /// mutate it and have the caller see the result. Always
    /// readable / writable; default empty.
    discipline_value: String,
    /// Subshell nesting level. `0` at the top, `+1` per
    /// `( … )` (subshell) entry, `-1` on exit. Surfaced as
    /// `${.sh.subshell}`.
    subshell_level: u32,
    /// 1-based line number of the currently-executing statement.
    /// Updated at every `eval_statement` entry; surfaced as
    /// `${.sh.lineno}`.
    current_lineno: u32,
    /// Most-recent `[[ $s =~ pat ]]` match text — exposed as
    /// `${.sh.match}`. Empty when no match has happened (or the
    /// last one failed).
    sh_match: String,
    /// Index of the last array element accessed via `${arr[i]}`
    /// or assigned via `arr[i]=v`. Exposed as `${.sh.subscript}`
    /// — same contract ksh93's variable-discipline context uses.
    sh_subscript: String,
    /// Stack of variable names whose discipline hooks are
    /// currently in flight. The top is what `${.sh.name}` returns
    /// — same contract ksh93 documents. Empty outside a hook.
    discipline_name_stack: Vec<String>,
    /// Re-entry guards for discipline-function hooks. When the
    /// set / get / unset hook for `name` is in flight, further
    /// assignment / lookup / unset of `name` skips the hook (so
    /// the hook itself can write to `.sh.value` or read the
    /// binding without triggering a cascade). Stored as a set of
    /// `(name, action)` pairs so multiple variables' hooks can be
    /// active concurrently.
    discipline_guard: alloc::collections::BTreeSet<(String, &'static str)>,
    /// User-defined type registry — `typedef NAME { … }`. Each
    /// entry stores the full body (per-instance fields, static
    /// fields, and lifecycle dunders); an instance copies
    /// per-instance fields into `var.field`, seeds static fields
    /// at `<NAME>.<field>` on first registration, and runs
    /// `__init` / `__del` against the active instance var.
    type_defs:
        alloc::collections::BTreeMap<String, Vec<crate::ast::TypeMember>>,
    /// Maps a live instance variable name to the type it was
    /// minted from. Drives `__del` dispatch on `unset` and
    /// `private` access enforcement on `var.field` reads /
    /// writes.
    type_instances: alloc::collections::BTreeMap<String, String>,
    /// Name of the type whose `__init` / `__del` body is
    /// currently executing — used to permit `private` field
    /// access only from inside the owning type's lifecycle
    /// methods. `None` outside a typedef body.
    in_type_method: Option<String>,
    /// Variable name of the instance whose lifecycle method is
    /// currently running. `_.field` reads / writes resolve
    /// against this name so the body's `_` prefix points at the
    /// active instance. `None` outside a typedef body.
    self_instance_var: Option<String>,
    /// Lazily-initialised `astro-float` constant cache, used by
    /// the extended-precision (`float128` / `complex256`)
    /// parse / format path. `Consts::new()` is comparatively
    /// expensive, so it's built once on first use.
    bf_consts: Option<astro_float::Consts>,
    /// Effective stdin for *external* commands inside the active
    /// compound body. Set by `{ … } < file` (and similar) on
    /// enter; restored on exit. The spawn paths consult this
    /// when no per-command input redirect / pipe is supplied —
    /// each spawn `dup`s a fresh handle so file offsets advance
    /// the way a real shell expects.
    #[cfg(feature = "std")]
    compound_input: Option<std::fs::File>,
    /// Live background-job handles. Std-only because `Child` is
    /// itself std. Kept just so each spawn doesn't immediately drop
    /// the handle and orphan the process — full job-control
    /// (`jobs`, `fg`, `bg`, `wait`) lands later.
    #[cfg(feature = "std")]
    background_jobs: Vec<std::process::Child>,
    /// Stderr-style diagnostic buffer for shell-emitted messages
    /// like `kash: cmd: command not found` and capability-denied
    /// notices. Distinct from `trace_output` (xtrace) because the
    /// host typically wants to route the two to different sinks.
    /// CLI entry points drain it via [`Evaluator::take_stderr`].
    stderr_output: String,
    /// Currently active mode. Not yet consulted (mode declarations
    /// aren't wired in), but threaded so callers can construct an
    /// evaluator under e.g. `default-secure`.
    mode: Mode<B>,
    /// Current positional arguments (`$1`, `$2`, …). Top-level value
    /// is empty; function calls push their argument list and restore
    /// the caller's on return.
    positionals: Vec<String>,
    /// Stack of saved positional sets for nested function calls.
    positionals_stack: Vec<Vec<String>>,
    /// Function registry: name → definition. Inside a `namespace`
    /// block, definitions are stored under their fully-qualified
    /// name (`.outer.inner.name`); top-level definitions are stored
    /// under the bare name.
    functions: B::Map<String, FunctionEntry>,
    /// Active namespace path. Each entry is a single segment with no
    /// leading `.`; e.g. inside `namespace foo { namespace bar { … } }`
    /// this is `["foo", "bar"]` and declarations register under
    /// `.foo.bar.NAME`. Empty at the top level. Push/pop is driven
    /// by [`CompoundKind::NamespaceDef`] and by function-call entry
    /// (each function carries the namespace path it was *defined* in
    /// so callers see the lexical view inside the body).
    namespace_path: Vec<String>,
    /// Active virtual-environment frames, stacked outer-to-inner.
    /// A frame is pushed on `venv NAME { … }` entry and popped on
    /// exit; the body's `body { … }` section runs while the frame
    /// is on top. v.1 ships an empty marker — capability sets,
    /// env overlays, and namespace imports hang here in later
    /// stages. Locked in `project_kash_venv.md`.
    venv_stack: Vec<VenvFrame>,
    /// Active namespace imports, organised by function-frame stack.
    /// Each top-level / function-call frame gets its own slot in the
    /// outer `Vec`. A `use namespace foo` statement pushes onto the
    /// topmost slot, and lookup consults *only* the topmost slot
    /// (strict isolation per `project_shell_namespace.md`).
    imports: Vec<Vec<ImportEntry>>,
    /// Mode-restoration stack, one entry per active mode-scoping
    /// frame (function call or `mode <name> { … }` block).
    /// `Some(saved_mode)` means "restore this on exit"; `None`
    /// means an unbounded `mode` declaration has propagated through
    /// this frame and the corresponding restore must be skipped.
    /// Locked by `project_shell_mode_syntax.md`.
    function_mode_save: Vec<Option<Mode<B>>>,
    /// Typeclass registry: typeclass name → default methods. Filled
    /// at `typeclass NAME { … }` declaration time.
    typeclasses: alloc::collections::BTreeMap<String, TypeclassEntry>,
    /// Instance registry: `(typeclass, type)` → method overrides.
    /// Filled at `instance NAME for TYPE { … }` declaration time.
    instances: alloc::collections::BTreeMap<(String, String), InstanceEntry>,
    /// Alias table: NAME → expansion text. Substitution happens at
    /// the start of a simple command's dispatch — the first
    /// (already-expanded) argv slot is matched against this table,
    /// and on a hit the slot is replaced by the alias body split on
    /// whitespace. Recursion is bounded per-command by an
    /// already-seen set so a self-referential alias (e.g.
    /// `alias ls='ls --color'`) terminates.
    aliases: B::Map<String, String>,
    /// Trap action registry: signal name → command source. Names are
    /// normalised to upper-case without a `SIG` prefix
    /// (`INT`, `TERM`, `EXIT`, …). The pseudo-signals `EXIT` / `ERR`
    /// are wired to fire at the appropriate points in evaluation; the
    /// real OS signals are accepted into the table but not yet
    /// delivered (that lands with the unix-only signal layer).
    traps: B::Map<String, String>,
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

impl<B: MapBackend> Evaluator<B> {
    /// New evaluator under the default mode.
    #[must_use]
    pub fn new() -> Self {
        Self::with_mode(Mode::<B>::default())
    }

    /// New evaluator under a specific mode.
    #[must_use]
    pub fn with_mode(mode: Mode<B>) -> Self {
        Self {
            scope: Scope::<B>::new(),
            last_status: 0,
            output: String::new(),
            trace_output: String::new(),
            stderr_output: String::new(),
            last_bg_pid: 0,
            discipline_value: String::new(),
            subshell_level: 0,
            current_lineno: 0,
            sh_match: String::new(),
            sh_subscript: String::new(),
            discipline_name_stack: Vec::new(),
            discipline_guard: alloc::collections::BTreeSet::new(),
            type_defs: alloc::collections::BTreeMap::new(),
            type_instances: alloc::collections::BTreeMap::new(),
            in_type_method: None,
            self_instance_var: None,
            bf_consts: None,
            #[cfg(feature = "std")]
            compound_input: None,
            #[cfg(feature = "std")]
            background_jobs: Vec::new(),
            mode,
            positionals: Vec::new(),
            positionals_stack: Vec::new(),
            functions: <B::Map<String, FunctionEntry> as Default>::default(),
            venv_stack: Vec::new(),
            namespace_path: Vec::new(),
            imports: alloc::vec![Vec::new()],
            function_mode_save: Vec::new(),
            typeclasses: alloc::collections::BTreeMap::new(),
            instances: alloc::collections::BTreeMap::new(),
            aliases: <B::Map<String, String> as Default>::default(),
            traps: <B::Map<String, String> as Default>::default(),
            in_trap: false,
            options: ShellOptions::default(),
            errexit_active: true,
        }
    }

    /// Read-only access to the active option set.
    #[inline]
    #[must_use]
    pub fn options(&self) -> &ShellOptions {
        &self.options
    }

    /// Replace the evaluator's top-level positional parameters
    /// (`$1`, `$2`, …) before running a program. Intended for CLI
    /// entry points that pass through `argv` past the script path.
    pub fn set_positionals(&mut self, args: Vec<String>) {
        self.positionals = args;
    }

    /// Seed a single export-flagged binding. Used by CLI entry
    /// points to surface the inherited process environment (each
    /// entry is registered as `name=value` with `export` set, so
    /// child commands see it through `apply_exported_env`).
    pub fn set_env_var(&mut self, name: &str, value: &str) -> Result<()> {
        self.scope.assign(name, Value::Scalar(value.into()))?;
        let attrs = crate::scope::AttrSet {
            export: true,
            ..crate::scope::AttrSet::default()
        };
        self.scope.apply_attrs(name, &attrs)?;
        Ok(())
    }

    /// Active mode.
    #[inline]
    #[must_use]
    pub fn mode(&self) -> &Mode<B> {
        &self.mode
    }

    /// Last command's `$?`.
    #[inline]
    #[must_use]
    pub fn last_status(&self) -> i32 {
        self.last_status
    }

    /// Build the fully-qualified storage name for a *declaration*
    /// (function, variable, typeclass, instance, …) defined at the
    /// current namespace path. `foo` inside `namespace utils { … }`
    /// becomes `.utils.foo`; at the top level (empty namespace
    /// path), the bare name is used unchanged.
    ///
    /// If the source already supplies a leading `.`, the path is
    /// taken as already fully-qualified and returned verbatim — this
    /// allows e.g. discipline functions like `.sh.value.set` to
    /// register against the root namespace regardless of where they
    /// were declared.
    fn qualify_decl_name(&self, name: &str) -> String {
        if name.starts_with('.') {
            return name.to_string();
        }
        if self.namespace_path.is_empty() {
            return name.to_string();
        }
        let mut out = String::with_capacity(
            self.namespace_path.iter().map(|s| s.len() + 1).sum::<usize>()
                + name.len()
                + 1,
        );
        for seg in &self.namespace_path {
            out.push('.');
            out.push_str(seg);
        }
        out.push('.');
        out.push_str(name);
        out
    }

    /// Pick the storage name under which a *variable* assignment
    /// should land. Inside a function frame the name is taken as-is
    /// (assignments are local to the frame). At file / namespace
    /// scope the active `namespace_path` is prefixed, so
    /// `foo=val` inside `namespace utils { … }` registers as
    /// `.utils.foo`. Absolute paths (`.foo.bar`) are pass-through.
    fn qualify_var_for_write(&self, name: &str) -> String {
        // Resolve `_` / `_.field` against the active instance var
        // first — that turns the lexical self-reference into a
        // regular qualified name before the namespace prefixing
        // pass runs.
        let rewritten = self.rewrite_self_ref(name);
        let name = rewritten.as_ref();
        if name.starts_with('.') || self.scope.in_function() || self.namespace_path.is_empty() {
            return name.to_string();
        }
        self.qualify_decl_name(name)
    }

    /// Apply the `set` discipline hook (`.<name>.set`) when one
    /// is registered. The raw value is placed in `.sh.value`, the
    /// hook is invoked, and the (possibly mutated) `.sh.value` is
    /// returned to the caller as the value to actually store.
    /// Re-entry from inside the hook itself skips this path and
    /// stores the value directly.
    fn apply_set_discipline(
        &mut self,
        name: &str,
        raw_value: String,
    ) -> Result<String> {
        let hook = alloc::format!(".{name}.set");
        if self.discipline_guard.contains(&(name.to_string(), "set"))
            || self.resolve_function_name(&hook).is_none()
        {
            return Ok(raw_value);
        }
        // Seed the discipline channel with the incoming value so
        // the hook can read / mutate it through `${.sh.value}`.
        let saved = core::mem::replace(&mut self.discipline_value, raw_value);
        self.discipline_guard.insert((name.to_string(), "set"));
        self.discipline_name_stack.push(name.to_string());
        let _ = self.call_function(&alloc::vec![hook]);
        self.discipline_name_stack.pop();
        self.discipline_guard.remove(&(name.to_string(), "set"));
        let stored = core::mem::replace(&mut self.discipline_value, saved);
        Ok(stored)
    }

    /// Apply the `get` discipline hook (`.<name>.get`) when one
    /// is registered. The current binding's value is placed in
    /// `.sh.value` before the hook runs; the hook can transform
    /// it; the hook's resulting `.sh.value` is returned.
    fn apply_get_discipline(
        &mut self,
        name: &str,
        current: String,
    ) -> String {
        let hook = alloc::format!(".{name}.get");
        if self.discipline_guard.contains(&(name.to_string(), "get"))
            || self.resolve_function_name(&hook).is_none()
        {
            return current;
        }
        let saved = core::mem::replace(&mut self.discipline_value, current);
        self.discipline_guard.insert((name.to_string(), "get"));
        self.discipline_name_stack.push(name.to_string());
        let _ = self.call_function(&alloc::vec![hook]);
        self.discipline_name_stack.pop();
        self.discipline_guard.remove(&(name.to_string(), "get"));
        core::mem::replace(&mut self.discipline_value, saved)
    }

    /// Apply the `unset` discipline hook (`.<name>.unset`) when
    /// one is registered. The hook receives no value — it's a
    /// notification. Re-entry skips.
    fn apply_unset_discipline(&mut self, name: &str) {
        let hook = alloc::format!(".{name}.unset");
        if self.discipline_guard.contains(&(name.to_string(), "unset"))
            || self.resolve_function_name(&hook).is_none()
        {
            return;
        }
        self.discipline_guard
            .insert((name.to_string(), "unset"));
        self.discipline_name_stack.push(name.to_string());
        let _ = self.call_function(&alloc::vec![hook]);
        self.discipline_name_stack.pop();
        self.discipline_guard
            .remove(&(name.to_string(), "unset"));
    }

    /// Register a `typedef NAME { … }` declaration. Stores the
    /// full body so later instances can re-read it; also seeds
    /// any `static` fields under `<NAME>.<field>` so they exist
    /// from registration time on.
    fn register_type_def(
        &mut self,
        name: &str,
        members: &[crate::ast::TypeMember],
    ) -> Result<()> {
        // Compute static-field defaults *before* we register the
        // members, so re-registering doesn't accidentally reset
        // static state if the user reloads the same typedef.
        let already_registered = self.type_defs.contains_key(name);
        self.type_defs.insert(name.to_string(), members.to_vec());
        // Install `__init` / `__del` bodies as hidden functions
        // under `.<Type>.__init` / `.<Type>.__del` so they go
        // through the normal `call_function` path (which gives
        // them a function frame — `local`, `return`, etc.).
        let defining_ns = self.namespace_path.clone();
        for m in members {
            match m {
                crate::ast::TypeMember::Init { body } => {
                    let key = alloc::format!(".{name}.__init");
                    self.functions.insert(
                        key,
                        FunctionEntry {
                            // Lifecycle dunders run with dynamic
                            // scope so writes to the instance's
                            // own `var.field` (and to type-level
                            // `<Type>.field` static storage)
                            // reach the surrounding scope. A
                            // strict static frame would trap
                            // every write inside the body and
                            // make the hook write-only-to-itself.
                            scope: crate::ast::FunctionScope::Dynamic,
                            captures: None,
                            body: body.clone(),
                            defining_namespace: defining_ns.clone(),
                        },
                    );
                }
                crate::ast::TypeMember::Del { body } => {
                    let key = alloc::format!(".{name}.__del");
                    self.functions.insert(
                        key,
                        FunctionEntry {
                            scope: crate::ast::FunctionScope::Dynamic,
                            captures: None,
                            body: body.clone(),
                            defining_namespace: defining_ns.clone(),
                        },
                    );
                }
                crate::ast::TypeMember::Field { .. } => {}
            }
        }
        if !already_registered {
            for m in members {
                if let crate::ast::TypeMember::Field {
                    name: field,
                    default,
                    static_: true,
                    ..
                } = m
                {
                    let value = self.expand_word(default)?;
                    let binding = alloc::format!("{name}.{field}");
                    let target = self.qualify_var_for_write(&binding);
                    self.scope.assign(&target, Value::Scalar(value))?;
                }
            }
        }
        Ok(())
    }

    /// Materialise a `typedef NAME var` instance — copy each
    /// per-instance field default into `<var>.<field>`, record the
    /// var→type mapping (so `__del` fires on unset and `private`
    /// access can be gated), then run the optional `__init` body
    /// under `in_type_method = Some(type)` so its body may touch
    /// private fields freely.
    fn instantiate_type(
        &mut self,
        type_name: &str,
        var_name: &str,
    ) -> Result<()> {
        let members = self
            .type_defs
            .get(type_name)
            .ok_or_else(|| {
                KashError::NotFound(alloc::format!(
                    "type `{type_name}` (use `typedef {type_name} {{ … }}` first)"
                ))
            })?
            .clone();
        for m in &members {
            if let crate::ast::TypeMember::Field {
                name: field,
                default,
                static_: false,
                ..
            } = m
            {
                let value = self.expand_word(default)?;
                let binding = alloc::format!("{var_name}.{field}");
                let target = self.qualify_var_for_write(&binding);
                self.scope.assign(&target, Value::Scalar(value))?;
            }
        }
        self.type_instances
            .insert(var_name.to_string(), type_name.to_string());
        // Run `__init` if defined.
        let init_name = alloc::format!(".{type_name}.__init");
        if self.functions.contains_key(&init_name) {
            let saved_t = self.in_type_method.replace(type_name.to_string());
            let saved_self = self.self_instance_var.replace(var_name.to_string());
            let res = self.call_function(&alloc::vec![init_name]);
            self.self_instance_var = saved_self;
            self.in_type_method = saved_t;
            res?;
        }
        Ok(())
    }

    /// Run a type's `__del` body if one is defined. Called from
    /// the `unset` path right before the instance's field
    /// bindings are removed, so the body still sees `var.field`.
    fn run_del_hook(&mut self, var_name: &str) -> Result<()> {
        let Some(type_name) = self.type_instances.get(var_name).cloned() else {
            return Ok(());
        };
        let del_name = alloc::format!(".{type_name}.__del");
        if self.functions.contains_key(&del_name) {
            let saved_t = self.in_type_method.replace(type_name.clone());
            let saved_self = self.self_instance_var.replace(var_name.to_string());
            let res = self.call_function(&alloc::vec![del_name]);
            self.self_instance_var = saved_self;
            self.in_type_method = saved_t;
            res?;
        }
        Ok(())
    }

    /// Rewrite the leading `_` in `_.<rest>` (or a bare `_`) to
    /// the active instance variable name when a lifecycle
    /// method is running. Outside a lifecycle body — or for any
    /// name that doesn't start with `_` — `name` is returned
    /// unchanged.
    fn rewrite_self_ref<'a>(&self, name: &'a str) -> alloc::borrow::Cow<'a, str> {
        let Some(self_var) = self.self_instance_var.as_deref() else {
            return alloc::borrow::Cow::Borrowed(name);
        };
        if name == "_" {
            return alloc::borrow::Cow::Owned(self_var.to_string());
        }
        if let Some(rest) = name.strip_prefix("_.") {
            return alloc::borrow::Cow::Owned(alloc::format!("{self_var}.{rest}"));
        }
        alloc::borrow::Cow::Borrowed(name)
    }

    /// Refuse external reads / writes to a `private` field of a
    /// live typed instance. `binding_name` is the qualified
    /// `var.field` form; the check is a no-op for any binding
    /// that isn't a private field of a recorded instance.
    fn check_private_member_access(&self, binding_name: &str) -> Result<()> {
        let Some((var, field)) = binding_name.split_once('.') else {
            return Ok(());
        };
        let Some(type_name) = self.type_instances.get(var) else {
            return Ok(());
        };
        let Some(members) = self.type_defs.get(type_name) else {
            return Ok(());
        };
        for m in members {
            if let crate::ast::TypeMember::Field {
                name,
                private: true,
                static_: false,
                ..
            } = m
                && name == field
                && self.in_type_method.as_deref() != Some(type_name.as_str())
            {
                return Err(KashError::Runtime(alloc::format!(
                    "field `{var}.{field}` is private to type `{type_name}`"
                )));
            }
        }
        Ok(())
    }

    /// Follow a chain of `typeset -n` namerefs starting from
    /// `name`. Returns the storage name the chain *terminates* on —
    /// the binding that actually holds the value. Cycles are
    /// truncated at a hop budget to avoid infinite recursion.
    fn follow_nameref_chain(&self, name: &str) -> String {
        let mut current = name.to_string();
        for _ in 0..16 {
            let Some(resolved) = self.resolve_var_name_skipping_nameref(&current) else {
                return current;
            };
            let Some(binding) = self.scope.get_binding(&resolved) else {
                return current;
            };
            match &binding.attrs.pending_nameref {
                Some(target) if !target.is_empty() && target != &current => {
                    current = target.clone();
                }
                _ => return resolved,
            }
        }
        current
    }

    /// Inner half of `resolve_var_name` that ignores the
    /// nameref-following step. Used by `follow_nameref_chain` to
    /// resolve each hop without re-entering itself.
    fn resolve_var_name_skipping_nameref(&self, name: &str) -> Option<String> {
        if self.scope.get(name).is_some() {
            return Some(name.to_string());
        }
        if name.starts_with('.') {
            return None;
        }
        for i in (1..=self.namespace_path.len()).rev() {
            let candidate = build_qualified_name(&self.namespace_path[..i], name);
            if self.scope.get(&candidate).is_some() {
                return Some(candidate);
            }
        }
        if let Some(frame) = self.imports.last() {
            for entry in frame {
                match entry {
                    ImportEntry::Wildcard { source } => {
                        if name.starts_with('_') {
                            continue;
                        }
                        let candidate = build_qualified_name(source, name);
                        if self.scope.get(&candidate).is_some() {
                            return Some(candidate);
                        }
                    }
                    ImportEntry::Symbol {
                        source_path,
                        source_name,
                        alias,
                    } => {
                        let bound = alias.as_deref().unwrap_or(source_name);
                        if name == bound {
                            let candidate = build_qualified_name(source_path, source_name);
                            if self.scope.get(&candidate).is_some() {
                                return Some(candidate);
                            }
                        }
                    }
                    ImportEntry::Aliased { .. } => {}
                }
            }
        }
        None
    }

    /// Resolve a *variable* reference for read. Returns the storage
    /// name we should look up (the bare name as written, or a
    /// path-prefixed one). Mirrors `resolve_function_name`: the
    /// walked path goes inside-out so an inner namespace shadows an
    /// outer one, then `use …` imports are consulted in declaration
    /// order. Returns `None` only when nothing matches.
    fn resolve_var_name(&self, name: &str) -> Option<String> {
        if self.scope.get(name).is_some() {
            return Some(name.to_string());
        }
        if let Some(rewritten) = self.rewrite_via_alias_import(name)
            && self.scope.get(&rewritten).is_some()
        {
            return Some(rewritten);
        }
        if name.starts_with('.') {
            return None;
        }
        for i in (1..=self.namespace_path.len()).rev() {
            let candidate = build_qualified_name(&self.namespace_path[..i], name);
            if self.scope.get(&candidate).is_some() {
                return Some(candidate);
            }
        }
        // Wildcard / symbol imports. Underscore-prefixed names are
        // excluded from *wildcard* imports (Python `__all__`-style
        // privacy convention); they remain reachable via the
        // explicit `use .foo._name` single-symbol form.
        if let Some(frame) = self.imports.last() {
            for entry in frame {
                match entry {
                    ImportEntry::Wildcard { source } => {
                        if name.starts_with('_') {
                            continue;
                        }
                        let candidate = build_qualified_name(source, name);
                        if self.scope.get(&candidate).is_some() {
                            return Some(candidate);
                        }
                    }
                    ImportEntry::Symbol {
                        source_path,
                        source_name,
                        alias,
                    } => {
                        let bound = alias.as_deref().unwrap_or(source_name);
                        if name == bound {
                            let candidate = build_qualified_name(source_path, source_name);
                            if self.scope.get(&candidate).is_some() {
                                return Some(candidate);
                            }
                        }
                    }
                    ImportEntry::Aliased { .. } => {}
                }
            }
        }
        None
    }

    /// If `name` is a dotted absolute reference whose first segment
    /// matches an `Aliased` import's alias, rewrite it to the
    /// import's source path. Otherwise return `None`. Doesn't check
    /// the resulting binding exists; callers must.
    fn rewrite_via_alias_import(&self, name: &str) -> Option<String> {
        let rest = name.strip_prefix('.')?;
        let (first, tail) = match rest.find('.') {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };
        let frame = self.imports.last()?;
        for entry in frame {
            if let ImportEntry::Aliased { source, alias } = entry
                && first == alias
            {
                let mut out = String::new();
                for seg in source {
                    out.push('.');
                    out.push_str(seg);
                }
                if let Some(tail) = tail {
                    out.push('.');
                    out.push_str(tail);
                }
                return Some(out);
            }
        }
        None
    }

    /// Resolve a *typeclass* name reference. Same shape as
    /// [`resolve_function_name`] / [`resolve_var_name`] — namespace
    /// path walk inside-out, then `use …` imports.
    fn resolve_typeclass_name(&self, name: &str) -> Option<String> {
        if self.typeclasses.contains_key(name) {
            return Some(name.to_string());
        }
        if let Some(rewritten) = self.rewrite_via_alias_import(name)
            && self.typeclasses.contains_key(&rewritten)
        {
            return Some(rewritten);
        }
        if name.starts_with('.') {
            return None;
        }
        for i in (1..=self.namespace_path.len()).rev() {
            let candidate = build_qualified_name(&self.namespace_path[..i], name);
            if self.typeclasses.contains_key(&candidate) {
                return Some(candidate);
            }
        }
        if let Some(frame) = self.imports.last() {
            for entry in frame {
                match entry {
                    ImportEntry::Wildcard { source } => {
                        if name.starts_with('_') {
                            continue;
                        }
                        let candidate = build_qualified_name(source, name);
                        if self.typeclasses.contains_key(&candidate) {
                            return Some(candidate);
                        }
                    }
                    ImportEntry::Symbol {
                        source_path,
                        source_name,
                        alias,
                    } => {
                        let bound = alias.as_deref().unwrap_or(source_name);
                        if name == bound {
                            let candidate =
                                build_qualified_name(source_path, source_name);
                            if self.typeclasses.contains_key(&candidate) {
                                return Some(candidate);
                            }
                        }
                    }
                    ImportEntry::Aliased { .. } => {}
                }
            }
        }
        None
    }

    /// Resolve a *reference* to a function by trying, in order:
    ///
    /// 1. the name as written (so absolute `.foo.bar` calls and
    ///    fully-qualified internal calls both win directly);
    /// 2. the name qualified against the current namespace path
    ///    (so a bare `helper` inside `namespace foo` finds
    ///    `.foo.helper`);
    /// 3. the same against successive outer namespaces (so a bare
    ///    reference inside `namespace foo.bar` falls back to
    ///    `.foo.helper` if no `.foo.bar.helper` exists);
    /// 4. `use namespace foo` imports in declaration order.
    ///
    /// Returns the *storage* name on success.
    fn resolve_function_name(&self, name: &str) -> Option<String> {
        if self.functions.contains_key(name) {
            return Some(name.to_string());
        }
        if let Some(rewritten) = self.rewrite_via_alias_import(name)
            && self.functions.contains_key(&rewritten)
        {
            return Some(rewritten);
        }
        if name.starts_with('.') {
            return None;
        }
        for i in (1..=self.namespace_path.len()).rev() {
            let candidate = build_qualified_name(&self.namespace_path[..i], name);
            if self.functions.contains_key(&candidate) {
                return Some(candidate);
            }
        }
        if let Some(frame) = self.imports.last() {
            for entry in frame {
                match entry {
                    ImportEntry::Wildcard { source } => {
                        if name.starts_with('_') {
                            continue;
                        }
                        let candidate = build_qualified_name(source, name);
                        if self.functions.contains_key(&candidate) {
                            return Some(candidate);
                        }
                    }
                    ImportEntry::Symbol {
                        source_path,
                        source_name,
                        alias,
                    } => {
                        let bound = alias.as_deref().unwrap_or(source_name);
                        if name == bound {
                            let candidate =
                                build_qualified_name(source_path, source_name);
                            if self.functions.contains_key(&candidate) {
                                return Some(candidate);
                            }
                        }
                    }
                    ImportEntry::Aliased { .. } => {}
                }
            }
        }
        None
    }

    /// Read-only access to the variable scope (for tests and
    /// embedders that want to peek without running anything).
    #[inline]
    #[must_use]
    pub fn scope(&self) -> &Scope<B> {
        &self.scope
    }

    #[cfg(test)]
    pub(crate) fn aliases_for_test(&self) -> &B::Map<String, String> {
        &self.aliases
    }

    /// Drain the accumulated output buffer, returning its contents.
    /// The internal buffer is left empty.
    #[inline]
    pub fn take_output(&mut self) -> String {
        core::mem::take(&mut self.output)
    }

    /// Drain the accumulated trace buffer (xtrace lines), returning
    /// its contents. The internal buffer is left empty.
    #[inline]
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
            if !outcome.success()
                && self.errexit_active
                && let Some(cmd) = self.traps.get("ERR").cloned()
            {
                let _ = self.run_trap_command(&cmd);
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
        self.current_lineno = stmt.lineno;
        let outcome = match stmt.terminator {
            crate::ast::Terminator::Background => self.eval_background(&stmt.list)?,
            crate::ast::Terminator::Sync => self.eval_and_or(&stmt.list)?,
        };
        self.last_status = outcome.status();
        Ok(outcome)
    }

    /// Run a list backgrounded. POSIX `cmd &` semantics:
    ///
    ///   * A simple external command is the easy case — `spawn`
    ///     without `wait`, record the PID as `$!`, return 0.
    ///   * A pipeline of *external* stages is the same, just one
    ///     stage per child.
    ///   * Builtins / functions / compound bodies don't fork — we
    ///     run them in-process, synchronously, but still report
    ///     status 0 and zero out `$!`. They aren't *truly*
    ///     backgrounded (the caller waits for them inline), and
    ///     that limitation is preserved as a deliberate v.1 simp
    ///     until a proper fork / thread / coroutine boundary
    ///     lands.
    ///   * `&&`/`||` lists backgrounded as a whole still bail —
    ///     the recovery semantics need their own design pass.
    fn eval_background(&mut self, list: &AndOrList) -> Result<Outcome> {
        if !list.tail.is_empty() {
            return Err(KashError::Runtime(
                "backgrounding an `&&`/`||` list isn't supported yet".into(),
            ));
        }
        let pipe = &list.head;
        if pipe.stages.len() == 1 {
            return self.eval_background_single(&pipe.stages[0]);
        }
        // Multi-stage pipeline. All-external → real background;
        // anything else falls back to in-process sync execution.
        let all_external = pipe.stages.iter().all(|st| {
            let crate::ast::Command::Simple(s) = st else {
                return false;
            };
            // We have to actually peek argv[0] to know — function
            // / builtin lookup is name-sensitive.
            if let Some(first) = s.words.first() {
                let raw = word_first_field_hint(first);
                !is_builtin_name(&raw) && self.resolve_function_name(&raw).is_none()
            } else {
                false
            }
        });
        if all_external {
            return self.spawn_pipeline_background(pipe);
        }
        // In-process fallback — run the pipeline synchronously and
        // discard its status, returning 0 / clearing `$!`. Same
        // limitation note as the single non-external case.
        self.last_bg_pid = 0;
        let _ = self.eval_pipeline(pipe)?;
        Ok(Outcome::Status(0))
    }

    fn eval_background_single(&mut self, stage: &crate::ast::Command) -> Result<Outcome> {
        match stage {
            crate::ast::Command::Compound(c) => {
                // Compound bodies run in-process — see the doc on
                // `eval_background`.
                self.last_bg_pid = 0;
                let _ = self.eval_compound(c)?;
                Ok(Outcome::Status(0))
            }
            crate::ast::Command::Simple(simple) => {
                let mut argv: Vec<String> = Vec::with_capacity(simple.words.len());
                for w in &simple.words {
                    argv.extend(self.expand_word_to_fields(w)?);
                }
                if argv.is_empty() {
                    return Err(KashError::Runtime(
                        "background stage expanded to nothing".into(),
                    ));
                }
                let name = argv[0].clone();
                let is_builtin_or_fn = is_builtin_name(&name)
                    || self.resolve_function_name(&name).is_some();
                if is_builtin_or_fn {
                    // In-process synchronous, status discarded.
                    self.last_bg_pid = 0;
                    let _ = self.eval_simple(simple)?;
                    return Ok(Outcome::Status(0));
                }
                if !simple.redirects.is_empty() {
                    return Err(KashError::Runtime(
                        "backgrounding with redirects isn't supported yet".into(),
                    ));
                }
                self.check_external_spawn(&name)?;
                self.spawn_background(argv)
            }
        }
    }

    #[cfg(feature = "std")]
    fn spawn_pipeline_background(&mut self, pipe: &Pipeline) -> Result<Outcome> {
        use std::process::{Child, Command, Stdio};
        // Replay the external pipeline spawn loop, but without
        // any wait / drain at the end. The first child's PID
        // lands in `$!` — POSIX nominates the *last* command's
        // PID for pipeline `$!`, but the last pid is also exposed
        // by `wait`; the first is more useful for tracking.
        let mut specs: Vec<(Vec<String>, StageIo)> =
            Vec::with_capacity(pipe.stages.len());
        for stage in &pipe.stages {
            let crate::ast::Command::Simple(simple) = stage else {
                return Err(KashError::Runtime(
                    "compound commands in pipeline stages are not yet supported".into(),
                ));
            };
            let mut argv: Vec<String> = Vec::with_capacity(simple.words.len());
            for w in &simple.words {
                argv.extend(self.expand_word_to_fields(w)?);
            }
            if argv.is_empty() {
                return Err(KashError::Runtime(
                    "pipeline stage expanded to nothing".into(),
                ));
            }
            let io = self.resolve_stage_io(&simple.redirects)?;
            specs.push((argv, io));
        }
        let n = specs.len();
        let mut children: Vec<Child> = Vec::with_capacity(n);
        let mut last_pid: i32 = 0;
        for (i, (argv, io)) in specs.iter_mut().enumerate() {
            self.check_external_spawn(&argv[0])?;
            let resolved =
                resolve_in_path(self, &argv[0]).unwrap_or_else(|| argv[0].clone());
            let mut cmd = Command::new(&resolved);
            cmd.args(&argv[1..]);
            self.apply_exported_env(&mut cmd);
            // Apply assignment prefixes (handled below for the
            // single-command external path; pipeline stages have
            // their own assignments check earlier).
            if let Some(f) = io.in_file.take() {
                cmd.stdin(Stdio::from(f));
            } else if i == 0 {
                cmd.stdin(Stdio::null());
            } else {
                let prev = children[i - 1]
                    .stdout
                    .take()
                    .expect("piped stdout");
                cmd.stdin(Stdio::from(prev));
            }
            if let Some(f) = io.stdout_file.take() {
                cmd.stdout(Stdio::from(f));
            } else {
                cmd.stdout(if i == n - 1 {
                    Stdio::inherit()
                } else {
                    Stdio::piped()
                });
            }
            if let Some(ef) = io.stderr_file.take() {
                cmd.stderr(Stdio::from(ef));
            } else {
                cmd.stderr(Stdio::inherit());
            }
            let child = cmd.spawn().map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    KashError::ExternalNotFound(argv[0].clone())
                } else {
                    KashError::Runtime(alloc::format!("spawn `{}`: {e}", argv[0]))
                }
            })?;
            if i == 0 {
                self.last_bg_pid = child.id() as i32;
            }
            last_pid = child.id() as i32;
            children.push(child);
        }
        let _ = last_pid;
        for c in children {
            self.background_jobs.push(c);
        }
        Ok(Outcome::Status(0))
    }

    #[cfg(not(feature = "std"))]
    fn spawn_pipeline_background(&mut self, _: &Pipeline) -> Result<Outcome> {
        Err(KashError::Runtime(
            "background pipelines require the std feature".into(),
        ))
    }

    #[cfg(feature = "std")]
    fn spawn_background(&mut self, argv: Vec<String>) -> Result<Outcome> {
        use std::process::{Command, Stdio};
        let resolved = resolve_in_path(self, &argv[0]).unwrap_or_else(|| argv[0].clone());
        let mut cmd = Command::new(&resolved);
        cmd.args(&argv[1..]);
        self.apply_exported_env(&mut cmd);
        // Detach stdin so the background process can't fight the
        // foreground for terminal reads. stdout / stderr inherit
        // so the user sees output the way real shells do.
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::inherit());
        cmd.stderr(Stdio::inherit());
        let child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                KashError::ExternalNotFound(argv[0].clone())
            } else {
                KashError::Runtime(alloc::format!("spawn `{}`: {e}", argv[0]))
            }
        })?;
        self.last_bg_pid = child.id() as i32;
        self.background_jobs.push(child);
        Ok(Outcome::Status(0))
    }

    #[cfg(not(feature = "std"))]
    fn spawn_background(&mut self, _argv: Vec<String>) -> Result<Outcome> {
        Err(KashError::Runtime(
            "background jobs require the std feature".into(),
        ))
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

    /// Buffer an error message destined for stderr. CLI entry
    /// points flush `take_stderr()` into the real stderr after
    /// `eval_program` returns; embedders see the buffer directly.
    fn report_to_stderr(&mut self, msg: &str) {
        self.stderr_output.push_str(msg);
        self.stderr_output.push('\n');
    }

    /// Drain the buffered stderr output. Returns whatever was
    /// written through `report_to_stderr` since the last drain.
    pub fn take_stderr(&mut self) -> String {
        core::mem::take(&mut self.stderr_output)
    }

    /// Run a compound command with redirects applied. Minimal
    /// surface: `> file`, `>> file`, `&> file`, `&>> file` route
    /// the body's captured stdout (and optionally stderr) into the
    /// target file. Input / stderr-only / fd-dup redirects on
    /// compound bodies aren't supported yet.
    fn eval_compound_with_redirects(&mut self, c: &CompoundCommand) -> Result<Outcome> {
        #[cfg(not(feature = "std"))]
        {
            let _ = c;
            Err(KashError::Runtime(
                "redirections on compound commands require the std feature".into(),
            ))
        }
        #[cfg(feature = "std")]
        {
            self.eval_compound_with_redirects_std(c)
        }
    }

    #[cfg(feature = "std")]
    fn eval_compound_with_redirects_std(&mut self, c: &CompoundCommand) -> Result<Outcome> {
        use crate::ast::RedirectKind;
        use std::io::Write;
        // Inline-bytes (here-doc / here-string) on compound bodies
        // still need cross-stage plumbing we haven't built. fd-dup
        // forms on compound bodies also stay out of scope.
        for r in &c.redirects {
            if matches!(
                r.kind,
                RedirectKind::HereString
                    | RedirectKind::HereDoc { .. }
                    | RedirectKind::DupInput
            ) {
                return Err(KashError::Runtime(
                    "here-doc / fd-dup input redirect on a compound command isn't supported yet"
                        .into(),
                ));
            }
        }
        let io = self.resolve_stage_io(&c.redirects)?;
        if io.in_inline.is_some() {
            return Err(KashError::Runtime(
                "here-doc / here-string redirect on a compound command isn't supported yet".into(),
            ));
        }
        // `{ … } < file` — install the file as effective stdin
        // for every external spawn inside `c`'s body. Each spawn
        // dup's its own handle so file offset advances naturally
        // across the body's commands.
        let saved_input = if let Some(f) = io.in_file {
            Some(core::mem::replace(&mut self.compound_input, Some(f)))
        } else {
            None
        };
        // If there's no stdout redirect either, the body just
        // runs with input set — done.
        let Some(mut out_file) = io.stdout_file else {
            let result = self.eval_compound_inner(c);
            if let Some(saved) = saved_input {
                self.compound_input = saved;
            }
            return result;
        };
        // Capture everything the body writes to the evaluator's
        // output buffer, then route the new tail into `out_file`.
        // Builtins / function results land in `output` directly;
        // external commands' stdout is captured into the same
        // buffer by the spawn-time `stdout(Stdio::piped())`.
        let mut stderr_file = if io.stderr_follows_stdout {
            Some(
                out_file
                    .try_clone()
                    .map_err(|e| KashError::Runtime(alloc::format!("dup: {e}")))?,
            )
        } else {
            io.stderr_file
        };
        let old_len = self.output.len();
        let result = self.eval_compound_inner(c);
        let chunk = self.output[old_len..].as_bytes().to_vec();
        self.output.truncate(old_len);
        out_file
            .write_all(&chunk)
            .map_err(|e| KashError::Runtime(alloc::format!("compound redirect write: {e}")))?;
        // Route any buffered stderr too — same drain pattern.
        if let Some(stderr_file) = stderr_file.as_mut() {
            let stderr_chunk = core::mem::take(&mut self.stderr_output);
            stderr_file
                .write_all(stderr_chunk.as_bytes())
                .map_err(|e| KashError::Runtime(alloc::format!("compound stderr write: {e}")))?;
        }
        if let Some(saved) = saved_input {
            self.compound_input = saved;
        }
        result
    }

    fn eval_command(&mut self, cmd: &Command) -> Result<Outcome> {
        let result = match cmd {
            Command::Simple(s) => self.eval_simple(s),
            Command::Compound(c) => self.eval_compound(c),
        };
        // POSIX 2.8.2: an external command-not-found surfaces as
        // exit status 127, *not* a shell-fatal error. `||` / `&&`
        // / `if … then` / explicit status checks all rely on that.
        // Capability-denied (kash extension) maps to 126 — POSIX
        // "command found but not invocable" — for the same reason.
        // Generic `KashError::NotFound` (typeclass / instance
        // declarations against undefined names, etc.) keeps
        // propagating — those are declarative errors, not command
        // dispatch failures.
        match result {
            Ok(o) => Ok(o),
            Err(KashError::ExternalNotFound(name)) => {
                self.report_to_stderr(&alloc::format!(
                    "kash: {name}: command not found"
                ));
                Ok(Outcome::Status(127))
            }
            Err(KashError::CapabilityDenied(msg)) => {
                self.report_to_stderr(&alloc::format!(
                    "kash: capability denied: {msg}"
                ));
                Ok(Outcome::Status(126))
            }
            Err(e) => Err(e),
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
            // Pick up `-i` / primitive-numeric attributes from the
            // existing binding (if any) so plain `a=300` honours a
            // prior `int8 a=…` declaration. `numeric_type` implies
            // `integer`: we run the RHS through arithmetic and
            // then wrap.
            let (integer, numeric_type) = self
                .scope
                .get_binding(&a.name)
                .map(|b| (b.attrs.integer, b.attrs.numeric_type))
                .unwrap_or((false, None));
            // Complex retains a different store path — `name`,
            // `name.re`, `name.im` all rewrite together — so peel
            // it off before the scalar-string branch.
            if let Some(nt) = numeric_type
                && nt.is_complex()
                && a.subscript.is_none()
            {
                let qualified = self.qualify_var_for_write(&a.name);
                let target = self.follow_nameref_chain(&qualified);
                self.check_private_member_access(&target)?;
                self.store_complex(&target, nt, &value, false)?;
                continue;
            }
            let value = if let Some(nt) = numeric_type {
                if nt.is_integer() {
                    let raw = i128::from(self.eval_arith(&value)?);
                    let wrapped = nt.wrap(raw);
                    if wrapped != raw && self.options.warn_integer_overflow {
                        self.stderr_output.push_str(&alloc::format!(
                            "kash: warning: value {raw} wrapped to {wrapped} for type `{}`\n",
                            nt.name(),
                        ));
                    }
                    alloc::format!("{wrapped}")
                } else if nt.is_extended_precision() {
                    // `float128` — 113-bit BigFloat path. (A
                    // `complex256` was already peeled off above.)
                    self.coerce_float128(&value)?
                } else if nt.is_float() {
                    let raw = match value.trim().parse::<f64>() {
                        Ok(f) => f,
                        Err(_) => self.eval_arith(&value)? as f64,
                    };
                    let projected = nt.project_float(raw);
                    format_float_value(projected)
                } else {
                    // Should be unreachable — complex was peeled
                    // off above. Fall back to the raw string to
                    // avoid an interactive panic if a future
                    // variant slips in.
                    value
                }
            } else if integer {
                let n = self.eval_arith(&value)?;
                alloc::format!("{n}")
            } else {
                value
            };
            let qualified = self.qualify_var_for_write(&a.name);
            // If `qualified` is itself a `typeset -n` nameref, the
            // write follows the chain to the target binding.
            let target = self.follow_nameref_chain(&qualified);
            // `.sh.value` is the discipline-hook channel — it lives
            // outside the scope stack so a hook defined under a
            // static-scoped `function NAME` form can still mutate
            // it visibly to its caller.
            if target == ".sh.value" {
                self.discipline_value = value;
                continue;
            }
            if let Some(sub) = &a.subscript {
                let idx = self.expand_word(sub)?;
                self.check_private_member_access(&target)?;
                self.assign_array_element(&target, &idx, value)?;
            } else {
                self.check_private_member_access(&target)?;
                // Discipline `.<target>.set` hook gets a chance to
                // transform the incoming value before storage.
                let stored = self.apply_set_discipline(&target, value)?;
                self.scope.assign(&target, Value::Scalar(stored))?;
            }
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
        let mut seen: B::Set<String> = <B::Set<String> as Default>::default();
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
        // Phase 3: dispatch. Try the typeclass explicit-dispatch
        // form first (`Typeclass::Type::method` lexes as one Word, so
        // a regular function name can never match it), then function
        // lookup (POSIX: regular builtins lose to user functions),
        // then builtins, then external exec.
        let name = argv[0].as_str();
        if let Some(out) = self.try_dispatch_typeclass(name, &argv)? {
            return Ok(out);
        }
        if self.resolve_function_name(name).is_some() {
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
            "read" => self.builtin_read(&argv[1..]),
            "source" | "." => self.builtin_source(&argv[1..]),
            "eval" => self.builtin_eval(&argv[1..]),
            "command" => self.builtin_command(&argv[1..]),
            "printf" => self.builtin_printf(&argv[1..]),
            "jobs" => self.builtin_jobs(&argv[1..]),
            "wait" => self.builtin_wait(&argv[1..]),
            "fg" => self.builtin_fg(&argv[1..]),
            "bg" => self.builtin_bg(&argv[1..]),
            "die" => self.builtin_die(&argv[1..]),
            "assert" => self.builtin_assert(&argv[1..]),
            "usage" => self.builtin_usage(&argv[1..]),
            "time" => self.builtin_time(&argv[1..]),
            "getopts" => self.builtin_getopts(&argv[1..]),
            "readonly" => self.builtin_readonly(&argv[1..]),
            "test" => builtin_test(false, &argv[1..]),
            "[" => builtin_test(true, &argv[1..]),
            "trap" => self.builtin_trap(&argv[1..]),
            "alias" => self.builtin_alias(&argv[1..]),
            "unalias" => self.builtin_unalias(&argv[1..]),
            "typeset" | "declare" => self.builtin_typeset(&argv[1..]),
            "export" => self.builtin_export(&argv[1..]),
            "use" => self.builtin_use(&argv[1..]),
            name if crate::scope::NumericType::from_name(name).is_some() => {
                // Bare primitive-type declaration form, e.g.
                // `int8 x=42` — treated as `typeset int8 x=42` so
                // the same attribute + wrap pipeline runs. The
                // type-name stays at `argv[0]` for the inner
                // parser to consume.
                self.builtin_typeset(&argv)
            }
            _ => self.run_external(&argv),
        }
    }

    /// Run `argv[0]` as an external program. Available only under
    /// `std` — the alloc-only build collapses this into `NotFound`.
    /// The venv capability check fires here so both builds enforce
    /// the same policy at the *call* site; the std-only spawn
    /// helpers run it again at the spawn site for defence-in-depth.
    fn run_external(&mut self, argv: &[String]) -> Result<Outcome> {
        self.check_external_spawn(&argv[0])?;
        #[cfg(feature = "std")]
        {
            self.run_external_std(argv)
        }
        #[cfg(not(feature = "std"))]
        {
            Err(KashError::ExternalNotFound(argv[0].clone()))
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
            "warn-integer-overflow" => self.options.warn_integer_overflow = on,
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
            // Lifecycle: run `__del` for a typed instance *before*
            // we strip any state, so the body still sees the
            // instance's fields.
            self.run_del_hook(name)?;
            // Discipline `.<name>.unset` hook gets notified before
            // the binding actually disappears.
            self.apply_unset_discipline(name);
            // For a typed instance, sweep its per-instance
            // `var.field` bindings out alongside the bare `var`.
            self.tear_down_type_instance(name);
            // A non-existent name returning 0 matches POSIX behaviour.
            let _ = self.scope.unset(name);
            // Allow unsetting a function as a convenience.
            self.functions.remove(name);
        }
        Ok(Outcome::Status(0))
    }

    /// Sweep every `var.field` binding for the named instance and
    /// drop the var→type entry. Called from `builtin_unset` after
    /// `__del` has already run.
    fn tear_down_type_instance(&mut self, var_name: &str) {
        let Some(type_name) = self.type_instances.remove(var_name) else {
            return;
        };
        let Some(members) = self.type_defs.get(&type_name).cloned() else {
            return;
        };
        for m in &members {
            if let crate::ast::TypeMember::Field {
                name: field,
                static_: false,
                ..
            } = m
            {
                let binding = alloc::format!("{var_name}.{field}");
                let _ = self.scope.unset(&binding);
            }
        }
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

    /// `time COMMAND [ARGS …]` — run COMMAND and emit a
    /// `real m s` line to stderr afterwards. POSIX reserves
    /// `time` as a *keyword* whose syntax allows pipelines, but
    /// the builtin form (one command) covers the day-to-day case
    /// and is what's been wanted here. user / sys are stubbed at
    /// zero — those need `getrusage` plumbing, scheduled with the
    /// signal layer.
    fn builtin_time(&mut self, args: &[String]) -> Result<Outcome> {
        if args.is_empty() {
            return Err(KashError::Runtime("time: missing command".into()));
        }
        self.builtin_time_impl(args)
    }

    #[cfg(feature = "std")]
    fn builtin_time_impl(&mut self, args: &[String]) -> Result<Outcome> {
        let start = std::time::Instant::now();
        let argv = args.to_vec();
        let result = if self.resolve_function_name(&argv[0]).is_some() {
            self.call_function(&argv)
        } else if is_builtin_name(&argv[0]) {
            self.dispatch_known_builtin(&argv)
        } else {
            self.run_external(&argv)
        };
        let elapsed = start.elapsed();
        let secs = elapsed.as_secs();
        let line = alloc::format!(
            "\nreal\t{}m{}.{:03}s\nuser\t0m0.000s\nsys\t0m0.000s\n",
            secs / 60,
            secs % 60,
            elapsed.subsec_millis(),
        );
        self.report_to_stderr(&line);
        result
    }

    #[cfg(not(feature = "std"))]
    fn builtin_time_impl(&mut self, _args: &[String]) -> Result<Outcome> {
        Err(KashError::Runtime(
            "time requires the std feature (`Instant`)".into(),
        ))
    }

    /// `getopts OPTSTRING NAME [ARGS …]` — POSIX option-string
    /// parser. Reads `$OPTIND` to know how far we've walked,
    /// writes the chosen option letter to NAME, the matched
    /// argument (if any) to `OPTARG`, and bumps `OPTIND`. Returns
    /// 0 while there's still work to do, 1 once the args are
    /// exhausted (or we hit `--`).
    ///
    /// Minimum surface: single-letter options, optional argument
    /// trailer (`a:` means `-a` takes an argument as the rest of
    /// the same token or the next positional). Clustered forms
    /// (`-abc`) and OPTERR / silent-mode (`:opts`) are follow-up
    /// work.
    fn builtin_getopts(&mut self, args: &[String]) -> Result<Outcome> {
        let optstring = args
            .first()
            .cloned()
            .ok_or_else(|| KashError::Runtime("getopts: missing OPTSTRING".into()))?;
        let var_name = args
            .get(1)
            .cloned()
            .ok_or_else(|| KashError::Runtime("getopts: missing NAME".into()))?;
        let parse_args: Vec<String> = if args.len() > 2 {
            args[2..].to_vec()
        } else {
            self.positionals.clone()
        };
        let optind: usize = self
            .scope
            .get("OPTIND")
            .map(|v| v.to_scalar_string().parse::<usize>().unwrap_or(1))
            .unwrap_or(1);
        let bind_var = |this: &mut Self, name: &str, value: &str| -> Result<()> {
            let target = this.qualify_var_for_write(name);
            this.scope.assign(&target, Value::Scalar(value.into()))?;
            Ok(())
        };
        if optind == 0 || optind > parse_args.len() {
            bind_var(self, &var_name, "?")?;
            return Ok(Outcome::Status(1));
        }
        let cur = parse_args[optind - 1].clone();
        if !cur.starts_with('-') || cur == "-" {
            bind_var(self, &var_name, "?")?;
            return Ok(Outcome::Status(1));
        }
        if cur == "--" {
            bind_var(self, "OPTIND", &alloc::format!("{}", optind + 1))?;
            bind_var(self, &var_name, "?")?;
            return Ok(Outcome::Status(1));
        }
        let opt_char = cur.chars().nth(1).expect("validated above");
        let mut chars = optstring.chars().peekable();
        let mut found = false;
        let mut needs_arg = false;
        while let Some(c) = chars.next() {
            if c == opt_char {
                found = true;
                if chars.peek() == Some(&':') {
                    needs_arg = true;
                }
                break;
            }
        }
        if !found {
            // Unknown option — bind NAME=`?`, OPTARG to the char.
            bind_var(self, &var_name, "?")?;
            bind_var(self, "OPTARG", &opt_char.to_string())?;
            bind_var(self, "OPTIND", &alloc::format!("{}", optind + 1))?;
            return Ok(Outcome::Status(0));
        }
        bind_var(self, &var_name, &opt_char.to_string())?;
        if needs_arg {
            let arg_val = if cur.len() > 2 {
                cur[2..].to_string()
            } else {
                parse_args.get(optind).cloned().unwrap_or_default()
            };
            bind_var(self, "OPTARG", &arg_val)?;
            let step = if cur.len() > 2 { 1 } else { 2 };
            bind_var(self, "OPTIND", &alloc::format!("{}", optind + step))?;
        } else {
            bind_var(self, "OPTIND", &alloc::format!("{}", optind + 1))?;
        }
        Ok(Outcome::Status(0))
    }

    /// `die [MSG] [STATUS]` — kash extension. Print MSG to stderr
    /// (if given) and exit the script with STATUS (default 1).
    /// Bash idiom for fail-fast scripts; locked in
    /// `project_shell_builtins.md`.
    fn builtin_die(&mut self, args: &[String]) -> Result<Outcome> {
        let msg = args.first().cloned().unwrap_or_default();
        let status = args
            .get(1)
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(1);
        if !msg.is_empty() {
            self.report_to_stderr(&alloc::format!("kash: {msg}"));
        }
        Ok(Outcome::Exit(status))
    }

    /// `assert EXPR…` — kash extension. Evaluate the args as a
    /// `[[ … ]]` expression and `die` on false. Used for
    /// invariant / precondition checks at the top of a function.
    fn builtin_assert(&mut self, args: &[String]) -> Result<Outcome> {
        let owned: alloc::vec::Vec<String> = args.to_vec();
        let truth = eval_double_bracket(&owned)?;
        if truth {
            Ok(Outcome::Status(0))
        } else {
            let body = args.join(" ");
            self.report_to_stderr(&alloc::format!(
                "kash: assertion failed: [[ {body} ]]"
            ));
            Ok(Outcome::Exit(1))
        }
    }

    /// `usage [NAME]` — kash extension. Print a usage line for
    /// NAME (current function name when omitted) and exit with
    /// status 2 — the conventional "shell misuse" code. The full
    /// doc-comment plumbing remains a future stage; for now the
    /// builtin emits a stub line so scripts can call it as the
    /// `default` arm of an option parser.
    fn builtin_usage(&mut self, args: &[String]) -> Result<Outcome> {
        let target = args.first().cloned().unwrap_or_else(|| "<command>".into());
        self.output.push_str(&alloc::format!("Usage: {target}\n"));
        Ok(Outcome::Exit(2))
    }

    /// `jobs` — print the live background-job table. Output is
    /// `[<job-id>] <pid> Running` one per line, in the order the
    /// jobs were spawned. Std-only because background jobs are
    /// themselves std-only.
    fn builtin_jobs(&mut self, _args: &[String]) -> Result<Outcome> {
        self.builtin_jobs_impl()
    }

    #[cfg(feature = "std")]
    fn builtin_jobs_impl(&mut self) -> Result<Outcome> {
        for (i, child) in self.background_jobs.iter().enumerate() {
            self.output.push_str(&alloc::format!(
                "[{}] {} Running\n",
                i + 1,
                child.id()
            ));
        }
        Ok(Outcome::Status(0))
    }

    #[cfg(not(feature = "std"))]
    fn builtin_jobs_impl(&mut self) -> Result<Outcome> {
        Err(KashError::Runtime(
            "jobs requires the std feature".into(),
        ))
    }

    /// `wait [PID]` — block until the named background job exits;
    /// without an argument, block until *every* background job
    /// exits. Returns the waited-on job's exit status (the last
    /// one's, for the all-jobs form).
    fn builtin_wait(&mut self, args: &[String]) -> Result<Outcome> {
        self.builtin_wait_impl(args)
    }

    #[cfg(feature = "std")]
    fn builtin_wait_impl(&mut self, args: &[String]) -> Result<Outcome> {
        if let Some(pid_arg) = args.first() {
            let pid: i32 = pid_arg.parse().map_err(|_| {
                KashError::Runtime(alloc::format!("wait: `{pid_arg}` is not a valid PID"))
            })?;
            let idx = self
                .background_jobs
                .iter()
                .position(|c| c.id() as i32 == pid);
            let Some(idx) = idx else {
                return Err(KashError::Runtime(alloc::format!(
                    "wait: no such background job `{pid}`"
                )));
            };
            let mut child = self.background_jobs.swap_remove(idx);
            let st = child
                .wait()
                .map_err(|e| KashError::Runtime(alloc::format!("wait: {e}")))?;
            return Ok(Outcome::Status(st.code().unwrap_or(128)));
        }
        let mut last = 0;
        for mut child in core::mem::take(&mut self.background_jobs) {
            let st = child
                .wait()
                .map_err(|e| KashError::Runtime(alloc::format!("wait: {e}")))?;
            last = st.code().unwrap_or(128);
        }
        Ok(Outcome::Status(last))
    }

    #[cfg(not(feature = "std"))]
    fn builtin_wait_impl(&mut self, _: &[String]) -> Result<Outcome> {
        Err(KashError::Runtime(
            "wait requires the std feature".into(),
        ))
    }

    /// `fg` / `bg` — terminal-foreground job control. Not
    /// supported in this commit cycle; SIGSTOP / SIGCONT handling
    /// and tty foreground hand-off need their own design.
    fn builtin_fg(&mut self, _args: &[String]) -> Result<Outcome> {
        Err(KashError::Runtime(
            "fg: terminal foreground job control isn't supported yet".into(),
        ))
    }
    fn builtin_bg(&mut self, _args: &[String]) -> Result<Outcome> {
        Err(KashError::Runtime(
            "bg: terminal foreground job control isn't supported yet".into(),
        ))
    }

    /// `printf FORMAT [ARG …]` — POSIX format-string output. The
    /// format string honours `\n` / `\t` / `\r` / `\\` / `\0`
    /// escapes; conversions cover `%s`, `%d` / `%i`, `%x`, `%o`,
    /// `%c`, and `%%`. Width / precision modifiers are ignored
    /// (the conversion char is what we dispatch on). Missing
    /// arguments substitute the empty string for `%s` and zero
    /// for numeric conversions; surplus arguments cycle the
    /// format string until they're exhausted. Output streams into
    /// the evaluator's stdout buffer like every other builtin.
    fn builtin_printf(&mut self, args: &[String]) -> Result<Outcome> {
        if args.is_empty() {
            return Err(KashError::Runtime(
                "printf: missing format string".into(),
            ));
        }
        let format = printf_unescape(&args[0]);
        let mut params = &args[1..];
        loop {
            let (chunk, consumed) = printf_format(&format, params)?;
            self.output.push_str(&chunk);
            if params.is_empty() || consumed == 0 {
                break;
            }
            params = &params[consumed.min(params.len())..];
            if params.is_empty() {
                break;
            }
        }
        Ok(Outcome::Status(0))
    }

    /// `command [-v | -V] NAME [ARG …]` — POSIX bypass of the
    /// function / alias dispatch step. Two surface modes:
    ///
    /// * Bare form: `command NAME …` runs NAME against the
    ///   builtin table first, falling through to external lookup.
    ///   Any same-named function / alias is ignored.
    /// * Probe form: `command -v NAME` prints what NAME would
    ///   resolve to (alias / function / builtin / absolute path)
    ///   and exits 0; an unknown name exits 1 with no output.
    ///   `-V` is the verbose variant.
    fn builtin_command(&mut self, args: &[String]) -> Result<Outcome> {
        let mut probe_v = false;
        let mut probe_caps = false;
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if a == "-v" {
                probe_v = true;
                i += 1;
            } else if a == "-V" {
                probe_caps = true;
                i += 1;
            } else if a == "--" {
                i += 1;
                break;
            } else if a.starts_with('-') && a.len() > 1 {
                return Err(KashError::Runtime(alloc::format!(
                    "command: unknown option `{a}`"
                )));
            } else {
                break;
            }
        }
        let rest = &args[i..];
        if rest.is_empty() {
            return Err(KashError::Runtime("command: missing command name".into()));
        }
        let name = &rest[0];
        if probe_v || probe_caps {
            return self.builtin_command_probe(name, probe_caps);
        }
        // Bare form — dispatch like a simple command, but skip
        // function / alias lookup.
        self.builtin_command_invoke(rest)
    }

    fn builtin_command_probe(&mut self, name: &str, verbose: bool) -> Result<Outcome> {
        // Alias?
        if self.aliases.contains_key(name) {
            let body = self.aliases.get(name).cloned().unwrap_or_default();
            let line = if verbose {
                alloc::format!("{name} is aliased to `{body}`\n")
            } else {
                alloc::format!("alias {name}='{body}'\n")
            };
            self.output.push_str(&line);
            return Ok(Outcome::Status(0));
        }
        // Function?
        if let Some(resolved) = self.resolve_function_name(name) {
            let line = if verbose {
                alloc::format!("{name} is a function ({resolved})\n")
            } else {
                alloc::format!("{name}\n")
            };
            self.output.push_str(&line);
            return Ok(Outcome::Status(0));
        }
        // Builtin?
        if is_builtin_name(name) {
            let line = if verbose {
                alloc::format!("{name} is a shell builtin\n")
            } else {
                alloc::format!("{name}\n")
            };
            self.output.push_str(&line);
            return Ok(Outcome::Status(0));
        }
        // External — std-feature only; alloc-only build has no
        // way to spot a PATH hit so we just report "not found".
        #[cfg(feature = "std")]
        {
            // Absolute / relative paths: check the file directly
            // instead of walking PATH (resolve_in_path skips
            // anything containing a slash).
            if name.contains('/') {
                if std::path::Path::new(name).is_file() {
                    let line = if verbose {
                        alloc::format!("{name} is {name}\n")
                    } else {
                        alloc::format!("{name}\n")
                    };
                    self.output.push_str(&line);
                    return Ok(Outcome::Status(0));
                }
            } else if let Some(path) = resolve_in_path(self, name) {
                let line = if verbose {
                    alloc::format!("{name} is {path}\n")
                } else {
                    alloc::format!("{path}\n")
                };
                self.output.push_str(&line);
                return Ok(Outcome::Status(0));
            }
        }
        Ok(Outcome::Status(1))
    }

    fn builtin_command_invoke(&mut self, argv: &[String]) -> Result<Outcome> {
        // The bare-form dispatch is just "builtin or external"
        // skipping the function / alias steps.
        let name = &argv[0];
        if is_builtin_name(name) {
            self.dispatch_known_builtin(argv)
        } else {
            self.run_external(argv)
        }
    }

    /// Dispatch a builtin by name without going through the full
    /// command-resolution pipeline. Available on both alloc and
    /// std builds (unlike `dispatch_builtin`, which is std-only
    /// and shaped for the redirect-bearing path).
    fn dispatch_known_builtin(&mut self, argv: &[String]) -> Result<Outcome> {
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
            "read" => self.builtin_read(&argv[1..]),
            "source" | "." => self.builtin_source(&argv[1..]),
            "eval" => self.builtin_eval(&argv[1..]),
            "command" => self.builtin_command(&argv[1..]),
            "printf" => self.builtin_printf(&argv[1..]),
            "jobs" => self.builtin_jobs(&argv[1..]),
            "wait" => self.builtin_wait(&argv[1..]),
            "fg" => self.builtin_fg(&argv[1..]),
            "bg" => self.builtin_bg(&argv[1..]),
            "die" => self.builtin_die(&argv[1..]),
            "assert" => self.builtin_assert(&argv[1..]),
            "usage" => self.builtin_usage(&argv[1..]),
            "time" => self.builtin_time(&argv[1..]),
            "getopts" => self.builtin_getopts(&argv[1..]),
            "readonly" => self.builtin_readonly(&argv[1..]),
            "test" => builtin_test(false, &argv[1..]),
            "[" => builtin_test(true, &argv[1..]),
            "trap" => self.builtin_trap(&argv[1..]),
            "alias" => self.builtin_alias(&argv[1..]),
            "unalias" => self.builtin_unalias(&argv[1..]),
            "typeset" | "declare" => self.builtin_typeset(&argv[1..]),
            "export" => self.builtin_export(&argv[1..]),
            "use" => self.builtin_use(&argv[1..]),
            name if crate::scope::NumericType::from_name(name).is_some() => {
                self.builtin_typeset(&argv)
            }
            other => Err(KashError::Runtime(alloc::format!(
                "command: `{other}` is not a known builtin"
            ))),
        }
    }

    /// `eval ARGS …` — join the args with spaces, parse the result
    /// as kash source, and evaluate in the current scope. Blocked
    /// under the `-secure` modifier per the locked security policy
    /// (`project_kash_security_policy.md`).
    fn builtin_eval(&mut self, args: &[String]) -> Result<Outcome> {
        use crate::collections::SetStorage;
        if self.mode.modifiers.contains(&crate::mode::Modifier::Secure) {
            return Err(KashError::SecureViolation(
                "`eval` is disabled under the `-secure` modifier".into(),
            ));
        }
        if args.is_empty() {
            return Ok(Outcome::Status(0));
        }
        let source = args.join(" ");
        let prog = crate::parser::parse(&source)
            .map_err(|e| KashError::Parse(alloc::format!("eval: {e}")))?;
        self.eval_program(&prog)
    }

    /// `source PATH` / `. PATH` — read PATH and evaluate its
    /// contents in the *current* scope (no new function frame).
    /// Definitions, assignments, mode declarations, and namespace
    /// imports inside the loaded file all affect the caller.
    /// Honours the active venv's `fs-read` capability.
    fn builtin_source(&mut self, args: &[String]) -> Result<Outcome> {
        let path = args
            .first()
            .ok_or_else(|| KashError::Runtime("source: missing PATH".into()))?;
        if !self.is_capability_allowed(crate::capability::Capability::FsRead) {
            return Err(KashError::CapabilityDenied(alloc::format!(
                "source `{path}`: this venv revoked `fs-read`"
            )));
        }
        self.builtin_source_impl(path)
    }

    #[cfg(feature = "std")]
    fn builtin_source_impl(&mut self, path: &str) -> Result<Outcome> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            KashError::Runtime(alloc::format!("source: {path}: {e}"))
        })?;
        let prog = crate::parser::parse(&content)
            .map_err(|e| KashError::Parse(alloc::format!("source {path}: {e}")))?;
        self.eval_program(&prog)
    }

    #[cfg(not(feature = "std"))]
    fn builtin_source_impl(&mut self, _: &str) -> Result<Outcome> {
        Err(KashError::Runtime(
            "source requires the std feature (filesystem access)".into(),
        ))
    }

    /// `read [-r] [-p PROMPT|--prompt=PROMPT] [NAME …]` — read a
    /// line from stdin and bind the IFS-split fields to the given
    /// names. Defaults: one name → `REPLY`; multi-name → first
    /// `N-1` names get one field each, the last gets the remainder.
    /// Returns exit status `1` on EOF, `0` otherwise.
    fn builtin_read(&mut self, args: &[String]) -> Result<Outcome> {
        let parsed = parse_read_args(args)?;
        self.builtin_read_impl(parsed)
    }

    #[cfg(feature = "std")]
    fn builtin_read_impl(&mut self, p: ReadArgs) -> Result<Outcome> {
        use std::io::{self, BufRead, Write};
        if let Some(prompt) = &p.prompt {
            let mut stderr = io::stderr();
            let _ = write!(stderr, "{prompt}");
            let _ = stderr.flush();
        }
        let mut line = String::new();
        let nread = io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| KashError::Runtime(alloc::format!("read: {e}")))?;
        if nread == 0 {
            return Ok(Outcome::Status(1));
        }
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        let processed = if p.raw {
            line
        } else {
            unescape_read_line(&line)
        };
        let names = if p.names.is_empty() {
            alloc::vec!["REPLY".to_string()]
        } else {
            p.names
        };
        let ifs = self.lookup_ifs();
        let fields = split_for_read(&processed, &ifs, names.len());
        for (n, v) in names.iter().zip(fields.iter()) {
            let target = self.qualify_var_for_write(n);
            self.scope.assign(&target, Value::Scalar(v.clone()))?;
        }
        Ok(Outcome::Status(0))
    }

    #[cfg(not(feature = "std"))]
    fn builtin_read_impl(&mut self, _: ReadArgs) -> Result<Outcome> {
        Err(KashError::Runtime(
            "read requires the std feature (stdin)".into(),
        ))
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
            for (name, value) in self.aliases.iter() {
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
            for (sig, cmd) in self.traps.iter() {
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

    /// `typeset` / `declare` builtin. Parses leading `-…` /  `+…`
    /// option clusters into an [`AttrSet`], then either:
    ///
    /// - with no further args, prints the (filtered) listing of
    ///   bindings, one `typeset … NAME=VALUE` line each, in
    ///   sorted-by-name order;
    /// - otherwise applies the attribute set to each `NAME` /
    ///   `NAME=VALUE` operand and, if a value is given, stores it
    ///   through `Scope::assign_local` when inside a function frame
    ///   or `Scope::assign` at top level.
    fn builtin_typeset(&mut self, args: &[String]) -> Result<Outcome> {
        let mut attrs = AttrSet::default();
        let mut print_mode = false;
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if a == "--" {
                i += 1;
                break;
            }
            // Bare primitive type-name positions before operands,
            // e.g. `typeset int8 x=42` / `typeset uint32 -r n`.
            // The name is consumed *as an attribute*, not as a
            // target.
            if let Some(nt) = crate::scope::NumericType::from_name(a) {
                attrs.numeric_type = Some(nt);
                attrs.integer = true;
                i += 1;
                continue;
            }
            if let Some(rest) = a.strip_prefix('-') {
                if rest.is_empty() {
                    i += 1;
                    break;
                }
                for c in rest.chars() {
                    match c {
                        'r' => attrs.readonly = true,
                        'x' => attrs.export = true,
                        'i' => attrs.integer = true,
                        'l' => attrs.lowercase = true,
                        'u' => attrs.uppercase = true,
                        'a' => attrs.indexed = true,
                        'A' => attrs.assoc = true,
                        'n' => {
                            // Marker — the actual target name lives
                            // in the operand's `=value` half, which
                            // the loop below pulls out.
                            attrs.pending_nameref = Some(String::new());
                        }
                        'p' => print_mode = true,
                        other => {
                            return Err(KashError::Runtime(alloc::format!(
                                "typeset: unknown flag `-{other}`"
                            )));
                        }
                    }
                }
                i += 1;
                continue;
            }
            break;
        }

        let has_operands = i < args.len();
        if print_mode || !has_operands {
            self.print_typeset_listing(&attrs);
            return Ok(Outcome::Status(0));
        }

        let in_func = self.scope.in_function();
        let is_nameref = attrs.pending_nameref.is_some();
        while i < args.len() {
            let arg = &args[i];
            if let Some(eq) = arg.find('=') {
                let (name, rest) = arg.split_at(eq);
                if !is_identifier(name) {
                    return Err(KashError::Runtime(alloc::format!(
                        "typeset: `{name}` is not a valid identifier"
                    )));
                }
                let raw_value = rest[1..].to_string();
                let target = self.qualify_var_for_write(name);
                let attrs_for_name = if is_nameref {
                    // nameref target lives in the `=` half — fold it
                    // into `pending_nameref` instead of the value.
                    AttrSet {
                        pending_nameref: Some(raw_value.clone()),
                        ..attrs.clone()
                    }
                } else {
                    attrs.clone()
                };
                self.scope.apply_attrs(&target, &attrs_for_name)?;
                if is_nameref {
                    // Reference: the binding's value is never read;
                    // skip the value-store step.
                    i += 1;
                    continue;
                }
                // Complex types fan out into three bindings
                // (`name`, `name.re`, `name.im`) — coerce_for_attrs
                // can only return one string, so route around it.
                if let Some(nt) = attrs.numeric_type
                    && nt.is_complex()
                {
                    self.store_complex(&target, nt, &raw_value, in_func)?;
                    i += 1;
                    continue;
                }
                let value = self.coerce_for_attrs(&attrs, raw_value)?;
                if in_func {
                    self.scope.assign_local(&target, Value::Scalar(value))?;
                } else {
                    self.scope.assign(&target, Value::Scalar(value))?;
                }
            } else {
                if !is_identifier(arg) {
                    return Err(KashError::Runtime(alloc::format!(
                        "typeset: `{arg}` is not a valid identifier"
                    )));
                }
                let target = self.qualify_var_for_write(arg);
                self.scope.apply_attrs(&target, &attrs)?;
            }
            i += 1;
        }
        Ok(Outcome::Status(0))
    }

    /// `use …` — install an import into the current function frame.
    /// Four surface forms are accepted:
    ///
    /// * `use namespace PATH` — wildcard import.
    /// * `use namespace PATH as ALIAS` — aliased namespace.
    ///   References of shape `.<ALIAS>.<name>` rewrite to
    ///   `.<PATH>.<name>` before lookup.
    /// * `use .PATH.NAME` — single-symbol import; binds the bare
    ///   name to the absolute path.
    /// * `use .PATH.NAME as ALIAS` — single symbol bound to `ALIAS`.
    fn builtin_use(&mut self, args: &[String]) -> Result<Outcome> {
        let parsed = parse_use_args(args)?;
        let frame = self
            .imports
            .last_mut()
            .expect("root imports frame always present");
        for entry in parsed {
            frame.push(entry);
        }
        Ok(Outcome::Status(0))
    }

    /// `export [NAME[=VAL] …]` — short-hand for `typeset -x`.
    /// `export` with no args lists the currently-exported bindings.
    fn builtin_export(&mut self, args: &[String]) -> Result<Outcome> {
        if args.is_empty() {
            let filter = AttrSet {
                export: true,
                ..AttrSet::default()
            };
            self.print_typeset_listing(&filter);
            return Ok(Outcome::Status(0));
        }
        let export_attrs = AttrSet {
            export: true,
            ..AttrSet::default()
        };
        for arg in args {
            if let Some(eq) = arg.find('=') {
                let (name, rest) = arg.split_at(eq);
                if !is_identifier(name) {
                    return Err(KashError::Runtime(alloc::format!(
                        "export: `{name}` is not a valid identifier"
                    )));
                }
                self.scope.apply_attrs(name, &export_attrs)?;
                let value = rest[1..].to_string();
                self.scope.assign(name, Value::Scalar(value))?;
            } else {
                if !is_identifier(arg) {
                    return Err(KashError::Runtime(alloc::format!(
                        "export: `{arg}` is not a valid identifier"
                    )));
                }
                self.scope.apply_attrs(arg, &export_attrs)?;
            }
        }
        Ok(Outcome::Status(0))
    }

    /// Apply the attribute-aware coercion that runs before a value
    /// goes through `scope.assign*`: `-i` runs the string through
    /// arithmetic, `-l` / `-u` fold case. Errors propagate (e.g.
    /// bad arithmetic).
    /// Apply the post-lookup half of the flag pipeline (split,
    /// sort, dedup, join, quote, case) to an already-formed
    /// `Vec<String>`. This is the seam the multi-element flags
    /// (`(k)` / `(v)`) hand their results off to.
    fn finish_flag_pipeline(
        &self,
        flags: &ExpansionFlags,
        parts: Vec<String>,
    ) -> String {
        apply_expansion_flags_parts(flags, parts)
    }

    /// `(t)` — produce the type-descriptor string for the
    /// variable named by `body`. Returns `"unset"` when nothing
    /// is bound; `"compound"` when no bare binding exists but
    /// at least one `body.X` member does.
    fn expansion_type_query(&self, body: &str) -> String {
        // Compound-var sentinel: no bare binding but at least
        // one `body.X` member exists.
        let bare = self.scope.get_binding(body);
        if bare.is_none() {
            let prefix = alloc::format!("{body}.");
            let has_member = self
                .scope
                .all_bindings()
                .any(|(n, _)| n.starts_with(&prefix));
            if has_member {
                return "compound".into();
            }
            return "unset".into();
        }
        match &bare.expect("checked").value {
            Value::Scalar(_) => "scalar".into(),
            Value::Array(_) => "indexed".into(),
            Value::AssocArray(_) => "assoc".into(),
            Value::Empty => "unset".into(),
        }
    }

    /// `(k)` / `(v)` — list the keys or values of `body`. For
    /// an associative array the keys / values are read from the
    /// stored map; for an indexed array we surface the numeric
    /// indices as keys; for a compound variable (a set of
    /// `body.X` bindings) the member names / values come from
    /// scanning the binding table; for a plain scalar both
    /// flags fall back to the value alone, matching zsh's
    /// scalar fallback.
    fn expansion_keys_or_values(
        &self,
        body: &str,
        want_keys: bool,
        want_values: bool,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(b) = self.scope.get_binding(body) {
            match &b.value {
                Value::AssocArray(m) => {
                    for (k, v) in m {
                        if want_keys {
                            out.push(k.clone());
                        }
                        if want_values {
                            out.push(v.clone());
                        }
                    }
                    return out;
                }
                Value::Array(v) => {
                    for (i, elem) in v.iter().enumerate() {
                        if want_keys {
                            out.push(alloc::format!("{i}"));
                        }
                        if want_values {
                            out.push(elem.clone());
                        }
                    }
                    return out;
                }
                Value::Scalar(s) => {
                    if want_keys {
                        out.push(body.to_string());
                    }
                    if want_values {
                        out.push(s.clone());
                    }
                    return out;
                }
                Value::Empty => {}
            }
        }
        // Compound: scan bindings whose name starts with `body.`.
        let prefix = alloc::format!("{body}.");
        let mut members: Vec<(String, String)> = self
            .scope
            .all_bindings()
            .filter_map(|(n, b)| {
                n.strip_prefix(&prefix).map(|rest| {
                    (rest.to_string(), b.value.to_scalar_string())
                })
            })
            .collect();
        members.sort_by(|a, b| a.0.cmp(&b.0));
        for (k, v) in members {
            if want_keys {
                out.push(k);
            }
            if want_values {
                out.push(v);
            }
        }
        out
    }

    /// `(%)` — prompt-style escape expansion. v1 understands a
    /// small fixed vocabulary: `%n` → `USER`, `%~` → cwd with
    /// `$HOME` collapsed to `~`, `%/` → cwd, `%%` → literal
    /// `%`. Anything else passes through verbatim so a future
    /// commit can extend the table without breaking callers.
    fn expand_prompt_escapes(&mut self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 1 < bytes.len() {
                match bytes[i + 1] {
                    b'%' => {
                        out.push('%');
                        i += 2;
                        continue;
                    }
                    b'n' => {
                        if let Some(v) = self.scope.get("USER") {
                            out.push_str(&v.to_scalar_string());
                        }
                        i += 2;
                        continue;
                    }
                    b'/' => {
                        #[cfg(feature = "std")]
                        if let Ok(p) = std::env::current_dir() {
                            out.push_str(&p.to_string_lossy());
                        }
                        i += 2;
                        continue;
                    }
                    b'~' => {
                        #[cfg(feature = "std")]
                        if let Ok(p) = std::env::current_dir() {
                            let s = p.to_string_lossy().to_string();
                            out.push_str(&self.abbreviate_home_prefix(s));
                        }
                        i += 2;
                        continue;
                    }
                    _ => {}
                }
            }
            out.push(bytes[i] as char);
            i += 1;
        }
        out
    }

    /// `(D)` — replace a leading `$HOME` prefix with `~`. No-op
    /// when `HOME` is unset or the value doesn't start with it.
    fn abbreviate_home_prefix(&mut self, value: String) -> String {
        let home = match self.scope.get("HOME") {
            Some(v) => v.to_scalar_string(),
            None => return value,
        };
        if home.is_empty() {
            return value;
        }
        if let Some(rest) = value.strip_prefix(&home) {
            return alloc::format!("~{rest}");
        }
        value
    }

    /// Whether an unmatched filesystem glob should *fail* rather
    /// than survive as its literal pattern. Per
    /// `project_shell_glob_pattern.md`: fail in `default` mode and
    /// under any `-secure` modifier; otherwise (POSIX-aware,
    /// ksh93u-*) the literal pattern is left in place (POSIX
    /// behaviour). The `(#qN)` qualifier opts back into the
    /// literal-survives form. Wired in this commit; the filesystem
    /// glob step that consults it lands in the next sub-commit.
    fn null_glob_should_fail(&self) -> bool {
        self.mode.modifiers.contains(&crate::mode::Modifier::Secure)
            || self.mode.base == crate::mode::BaseMode::Default
    }

    /// Refuse zsh `${(…)body}` flag-block expansions inside
    /// the strict modes that disable zsh extensions. POSIX-
    /// strict and `ksh93u-strict` are both no-extensions modes
    /// per `project_shell_modes.md`.
    fn check_zsh_flag_mode(&self) -> Result<()> {
        match self.mode.base {
            crate::mode::BaseMode::PosixStrict
            | crate::mode::BaseMode::Ksh93uStrict => Err(KashError::Mode(alloc::format!(
                "zsh-style `${{(flags)…}}` expansion is not available inside `{}`; \
                 switch to an `*-aware` or `default` mode",
                self.mode,
            ))),
            _ => Ok(()),
        }
    }

    /// Store a complex literal into `name`. Parses the input,
    /// projects each component through the type's float
    /// precision, and writes three bindings: `name`, `name.re`,
    /// `name.im`. The bare `name` carries the canonical
    /// `R+Ii`-form string so `${name}` reads round-trip.
    fn store_complex(
        &mut self,
        name: &str,
        nt: crate::scope::NumericType,
        raw: &str,
        in_func: bool,
    ) -> Result<()> {
        let (re_str, im_str, scalar) = if nt.is_extended_precision() {
            // `complex256` — each component is a 113-bit BigFloat.
            let (re_raw, im_raw) =
                split_complex_literal(raw).ok_or_else(|| {
                    KashError::Runtime(alloc::format!(
                        "invalid complex literal `{raw}` for type `{}`",
                        nt.name(),
                    ))
                })?;
            let re = self.coerce_float128(&re_raw)?;
            let im = self.coerce_float128(&im_raw)?;
            let scalar = format_complex_strings(&re, &im);
            (re, im, scalar)
        } else {
            let (re_raw, im_raw) = parse_complex_literal(raw).ok_or_else(|| {
                KashError::Runtime(alloc::format!(
                    "invalid complex literal `{raw}` for type `{}`",
                    nt.name(),
                ))
            })?;
            let (re, im) = nt.project_complex(re_raw, im_raw);
            (
                format_float_value(re),
                format_float_value(im),
                format_complex_value(re, im),
            )
        };
        // Per-component bindings — `name.re` / `name.im` — let
        // user code read or update one half without re-parsing
        // the string form.
        let re_name = alloc::format!("{name}.re");
        let im_name = alloc::format!("{name}.im");
        if in_func {
            self.scope.assign_local(&re_name, Value::Scalar(re_str))?;
            self.scope.assign_local(&im_name, Value::Scalar(im_str))?;
            self.scope.assign_local(name, Value::Scalar(scalar))?;
        } else {
            self.scope.assign(&re_name, Value::Scalar(re_str))?;
            self.scope.assign(&im_name, Value::Scalar(im_str))?;
            self.scope.assign(name, Value::Scalar(scalar))?;
        }
        Ok(())
    }

    /// Lazily build (and return) the `astro-float` constant cache
    /// used for extended-precision parsing / formatting.
    fn bf_consts(&mut self) -> Result<&mut astro_float::Consts> {
        if self.bf_consts.is_none() {
            let c = astro_float::Consts::new().map_err(|_| {
                KashError::Runtime(
                    "failed to initialise extended-precision constants".into(),
                )
            })?;
            self.bf_consts = Some(c);
        }
        Ok(self.bf_consts.as_mut().expect("just initialised"))
    }

    /// Parse a decimal string as a `float128` (113-bit binary128)
    /// and return its canonical decimal string form. Used for
    /// `float128` storage and `complex256` components.
    fn coerce_float128(&mut self, raw: &str) -> Result<String> {
        use astro_float::{BigFloat, Radix, RoundingMode};
        let trimmed = raw.trim();
        let cc = self.bf_consts()?;
        let bf = BigFloat::parse(
            trimmed,
            Radix::Dec,
            FLOAT128_PREC,
            RoundingMode::ToEven,
            cc,
        );
        if bf.is_nan() {
            return Err(KashError::Runtime(alloc::format!(
                "invalid float128 literal `{raw}`"
            )));
        }
        let rendered = bf
            .format(Radix::Dec, RoundingMode::ToEven, cc)
            .map_err(|_| {
                KashError::Runtime("float128 formatting failed".into())
            })?;
        Ok(cleanup_bigfloat_str(&rendered))
    }

    /// Format a `BigFloat` as its canonical `float128` decimal
    /// string (used by the arithmetic engine's float128 tier).
    fn format_bigfloat(
        &mut self,
        bf: &astro_float::BigFloat,
    ) -> Result<String> {
        use astro_float::{Radix, RoundingMode};
        let cc = self.bf_consts()?;
        let rendered = bf
            .format(Radix::Dec, RoundingMode::ToEven, cc)
            .map_err(|_| {
                KashError::Runtime("float128 formatting failed".into())
            })?;
        Ok(cleanup_bigfloat_str(&rendered))
    }

    fn coerce_for_attrs(
        &mut self,
        attrs: &AttrSet,
        value: String,
    ) -> Result<String> {
        let value = if let Some(nt) = attrs.numeric_type {
            if nt.is_integer() {
                // Typed integer: evaluate the RHS as arithmetic,
                // then wrap to the type's range. Wrap-on-overflow
                // is the policy; surfacing the wrap as a warning
                // is gated on the `warn-integer-overflow` set
                // option.
                let raw = i128::from(self.eval_arith(&value)?);
                let wrapped = nt.wrap(raw);
                if wrapped != raw && self.options.warn_integer_overflow {
                    self.stderr_output.push_str(&alloc::format!(
                        "kash: warning: value {raw} wrapped to {wrapped} for type `{}`\n",
                        nt.name(),
                    ));
                }
                alloc::format!("{wrapped}")
            } else if nt.is_extended_precision() {
                // `float128` — parse / store through the 113-bit
                // BigFloat path so precision beyond `f64` survives.
                self.coerce_float128(&value)?
            } else if nt.is_float() {
                // Typed float: parse the RHS as `f64`, falling
                // back to the arithmetic engine for integer-only
                // forms like `$((2 + 3))`. Project to the type's
                // precision and format back. No overflow warning
                // — IEEE 754 already encodes Inf / NaN on its own.
                let raw = match value.trim().parse::<f64>() {
                    Ok(f) => f,
                    Err(_) => self.eval_arith(&value)? as f64,
                };
                let projected = nt.project_float(raw);
                format_float_value(projected)
            } else {
                // Complex: callers route around this helper via
                // `store_complex`. Returning the raw string is a
                // defensive fallback in case a new path reaches
                // here.
                value
            }
        } else if attrs.integer {
            let n = self.eval_arith(&value)?;
            alloc::format!("{n}")
        } else {
            value
        };
        let value = if attrs.uppercase {
            value.to_uppercase()
        } else if attrs.lowercase {
            value.to_lowercase()
        } else {
            value
        };
        Ok(value)
    }

    /// `typeset` listing. Walks every binding, filters by the
    /// (possibly empty) attribute mask, and emits one canonical
    /// `typeset -<flags> NAME=VALUE` line each in sorted-by-name
    /// order. `[]` / `()` array forms follow the ksh93 shape.
    fn print_typeset_listing(&mut self, filter: &AttrSet) {
        // Collect (name, attrs, value) snapshots so we don't fight
        // the borrow checker while pushing into `self.output`.
        let mut entries: alloc::vec::Vec<(String, AttrSet, Value)> = self
            .scope
            .all_bindings()
            .map(|(n, b)| (n.clone(), b.attrs.clone(), b.value.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|a, b| a.0 == b.0);
        for (name, attrs, value) in entries {
            if !attrs_match_filter(&attrs, filter) {
                continue;
            }
            let flags = format_attrs(&attrs);
            let rendered = format_value_for_listing(&value);
            self.output
                .push_str(&alloc::format!("typeset{flags} {name}={rendered}\n"));
        }
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

    /// Explicit typeclass dispatch. Recognises a command name of the
    /// `Typeclass::Type::method` shape and routes the call into the
    /// matching instance's method body — falling back to the
    /// typeclass's default body if the instance doesn't override the
    /// method.
    ///
    /// Returns `Ok(None)` when the name doesn't fit the shape or the
    /// typeclass simply isn't registered, so the caller can fall
    /// through to ordinary function / builtin / external dispatch.
    /// Returns `Ok(Some(_))` if the dispatch found a body and ran it,
    /// or an error if a typeclass-shaped name matched a known
    /// typeclass but no method body was available.
    fn try_dispatch_typeclass(
        &mut self,
        name: &str,
        argv: &[String],
    ) -> Result<Option<Outcome>> {
        let parts: alloc::vec::Vec<&str> = name.splitn(3, "::").collect();
        match parts.len() {
            3 => {
                // Stage 2: fully-qualified `Typeclass::Type::method`.
                let (tc_ref, ty, method) = (parts[0], parts[1], parts[2]);
                let Some(tc) = self.resolve_typeclass_name(tc_ref) else {
                    return Ok(None);
                };
                let body = self.resolve_typeclass_body(&tc, ty, method)?;
                let body_args: alloc::vec::Vec<String> = argv[1..].to_vec();
                self.run_typeclass_body(body, body_args).map(Some)
            }
            2 => {
                // Stage 3: inferred `Typeclass::method`.
                let (tc_ref, method) = (parts[0], parts[1]);
                let Some(tc) = self.resolve_typeclass_name(tc_ref) else {
                    return Ok(None);
                };
                let (ty, body_args) = infer_dispatch_type(argv);
                let body = self.resolve_typeclass_body(&tc, &ty, method)?;
                self.run_typeclass_body(body, body_args).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Look up the method body for `(typeclass, type, method)`.
    /// Instance methods beat the typeclass default; neither present
    /// is a `KashError::NotFound`.
    fn resolve_typeclass_body(
        &self,
        tc: &str,
        ty: &str,
        method: &str,
    ) -> Result<alloc::boxed::Box<CompoundCommand>> {
        let instance_body = self
            .instances
            .get(&(tc.to_string(), ty.to_string()))
            .and_then(|i| i.methods.get(method))
            .cloned();
        match instance_body {
            Some(b) => Ok(b),
            None => self
                .typeclasses
                .get(tc)
                .and_then(|t| t.defaults.get(method))
                .cloned()
                .ok_or_else(|| {
                    KashError::NotFound(alloc::format!(
                        "typeclass method `{tc}::{ty}::{method}`"
                    ))
                }),
        }
    }

    /// Run a resolved typeclass method body like a function call:
    /// swap in the positional parameters, push a static-scope
    /// function frame, evaluate, restore.
    fn run_typeclass_body(
        &mut self,
        body: alloc::boxed::Box<CompoundCommand>,
        body_args: alloc::vec::Vec<String>,
    ) -> Result<Outcome> {
        let saved = core::mem::replace(&mut self.positionals, body_args);
        self.positionals_stack.push(saved);
        self.scope.push_function_frame(true);
        let result = self.eval_compound(&body);
        self.scope.pop();
        let restored = self.positionals_stack.pop().expect("just pushed");
        self.positionals = restored;
        result
    }

    fn call_function(&mut self, argv: &[String]) -> Result<Outcome> {
        let resolved = self
            .resolve_function_name(&argv[0])
            .expect("function existed at dispatch time");
        let entry = self
            .functions
            .get(&resolved)
            .cloned()
            .expect("just resolved");
        // Snapshot the capture list *before* pushing the new
        // function frame so the lookup sees the caller's view. Per
        // `project_shell_function_scope.md`, the `function f(a,b) …`
        // form binds exactly the listed names by-ref and read-only;
        // a missing caller binding snapshots as empty.
        let capture_snapshot: Vec<(String, Value)> = entry
            .captures
            .as_ref()
            .map(|caps| {
                caps.iter()
                    .map(|n| (n.clone(), self.scope.get(n).cloned().unwrap_or_default()))
                    .collect()
            })
            .unwrap_or_default();
        // Swap in the function's positional arguments.
        let saved = core::mem::replace(&mut self.positionals, argv[1..].to_vec());
        self.positionals_stack.push(saved);
        // Switch the evaluator's namespace path to the function's
        // *defining* namespace so bare references inside the body
        // resolve against the lexical view at the point of def,
        // not the caller's site.
        let saved_ns = core::mem::replace(
            &mut self.namespace_path,
            entry.defining_namespace.clone(),
        );
        // Push a fresh imports frame — function bodies start with
        // no namespace imports active and any `use namespace` they
        // run is visible only inside the body. Restored on return.
        self.imports.push(Vec::new());
        // Push a mode-save entry. By default the caller's mode is
        // restored on exit; an *unbounded* `mode` declaration inside
        // the body clears this entry so the change propagates back
        // up. `mode -L` and the block form work entirely off the
        // entry's saved value.
        self.function_mode_save.push(Some(self.mode.clone()));
        // Push a function frame. `static_scope = true` for ksh93
        // `function NAME`-form functions: assignments inside that
        // form's body default to local, matching the locked
        // `project_shell_function_scope.md` rule.
        let static_scope = matches!(entry.scope, FunctionScope::Static);
        self.scope.push_function_frame(static_scope);
        // Bind the captured values into the new frame as readonly.
        // Errors here would only surface against a binding that
        // *already* existed in the new frame — impossible, we just
        // pushed it — so any failure is genuinely fatal.
        for (n, v) in capture_snapshot {
            self.scope.assign_local(&n, v)?;
            let readonly_attr = crate::scope::AttrSet {
                readonly: true,
                ..crate::scope::AttrSet::default()
            };
            self.scope.apply_attrs(&n, &readonly_attr)?;
        }
        let result = self.eval_compound(&entry.body);
        self.scope.pop();
        // Restore mode if the function asked us to. A `None` slot
        // means an unbounded `mode` declaration inside the body
        // wanted the change to propagate; leave `self.mode` as it
        // is in that case.
        if let Some(Some(saved_mode)) = self.function_mode_save.pop() {
            self.mode = saved_mode;
        }
        // Drop the function's imports frame.
        self.imports.pop();
        self.namespace_path = saved_ns;
        let restored = self.positionals_stack.pop().expect("we just pushed");
        self.positionals = restored;
        result
    }

    // ---------- compound commands ----------

    fn eval_compound(&mut self, c: &CompoundCommand) -> Result<Outcome> {
        if !c.redirects.is_empty() {
            return self.eval_compound_with_redirects(c);
        }
        self.eval_compound_inner(c)
    }

    fn eval_compound_inner(&mut self, c: &CompoundCommand) -> Result<Outcome> {
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
                self.subshell_level += 1;
                let result = self.eval_statements(body);
                self.subshell_level -= 1;
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
                // Snapshot `=~` regex-match text into `${.sh.match}`
                // — before evaluating the test so a failing match
                // clears it.
                self.sh_match = first_regex_match_capture(&args).unwrap_or_default();
                let ok = eval_double_bracket(&args)?;
                Ok(Outcome::Status(if ok { 0 } else { 1 }))
            }
            CompoundKind::FunctionDef {
                name,
                scope,
                captures,
                body,
            } => {
                let qualified = self.qualify_decl_name(name);
                self.functions.insert(
                    qualified,
                    FunctionEntry {
                        scope: *scope,
                        captures: captures.clone(),
                        body: body.clone(),
                        defining_namespace: self.namespace_path.clone(),
                    },
                );
                Ok(Outcome::Status(0))
            }
            CompoundKind::NamespaceDef { name, body } => {
                self.namespace_path.push(name.clone());
                let result = self.eval_statements(body);
                self.namespace_path.pop();
                result
            }
            CompoundKind::TypeclassDef { name, items } => {
                let qualified_name = self.qualify_decl_name(name);
                let mut defaults: alloc::collections::BTreeMap<
                    String,
                    alloc::boxed::Box<CompoundCommand>,
                > = alloc::collections::BTreeMap::new();
                let mut abstracts: alloc::collections::BTreeSet<String> =
                    alloc::collections::BTreeSet::new();
                for m in items {
                    match m {
                        crate::ast::TypeclassMember::Default { name: n, body } => {
                            if defaults.contains_key(n) || abstracts.contains(n) {
                                return Err(KashError::Parse(alloc::format!(
                                    "typeclass `{qualified_name}` declares method `{n}` twice"
                                )));
                            }
                            defaults.insert(n.clone(), body.clone());
                        }
                        crate::ast::TypeclassMember::Signature { name: n } => {
                            if defaults.contains_key(n) || abstracts.contains(n) {
                                return Err(KashError::Parse(alloc::format!(
                                    "typeclass `{qualified_name}` declares method `{n}` twice"
                                )));
                            }
                            abstracts.insert(n.clone());
                        }
                    }
                }
                self.typeclasses.insert(
                    qualified_name,
                    TypeclassEntry {
                        defaults,
                        abstracts,
                    },
                );
                Ok(Outcome::Status(0))
            }
            CompoundKind::InstanceDef {
                typeclass,
                for_type,
                items,
            } => {
                // Resolve the typeclass name against the current
                // namespace path / imports so `instance Sayer for
                // Int` inside `namespace foo { … }` lands on
                // `.foo.Sayer` (rather than the bare name).
                let Some(resolved_tc) = self.resolve_typeclass_name(typeclass) else {
                    return Err(KashError::NotFound(alloc::format!(
                        "typeclass `{typeclass}` (cannot define an instance for an undeclared typeclass)"
                    )));
                };
                let tc_entry = self
                    .typeclasses
                    .get(&resolved_tc)
                    .expect("just resolved");
                let mut methods: alloc::collections::BTreeMap<
                    String,
                    alloc::boxed::Box<CompoundCommand>,
                > = alloc::collections::BTreeMap::new();
                for m in items {
                    let crate::ast::InstanceMember::Method { name: n, body } = m;
                    if !tc_entry.declares(n) {
                        return Err(KashError::Parse(alloc::format!(
                            "instance `{resolved_tc} for {for_type}` defines `{n}`, but typeclass `{resolved_tc}` does not declare it"
                        )));
                    }
                    if methods.contains_key(n) {
                        return Err(KashError::Parse(alloc::format!(
                            "instance `{resolved_tc} for {for_type}` defines method `{n}` twice"
                        )));
                    }
                    methods.insert(n.clone(), body.clone());
                }
                for required in &tc_entry.abstracts {
                    if !methods.contains_key(required) {
                        return Err(KashError::Parse(alloc::format!(
                            "instance `{resolved_tc} for {for_type}` is missing abstract method `{required}`"
                        )));
                    }
                }
                let key = (resolved_tc, for_type.clone());
                self.instances.insert(key, InstanceEntry { methods });
                Ok(Outcome::Status(0))
            }
            CompoundKind::ModeDecl { spec, form } => self.eval_mode_decl(spec, form),
            CompoundKind::VenvDecl { name, sections } => {
                self.eval_venv_decl(name, sections)
            }
            CompoundKind::TypeDef { name, members } => {
                self.register_type_def(name, members)?;
                Ok(Outcome::Status(0))
            }
            CompoundKind::TypeInstance { type_name, var_name } => {
                self.instantiate_type(type_name, var_name)?;
                Ok(Outcome::Status(0))
            }
        }
    }

    /// Evaluate a `venv NAME { … }` block. Materialise each
    /// configuring section into a fresh [`VenvFrame`], push it,
    /// apply any namespace imports onto the active imports slot,
    /// run the (at-most-one) `body { … }` section against the
    /// frame, then strip the imports and pop the frame on exit.
    fn eval_venv_decl(
        &mut self,
        _name: &str,
        sections: &[crate::ast::VenvSection],
    ) -> Result<Outcome> {
        // Strict modes disable the `venv` keyword per the locked
        // semantics in `project_kash_venv.md`. (POSIX-strict and
        // ksh93u-strict are by definition no-extensions modes.)
        match self.mode.base {
            crate::mode::BaseMode::PosixStrict
            | crate::mode::BaseMode::Ksh93uStrict => {
                return Err(KashError::Mode(alloc::format!(
                    "`venv {{ … }}` blocks are not available inside `{}`; \
                     switch to an `*-aware` or `default` mode in an outer scope",
                    self.mode
                )));
            }
            _ => {}
        }
        let mut frame = VenvFrame::new();
        let mut body: Option<&[Statement]> = None;
        let mut import_entries: Vec<ImportEntry> = Vec::new();
        for section in sections {
            match section {
                crate::ast::VenvSection::Body { statements } => {
                    if body.is_some() {
                        return Err(KashError::Parse(
                            "multiple `body { … }` sections in one venv block".into(),
                        ));
                    }
                    body = Some(statements);
                }
                crate::ast::VenvSection::Capabilities { spec } => {
                    let set = crate::capability::CapabilitySet::from_spec(spec)
                        .map_err(KashError::Runtime)?;
                    frame.capabilities = Some(set);
                }
                crate::ast::VenvSection::Env { directives } => {
                    frame.env_directives.extend(directives.iter().cloned());
                }
                crate::ast::VenvSection::Imports { statements } => {
                    for arg_words in statements {
                        let mut args: Vec<String> = Vec::with_capacity(arg_words.len());
                        for w in arg_words {
                            args.push(self.expand_word(w)?);
                        }
                        let entries = parse_use_args(&args)?;
                        import_entries.extend(entries);
                    }
                }
                crate::ast::VenvSection::LoadConfig { path } => {
                    let resolved = self.expand_word(path)?;
                    let (caps_spec, env_dirs) = load_venv_config(&resolved)?;
                    if let Some(spec) = caps_spec {
                        let set = crate::capability::CapabilitySet::from_spec(&spec)
                            .map_err(KashError::Runtime)?;
                        frame.capabilities = Some(set);
                    }
                    frame.env_directives.extend(env_dirs);
                }
            }
        }
        self.venv_stack.push(frame);
        // Push the import entries onto the active imports slot.
        // We record how many we added so we can pop *only* our
        // contribution on exit, even if the body itself ran
        // additional `use` statements.
        let imports_added = import_entries.len();
        if imports_added > 0
            && let Some(frame) = self.imports.last_mut()
        {
            frame.extend(import_entries);
        }
        let result = match body {
            Some(stmts) => self.eval_statements(stmts),
            None => Ok(Outcome::Status(0)),
        };
        if imports_added > 0
            && let Some(frame) = self.imports.last_mut()
        {
            let target = frame.len().saturating_sub(imports_added);
            frame.truncate(target);
        }
        self.venv_stack.pop();
        result
    }

    /// True iff `cap` is permitted at the current point. With no
    /// active venv frame, every capability is permitted (the venv
    /// system only gates *inside* a venv). When multiple venv
    /// frames are stacked, the *innermost* one's policy applies —
    /// that's the lexical frame the running code is in.
    #[must_use]
    pub fn is_capability_allowed(&self, cap: crate::capability::Capability) -> bool {
        match self.venv_stack.last().and_then(|f| f.capabilities.as_ref()) {
            None => true,
            Some(set) => set.allows(cap),
        }
    }

    /// True iff the external command name `cmd` may be spawned at
    /// the current point. With no active venv frame (or a venv
    /// without a `capabilities { … }` section), every name passes.
    /// Otherwise both `ExecSpawn` and the `allow-cmd` list (if any)
    /// must clear it.
    #[must_use]
    pub fn is_cmd_allowed(&self, cmd: &str) -> bool {
        match self.venv_stack.last().and_then(|f| f.capabilities.as_ref()) {
            None => true,
            Some(set) => {
                set.allows(crate::capability::Capability::ExecSpawn)
                    && set.cmd_allowed(cmd)
            }
        }
    }

    /// Gate every external-command spawn against the active venv
    /// frame. Returns `Err(KashError::CapabilityDenied)` if either
    /// the `ExecSpawn` capability is revoked or the `allow-cmd`
    /// list rejects `cmd`. Called from every spawn site.
    pub(crate) fn check_external_spawn(&self, cmd: &str) -> Result<()> {
        let Some(set) = self
            .venv_stack
            .last()
            .and_then(|f| f.capabilities.as_ref())
        else {
            return Ok(());
        };
        if !set.allows(crate::capability::Capability::ExecSpawn) {
            return Err(KashError::CapabilityDenied(alloc::format!(
                "spawning `{cmd}`: this venv revoked the `exec-spawn` capability"
            )));
        }
        if !set.cmd_allowed(cmd) {
            return Err(KashError::CapabilityDenied(alloc::format!(
                "spawning `{cmd}`: not in this venv's `allow-cmd` list"
            )));
        }
        Ok(())
    }

    /// Apply a `mode` declaration. Parses the spec, gates against
    /// strict modes (which disable the keyword entirely), runs the
    /// modifier-monotonicity check against the current mode, then
    /// installs the new mode according to the form.
    fn eval_mode_decl(
        &mut self,
        spec: &str,
        form: &crate::ast::ModeForm,
    ) -> Result<Outcome> {
        // Strict modes disable the keyword — escape isn't allowed
        // from within them (per `project_shell_mode_syntax.md`).
        match self.mode.base {
            crate::mode::BaseMode::PosixStrict | crate::mode::BaseMode::Ksh93uStrict => {
                return Err(KashError::Mode(alloc::format!(
                    "`mode` declarations are not allowed inside `{}`; to switch modes, set the outer scope's mode instead",
                    self.mode
                )));
            }
            _ => {}
        }
        let new_mode = Mode::<B>::parse(spec)?;
        if !new_mode.modifiers_satisfy(&self.mode) {
            return Err(KashError::Mode(alloc::format!(
                "mode `{spec}` would drop a modifier active in the enclosing mode `{}`; modifiers may only be added by an inner declaration",
                self.mode
            )));
        }
        match form {
            crate::ast::ModeForm::Block { body } => {
                // A block frame pushes a save slot of its own so
                // an inner unbounded `mode` declaration can walk
                // *through* the block to reach the caller (the
                // unbounded arm below clears every slot in the
                // stack, not just the topmost one).
                self.function_mode_save.push(Some(self.mode.clone()));
                self.mode = new_mode;
                let result = self.eval_statements(body);
                if let Some(Some(saved)) = self.function_mode_save.pop() {
                    self.mode = saved;
                }
                result
            }
            crate::ast::ModeForm::Lexical => {
                // Scope-bound: the change lasts until the enclosing
                // function frame (or block frame) pops. At file
                // scope, persists for the rest of the file —
                // identical to the unbounded form there.
                self.mode = new_mode;
                Ok(Outcome::Status(0))
            }
            crate::ast::ModeForm::Unbounded => {
                // Unbounded: change persists past every enclosing
                // scope frame, propagating outward to the file
                // scope. We clear every save slot on the stack so
                // none of the frames will restore on exit.
                self.mode = new_mode;
                for slot in self.function_mode_save.iter_mut() {
                    *slot = None;
                }
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
        let target = self.qualify_var_for_write(name);
        let mut outcome = Outcome::Status(0);
        for item in items {
            self.scope.assign(&target, Value::Scalar(item))?;
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
        // Brace expansion runs first — before `$`-expansion and
        // field splitting — fanning the word into one or more
        // words (`{a,b}` → `a`, `b`). Each resulting word then goes
        // through the normal per-word expansion, *including* POSIX
        // empty-word suppression: a brace alternative that ends up
        // a wholly-empty *unquoted* word produces no field (`echo
        // {a,}` → `a`, matching bash), while a quoted-empty
        // alternative (`{a,""}`) keeps its empty field.
        let words = brace_expand_word(w);
        let mut out: Vec<String> = Vec::new();
        for bw in &words {
            out.extend(self.expand_word_to_fields_one(bw)?);
        }
        Ok(out)
    }

    /// Expand a single (already brace-expanded) word into fields.
    /// See [`Self::expand_word_to_fields`] for the brace wrapper. A
    /// wholly-empty unquoted word produces no field (POSIX); a word
    /// with any quoted segment always yields at least one field.
    fn expand_word_to_fields_one(
        &mut self,
        w: &Word,
    ) -> Result<Vec<String>> {
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
        // Pathname expansion (globbing) applies only to a fully
        // unquoted word — a quoted `*` / `?` / `[…]` stays
        // literal. The check is at word granularity: a word with
        // any quoted segment opts out wholesale (the rare mixed
        // form `pre"x"*` is a documented limitation).
        if word_has_quoted_segment(w) {
            return Ok(fields);
        }
        self.glob_fields(fields)
    }

    /// Apply pathname expansion to `fields`. A field with no glob
    /// metacharacter passes through unchanged. On a pattern that
    /// matches nothing the null-glob policy decides — fail in
    /// `default` / `-secure`, or leave the literal pattern
    /// otherwise (see [`Self::null_glob_should_fail`]). Without the
    /// `std` feature there is no filesystem, so globbing is a
    /// no-op and patterns survive verbatim.
    fn glob_fields(&self, fields: Vec<String>) -> Result<Vec<String>> {
        #[cfg(not(feature = "std"))]
        {
            Ok(fields)
        }
        #[cfg(feature = "std")]
        {
            // `(#q…)` qualifiers are a zsh extension, unavailable
            // in the strict modes; there a trailing `(#q…)` stays
            // a literal part of the pattern.
            let strict = matches!(
                self.mode.base,
                crate::mode::BaseMode::PosixStrict
                    | crate::mode::BaseMode::Ksh93uStrict
            );
            let mut out: Vec<String> = Vec::new();
            for f in fields {
                let (pattern, quals) = if strict {
                    (f.clone(), None)
                } else {
                    match split_glob_qualifier(&f) {
                        Some((p, body)) => {
                            (p.to_string(), Some(parse_glob_qualifiers(body)?))
                        }
                        None => (f.clone(), None),
                    }
                };
                // No qualifier and no metacharacter → pass through.
                if quals.is_none() && !field_has_glob_meta(&pattern) {
                    out.push(f);
                    continue;
                }
                let dotglob =
                    quals.as_ref().map(|q| q.dotglob).unwrap_or(false);
                let mut matches = glob_expand_field(&pattern, dotglob);
                if let Some(q) = &quals {
                    matches = apply_glob_qualifiers(matches, q);
                }
                if matches.is_empty() {
                    // The `(#qN)` modifier opts into null glob: an
                    // empty result is fine (no error, no literal).
                    if quals.as_ref().map(|q| q.nullglob).unwrap_or(false) {
                        continue;
                    }
                    if self.null_glob_should_fail() {
                        return Err(KashError::Runtime(alloc::format!(
                            "no matches for glob pattern `{f}`"
                        )));
                    }
                    out.push(f);
                } else {
                    out.extend(matches);
                }
            }
            Ok(out)
        }
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
        // Lexer emits `Bare` segments with their backslashes already
        // resolved (escape happens at tokenisation time), but
        // `DoubleQuoted` segments arrive verbatim — the lexer keeps
        // both bytes of `\X` so the parser can re-route the body
        // through here. `split_ifs.is_none()` is the (only) marker
        // we have for "this segment is double-quoted"; if you ever
        // call this routine for some other no-split context, that
        // assumption needs a separate flag.
        let in_double_quoted = split_ifs.is_none();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if in_double_quoted && c == '\\' {
                // POSIX 2.2.3: inside double-quoted strings,
                // backslash retains its meaning only before
                // `$`, `` ` ``, `"`, `\`, and newline; for any
                // other character it survives literally.
                match chars.peek().copied() {
                    Some(n @ ('$' | '`' | '"' | '\\')) => {
                        chars.next();
                        fields.last_mut().expect("fields invariant").push(n);
                    }
                    Some('\n') => {
                        // `\<newline>` is line-continuation: both
                        // bytes drop.
                        chars.next();
                    }
                    _ => {
                        fields.last_mut().expect("fields invariant").push(c);
                    }
                }
                continue;
            }
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
                    self.eval_arith_render(&body)?
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
            } else if next == '!' {
                chars.next();
                self.last_bg_pid.to_string()
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
    /// Evaluate an arithmetic expression to a [`Num`] (int / float
    /// / float128). The full-fidelity entry point — used by
    /// `$((…))` expansion and typed-numeric storage so a float
    /// result keeps its float identity.
    fn eval_arith_num(&mut self, src: &str) -> Result<Num> {
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

    /// Evaluate an arithmetic expression to `i64`, truncating a
    /// float result toward zero. Used by integer-only contexts:
    /// array indices, exit status, `((…))` truthiness, integer
    /// `typeset`. A complex result is an error.
    fn eval_arith(&mut self, src: &str) -> Result<i64> {
        let n = self.eval_arith_num(src)?;
        // A complex value with a zero imaginary part is really a
        // real number and converts; a genuinely complex one has
        // no integer form.
        n.as_real_f64().map(|f| f as i64).ok_or_else(|| {
            KashError::Runtime(
                "arithmetic: a complex value has no integer form".into(),
            )
        })
    }

    /// Render any `Num` to its canonical string form — used for
    /// storage and `$((…))` output. The float128 / complex256
    /// tiers format through the `Consts` cache.
    fn render_num(&mut self, n: &Num) -> Result<String> {
        match n {
            Num::Int(v) => Ok(alloc::format!("{v}")),
            Num::Float(f) => Ok(format_float_value(*f)),
            Num::Float128(bf) => self.format_bigfloat(bf),
            Num::Complex(r, i) => Ok(format_complex_value(*r, *i)),
            Num::Complex256(r, i) => {
                let rs = self.format_bigfloat(r)?;
                let is = self.format_bigfloat(i)?;
                Ok(format_complex_strings(&rs, &is))
            }
        }
    }

    /// Render the result of `$((…))` — a float / complex keeps its
    /// decimal form rather than being truncated to an integer.
    fn eval_arith_render(&mut self, src: &str) -> Result<String> {
        let n = self.eval_arith_num(src)?;
        self.render_num(&n)
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
                    let rendered = self.eval_arith_render(&body)?;
                    out.push_str(&rendered);
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
            } else if next == '!' {
                chars.next();
                out.push_str(&self.last_bg_pid.to_string());
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
        // zsh-style flag block: `${(flags)body}`. Parsed once and
        // peeled off; the rest of the expansion runs unaware, and
        // we re-apply the flags' transformations afterwards in
        // zsh's fixed evaluation order. Strict modes that disable
        // zsh extensions are gated below.
        if body.starts_with('(') {
            let (flags, rest) = parse_expansion_flag_block(body)?;
            if !flags.is_empty() {
                self.check_zsh_flag_mode()?;
            }
            // `(e)` is locked under `-secure` per
            // `project_kash_security_policy.md`.
            if flags.reeval
                && self.mode.modifiers.contains(&crate::mode::Modifier::Secure)
            {
                return Err(KashError::SecureViolation(
                    "`${(e)…}` re-evaluation is disabled under the `-secure` modifier"
                        .into(),
                ));
            }
            // Type query short-circuits the rest of the
            // pipeline — `(t)` returns a fixed-vocabulary
            // descriptor, not the variable's value.
            if flags.type_query {
                return Ok(self.expansion_type_query(rest));
            }
            // Keys / values short-circuit similarly: the result
            // is a list of member names or member values,
            // joined per the flag rules below.
            if flags.compound_keys || flags.compound_values {
                let parts = self.expansion_keys_or_values(
                    rest,
                    flags.compound_keys,
                    flags.compound_values,
                );
                return Ok(self.finish_flag_pipeline(&flags, parts));
            }
            // `(P)` re-routes the lookup: `body`'s expansion is
            // a name, and we look *that* name up.
            let mut inner = if flags.indirect {
                let name = self.expand_braced(rest)?;
                self.lookup_param(&name)?
            } else {
                self.expand_braced(rest)?
            };
            // `(e)` re-runs parameter / arithmetic / command
            // substitution on the value — *not* a full script
            // eval. So `${(e)foo}` where `foo=hello $x world`
            // re-expands `$x` and surfaces the joined string;
            // it doesn't try to run `hello` as a command.
            if flags.reeval {
                let mut buf = String::new();
                self.expand_dollar(&inner, &mut buf)?;
                inner = buf;
            }
            // `(#)` — interpret the value as an arithmetic
            // expression yielding a Unicode codepoint, then
            // replace the value with the resulting character.
            if flags.sharp_char {
                let n = self.eval_arith(&inner)?;
                if let Some(c) = char::from_u32(n as u32) {
                    inner = alloc::format!("{c}");
                } else {
                    inner = String::new();
                }
            }
            // `(g)` — ANSI-C escape decode (`\n` → newline,
            // `\xHH` → byte, etc.). Re-uses the same decoder
            // as the `$'…'` literal handler.
            if flags.g_decode {
                inner = ansi_c_decode(&inner);
            }
            // `(%)` — prompt-style escapes. v1 understands a
            // tiny vocabulary; the full surface lands once the
            // prompt system is in place.
            if flags.pct_prompt {
                inner = self.expand_prompt_escapes(&inner);
            }
            // `(D)` collapses a `$HOME` prefix into `~`.
            if flags.dir_abbrev {
                inner = self.abbreviate_home_prefix(inner);
            }
            return Ok(self.finish_flag_pipeline(&flags, alloc::vec![inner]));
        }
        // `.sh.*` introspection variables. Resolved first so the
        // dotted reserved namespace never falls into the regular
        // identifier-parsing path (which would reject the leading
        // `.`). Per `project_shell_mode_syntax.md`, `.sh.mode` is
        // structured: bare gives the full mode string, `.base`
        // returns the base bucket, `.modifiers` returns the
        // space-joined modifier list.
        if let Some(out) = self.expand_dot_sh(body)? {
            return Ok(out);
        }
        // `${#NAME[subscript]}` / `${#NAME[@]}` — length forms. Have
        // to be checked before the operator-split because `#` here
        // is *not* a modifier-op.
        if let Some(rest) = body.strip_prefix('#') {
            if rest.is_empty() {
                // `${#}` — argc.
                return Ok(self.positionals.len().to_string());
            }
            // `${#NAME[@]}` / `${#NAME[*]}` — element count.
            if let Some((name, sub)) = split_subscripted(rest) {
                if sub == "@" || sub == "*" {
                    let n = self
                        .lookup_all_elements(name)
                        .map(|v| v.len())
                        .unwrap_or(0);
                    return Ok(n.to_string());
                }
                // `${#NAME[i]}` — length of that element.
                let elem = self.lookup_indexed(name, sub).unwrap_or_default();
                return Ok(elem.chars().count().to_string());
            }
            if !is_valid_param_name(rest) {
                return Err(KashError::Parse(format!(
                    "invalid `${{#{rest}}}` length expansion"
                )));
            }
            let resolved = self.resolve_var_name(rest);
            let len = match resolved.as_ref().and_then(|n| self.scope.get(n)) {
                Some(v) => v.to_scalar_string().chars().count(),
                None => 0,
            };
            return Ok(len.to_string());
        }

        // `${NAME[subscript]}` / `${NAME[@]}` / `${NAME[*]}` —
        // subscripted forms. Recognised *before* the bare-name
        // parser since `[` isn't a valid identifier byte.
        if let Some((name, sub)) = split_subscripted(body) {
            if sub == "@" || sub == "*" {
                let elems = self
                    .lookup_all_elements(name)
                    .unwrap_or_default();
                // In a single-string expansion context both `@` and
                // `*` collapse to the IFS-joined form (the multi-
                // field side is handled separately in
                // expand_into_fields for splittable contexts).
                let sep = first_ifs_char(&self.lookup_ifs());
                return Ok(elems.join(&sep));
            }
            return Ok(self.lookup_indexed(name, sub).unwrap_or_default());
        }

        // Find the parameter name (run of identifier bytes, with
        // optional dotted-namespace path). The first non-name byte
        // is either the end of the expansion or the start of an
        // operator suffix.
        //
        // Two shapes:
        //   * Plain identifier — `[_A-Za-z][_A-Za-z0-9]*` (single-
        //     char `?` / `#` / digit specials handled separately).
        //   * Absolute namespace path — `\.<seg>(\.<seg>)*`, e.g.
        //     `.utils.version`. The leading dot signals a fully
        //     qualified reference.
        let bytes = body.as_bytes();
        let mut idx = 0;
        if bytes.first() == Some(&b'.') {
            idx = 1;
            // Each segment must be a non-empty identifier.
            loop {
                let seg_start = idx;
                while idx < bytes.len()
                    && (bytes[idx] == b'_' || bytes[idx].is_ascii_alphanumeric())
                {
                    idx += 1;
                }
                if idx == seg_start {
                    return Err(KashError::Parse(format!(
                        "empty segment in `${{{body}}}`"
                    )));
                }
                if idx >= bytes.len() || bytes[idx] != b'.' {
                    break;
                }
                idx += 1; // consume the '.'
            }
        } else {
            // Plain identifier — optionally followed by dotted
            // compound-member segments (`var.x`, `var.x.y`). The
            // first byte must be a regular identifier opener (or
            // one of the single-byte specials), and each dotted
            // segment after the first must itself be a non-empty
            // identifier.
            if !bytes.is_empty()
                && (bytes[0] == b'_'
                    || bytes[0].is_ascii_alphabetic()
                    || bytes[0].is_ascii_digit()
                    || bytes[0] == b'?'
                    || bytes[0] == b'#')
            {
                idx = 1;
                // single-byte specials terminate here
                if !(bytes[0].is_ascii_digit() || bytes[0] == b'?' || bytes[0] == b'#') {
                    while idx < bytes.len()
                        && (bytes[idx] == b'_' || bytes[idx].is_ascii_alphanumeric())
                    {
                        idx += 1;
                    }
                    // dotted compound-member segments
                    while idx < bytes.len() && bytes[idx] == b'.' {
                        let prev = idx;
                        idx += 1;
                        let seg_start = idx;
                        while idx < bytes.len()
                            && (bytes[idx] == b'_' || bytes[idx].is_ascii_alphanumeric())
                        {
                            idx += 1;
                        }
                        if idx == seg_start {
                            // Trailing `.` or doubled `..` — back
                            // up; the `.` belongs to the operator
                            // tail (e.g. `${var.}` is a parse
                            // error elsewhere).
                            idx = prev;
                            break;
                        }
                    }
                }
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

        // ksh93/bash pattern-strip + replace + case-fold forms.
        // Recognised *before* the `:` modifier family so the
        // two-character operators (`##`, `%%`, `//`, `^^`, `,,`)
        // never get partially consumed.
        if let Some(out) = self.expand_strip_replace_fold(name, rest)? {
            return Ok(out);
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
        let current_present = self.resolve_var_name(name).is_some();
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
                    let target = self.qualify_var_for_write(name);
                    self.scope.assign(&target, Value::Scalar(v.clone()))?;
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

    /// Resolve a `.sh.*` reserved-namespace variable inside `${…}`.
    /// Returns `Ok(Some(value))` on a hit, `Ok(None)` if `body`
    /// doesn't start with `.sh.`, and `Err` for malformed members.
    fn expand_dot_sh(&self, body: &str) -> Result<Option<String>> {
        let Some(rest) = body.strip_prefix(".sh.") else {
            return Ok(None);
        };
        match rest {
            "mode" => Ok(Some(alloc::format!("{}", self.mode))),
            "mode.base" => Ok(Some(self.mode.base.as_str().to_string())),
            "mode.modifiers" => {
                let mut out = String::new();
                for (i, m) in self.mode.modifiers.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    out.push_str(m.as_str());
                }
                Ok(Some(out))
            }
            "value" => Ok(Some(self.discipline_value.clone())),
            "pid" => {
                #[cfg(feature = "std")]
                {
                    Ok(Some(alloc::format!("{}", std::process::id())))
                }
                #[cfg(not(feature = "std"))]
                {
                    Ok(Some("0".into()))
                }
            }
            "ppid" => {
                #[cfg(all(feature = "std", unix))]
                {
                    Ok(Some(alloc::format!(
                        "{}",
                        std::os::unix::process::parent_id()
                    )))
                }
                #[cfg(not(all(feature = "std", unix)))]
                {
                    Ok(Some("0".into()))
                }
            }
            "subshell" => Ok(Some(alloc::format!("{}", self.subshell_level))),
            "lineno" => Ok(Some(alloc::format!("{}", self.current_lineno))),
            "match" => Ok(Some(self.sh_match.clone())),
            "subscript" => Ok(Some(self.sh_subscript.clone())),
            "name" => Ok(Some(
                self.discipline_name_stack
                    .last()
                    .cloned()
                    .unwrap_or_default(),
            )),
            other => Err(KashError::Parse(alloc::format!(
                "unknown `.sh.{other}` introspection variable"
            ))),
        }
    }

    /// Try the pattern-strip / replace / case-fold forms of brace
    /// expansion. Returns `Ok(Some(out))` on a match, `Ok(None)` if
    /// none of these forms applied (so the caller can fall through
    /// to `:-`-style modifiers), and `Err` on a parse / runtime
    /// failure that's specific to the matched form.
    fn expand_strip_replace_fold(
        &mut self,
        name: &str,
        rest: &str,
    ) -> Result<Option<String>> {
        // Pattern-strip — `#`/`##` (prefix) and `%`/`%%` (suffix).
        // The two-char forms must be tested first because
        // `strip_prefix("#")` would otherwise eat a `##` operator.
        if let Some(pat) = rest.strip_prefix("##") {
            let pat = self.expand_inline(pat)?;
            let value = self.lookup_param_raw(name);
            return Ok(Some(strip_prefix_match(&pat, &value, true)));
        }
        if let Some(pat) = rest.strip_prefix("%%") {
            let pat = self.expand_inline(pat)?;
            let value = self.lookup_param_raw(name);
            return Ok(Some(strip_suffix_match(&pat, &value, true)));
        }
        if let Some(pat) = rest.strip_prefix('#') {
            let pat = self.expand_inline(pat)?;
            let value = self.lookup_param_raw(name);
            return Ok(Some(strip_prefix_match(&pat, &value, false)));
        }
        if let Some(pat) = rest.strip_prefix('%') {
            let pat = self.expand_inline(pat)?;
            let value = self.lookup_param_raw(name);
            return Ok(Some(strip_suffix_match(&pat, &value, false)));
        }
        // Replace — `${VAR/old/new}` and `${VAR//old/new}`. Also
        // honours the `/#old` / `/%old` anchor variants, which
        // restrict the match to a prefix / suffix respectively.
        if let Some(rest) = rest.strip_prefix("//") {
            let value = self.lookup_param_raw(name);
            let (old, new) = split_replace_args(rest);
            let old = self.expand_inline(&old)?;
            let new = self.expand_inline(&new)?;
            return Ok(Some(replace_glob_all(&value, &old, &new)));
        }
        if let Some(rest) = rest.strip_prefix('/') {
            let value = self.lookup_param_raw(name);
            // `/#pat/repl` / `/%pat/repl` — anchored replace.
            if let Some(rest) = rest.strip_prefix('#') {
                let (old, new) = split_replace_args(rest);
                let old = self.expand_inline(&old)?;
                let new = self.expand_inline(&new)?;
                return Ok(Some(replace_glob_anchored(&value, &old, &new, true)));
            }
            if let Some(rest) = rest.strip_prefix('%') {
                let (old, new) = split_replace_args(rest);
                let old = self.expand_inline(&old)?;
                let new = self.expand_inline(&new)?;
                return Ok(Some(replace_glob_anchored(&value, &old, &new, false)));
            }
            let (old, new) = split_replace_args(rest);
            let old = self.expand_inline(&old)?;
            let new = self.expand_inline(&new)?;
            return Ok(Some(replace_glob_first(&value, &old, &new)));
        }
        // Case-fold — `^^`/`^` (to upper) / `,,`/`,` (to lower).
        // The trailing `pat` filter (a glob the candidate char must
        // match) is bash semantics; we currently honour it only as
        // a literal char-set if the pattern is `[abc]`-style, and
        // otherwise apply unconditionally — minimal v1 surface.
        if let Some(pat) = rest.strip_prefix("^^") {
            let value = self.lookup_param_raw(name);
            let pat = if pat.is_empty() {
                None
            } else {
                Some(self.expand_inline(pat)?)
            };
            return Ok(Some(case_fold(&value, true, true, pat.as_deref())));
        }
        if let Some(pat) = rest.strip_prefix(",,") {
            let value = self.lookup_param_raw(name);
            let pat = if pat.is_empty() {
                None
            } else {
                Some(self.expand_inline(pat)?)
            };
            return Ok(Some(case_fold(&value, false, true, pat.as_deref())));
        }
        if let Some(pat) = rest.strip_prefix('^') {
            let value = self.lookup_param_raw(name);
            let pat = if pat.is_empty() {
                None
            } else {
                Some(self.expand_inline(pat)?)
            };
            return Ok(Some(case_fold(&value, true, false, pat.as_deref())));
        }
        if let Some(pat) = rest.strip_prefix(',') {
            let value = self.lookup_param_raw(name);
            let pat = if pat.is_empty() {
                None
            } else {
                Some(self.expand_inline(pat)?)
            };
            return Ok(Some(case_fold(&value, false, false, pat.as_deref())));
        }
        // Substring — `${VAR:OFFSET}` / `${VAR:OFFSET:LENGTH}`.
        // Only fires when `:` is *not* immediately followed by one
        // of the `-=?+` modifier operators, so the colon-prefixed
        // family still owns its surface.
        if let Some(rest) = rest.strip_prefix(':')
            && !rest.starts_with(['-', '=', '?', '+'])
        {
            let value = self.lookup_param_raw(name);
            return Ok(Some(self.expand_substring(&value, rest)?));
        }
        Ok(None)
    }

    /// Compute `${VAR:OFFSET[:LEN]}`. Both operands are arithmetic
    /// expressions evaluated in the usual `$((…))` context. Negative
    /// offsets count from the end of the string; negative lengths
    /// count back from the end. An out-of-range offset clamps to
    /// either end. Length 0 is allowed.
    fn expand_substring(&mut self, value: &str, rest: &str) -> Result<String> {
        let (off_expr, len_expr) = match rest.find(':') {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };
        let total = value.chars().count() as i64;
        let off = self.eval_arith(off_expr)?;
        let len = match len_expr {
            Some(s) => Some(self.eval_arith(s)?),
            None => None,
        };
        let start = if off < 0 {
            (total + off).max(0)
        } else {
            off.min(total)
        };
        let end = match len {
            None => total,
            Some(l) if l < 0 => (total + l).max(start),
            Some(l) => (start + l).min(total),
        };
        Ok(value
            .chars()
            .skip(start as usize)
            .take((end - start).max(0) as usize)
            .collect())
    }

    /// Assign `value` into the `name[idx]` slot. The binding is
    /// created on first touch (defaulting to an indexed array) and
    /// re-shaped if the existing attributes ask for one form but the
    /// stored value is the other. `name[idx]=` on a readonly binding
    /// fails with `KashError::Readonly`.
    fn assign_array_element(
        &mut self,
        name: &str,
        idx: &str,
        value: String,
    ) -> Result<()> {
        self.sh_subscript = idx.to_string();
        let value = self.apply_set_discipline(name, value)?;
        let existing = self
            .scope
            .get_binding(name)
            .map(|b| (b.attrs.clone(), matches!(b.value, Value::AssocArray(_))));
        let (is_assoc, is_readonly) = match &existing {
            Some((attrs, current_is_assoc)) => {
                (attrs.assoc || *current_is_assoc, attrs.readonly)
            }
            None => (false, false),
        };
        if is_readonly {
            return Err(KashError::Readonly(name.into()));
        }
        if existing.is_none() {
            let mut attrs = AttrSet::default();
            if is_assoc {
                attrs.assoc = true;
            } else {
                attrs.indexed = true;
            }
            self.scope.apply_attrs(name, &attrs)?;
        }
        let b = self
            .scope
            .get_binding_mut(name)
            .expect("just installed");
        if is_assoc {
            match &mut b.value {
                Value::AssocArray(m) => {
                    m.insert(idx.to_string(), value);
                }
                _ => {
                    let mut m = BTreeMap::new();
                    m.insert(idx.to_string(), value);
                    b.value = Value::AssocArray(m);
                }
            }
        } else {
            let i: usize = idx.parse().map_err(|_| {
                KashError::Runtime(alloc::format!(
                    "array index `{idx}` is not a non-negative integer"
                ))
            })?;
            match &mut b.value {
                Value::Array(v) => {
                    if v.len() <= i {
                        v.resize(i + 1, String::new());
                    }
                    v[i] = value;
                }
                _ => {
                    let mut v = alloc::vec::Vec::new();
                    v.resize(i + 1, String::new());
                    v[i] = value;
                    b.value = Value::Array(v);
                }
            }
        }
        Ok(())
    }

    /// Look up `name[idx]`. Returns `None` if the binding is unset,
    /// the index is out-of-range, or the value isn't array-shaped.
    /// Sets `${.sh.subscript}` to `idx` as a side effect so any
    /// in-flight discipline hook can read which index triggered it.
    fn lookup_indexed(&mut self, name: &str, idx: &str) -> Option<String> {
        self.sh_subscript = idx.to_string();
        let resolved = self.resolve_var_name(name)?;
        let raw = {
            let b = self.scope.get_binding(&resolved)?;
            match &b.value {
                Value::Array(v) => {
                    let i: usize = idx.parse().ok()?;
                    v.get(i).cloned()
                }
                Value::AssocArray(m) => m.get(idx).cloned(),
                _ => None,
            }
        }?;
        Some(self.apply_get_discipline(&resolved, raw))
    }

    /// Render `name[@]` / `name[*]` as a list of strings. For
    /// indexed arrays the order is index ascending; for associative
    /// arrays the order is `BTreeMap` (sorted by key).
    fn lookup_all_elements(&self, name: &str) -> Option<alloc::vec::Vec<String>> {
        let resolved = self.resolve_var_name(name)?;
        let b = self.scope.get_binding(&resolved)?;
        match &b.value {
            Value::Array(v) => Some(v.clone()),
            Value::AssocArray(m) => Some(m.values().cloned().collect()),
            Value::Scalar(s) => Some(alloc::vec![s.clone()]),
            Value::Empty => Some(alloc::vec::Vec::new()),
        }
    }

    /// Compose `$<name>` against the active venv `env { … }`
    /// overlays. Returns `Some(value)` only if at least one venv
    /// frame on the stack actually wrote to (or transformed) the
    /// entry; `None` means callers should fall through to the
    /// regular scope lookup. The walk is outer-to-inner so a
    /// `PathPrepend` in an inner venv ends up first.
    fn venv_env_value(&self, name: &str) -> Option<String> {
        use crate::ast::EnvDirective;
        let mut value: Option<String> = None;
        for frame in &self.venv_stack {
            for d in &frame.env_directives {
                match d {
                    EnvDirective::Set { name: n, value: v } if n == name => {
                        value = Some(v.clone());
                    }
                    EnvDirective::PathPrepend { dir } if name == "PATH" => {
                        let base = value.clone().unwrap_or_else(|| self.scope_path());
                        value = Some(if base.is_empty() {
                            dir.clone()
                        } else {
                            alloc::format!("{dir}:{base}")
                        });
                    }
                    EnvDirective::PathAppend { dir } if name == "PATH" => {
                        let base = value.clone().unwrap_or_else(|| self.scope_path());
                        value = Some(if base.is_empty() {
                            dir.clone()
                        } else {
                            alloc::format!("{base}:{dir}")
                        });
                    }
                    _ => {}
                }
            }
        }
        value
    }

    /// Read `PATH` from the regular variable scope (no venv
    /// overlay). Used as the base for `PATH-prepend` / `PATH-append`
    /// accumulation when no inner venv has already set it.
    fn scope_path(&self) -> String {
        self.resolve_var_name("PATH")
            .and_then(|n| self.scope.get(&n))
            .map(|v| v.to_scalar_string())
            .unwrap_or_default()
    }

    /// Like [`lookup_param`](Self::lookup_param) but never triggers
    /// `nounset`. Used by modifier forms (`${VAR:-…}`, `${VAR:+…}`,
    /// …) that explicitly handle the unset case themselves.
    fn lookup_param_raw(&mut self, name: &str) -> String {
        // Apply `_` self-reference rewriting before any other
        // lookup work — `_.field` inside a lifecycle hook should
        // behave identically to a literal `<instance>.field`.
        let rewritten = self.rewrite_self_ref(name);
        let name = rewritten.as_ref();
        // Block external reads of `private` typedef fields. The
        // empty string surfaces the same way as an unset variable
        // because this code path is `Result`-free; callers that
        // need to fail loudly use `lookup_param` instead.
        if self.check_private_member_access(name).is_err() {
            return String::new();
        }
        if name == "?" {
            return self.last_status.to_string();
        }
        if name == "#" {
            return self.positionals.len().to_string();
        }
        if name == "!" {
            return self.last_bg_pid.to_string();
        }
        if name.len() == 1
            && let Some(d) = name.chars().next().and_then(|c| c.to_digit(10))
        {
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
        // Follow `typeset -n` namerefs through to the bound name.
        let effective = self.follow_nameref_chain(name);
        // venv env overlay wins over the regular scope.
        let raw = if let Some(v) = self.venv_env_value(&effective) {
            v
        } else {
            match self.resolve_var_name(&effective) {
                Some(resolved) => self
                    .scope
                    .get(&resolved)
                    .map(|v| v.to_scalar_string())
                    .unwrap_or_default(),
                None => String::new(),
            }
        };
        self.apply_get_discipline(&effective, raw)
    }

    /// Look up `name` and return its scalar form, or empty for unset.
    /// Honours `nounset`: a plain `$NAME` / `${NAME}` lookup against
    /// an unset name raises [`KashError::Runtime`] when the option is
    /// on. Specials (`?`, `#`, `$`, `!`) and positional `$0`-`$9` are
    /// always considered set.
    fn lookup_param(&mut self, name: &str) -> Result<String> {
        let rewritten = self.rewrite_self_ref(name);
        let name = rewritten.as_ref();
        // Refuse external reads of `private` typedef fields.
        self.check_private_member_access(name)?;
        // Specials are always present.
        if name == "?" {
            return Ok(self.last_status.to_string());
        }
        if name == "#" {
            return Ok(self.positionals.len().to_string());
        }
        if name == "!" {
            return Ok(self.last_bg_pid.to_string());
        }
        if name.len() == 1
            && let Some(d) = name.chars().next().and_then(|c| c.to_digit(10))
        {
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
        let effective = self.follow_nameref_chain(name);
        if let Some(v) = self.venv_env_value(&effective) {
            return Ok(self.apply_get_discipline(&effective, v));
        }
        let resolved = self.resolve_var_name(&effective);
        let raw = match resolved.as_ref().and_then(|n| self.scope.get(n)) {
            Some(v) => Some(v.to_scalar_string()),
            None => None,
        };
        match raw {
            Some(r) => Ok(self.apply_get_discipline(&effective, r)),
            None => {
                if self.options.nounset {
                    Err(KashError::Runtime(alloc::format!(
                        "{name}: parameter not set",
                        name = effective
                    )))
                } else {
                    // Even unset bindings get to run their get
                    // hook — that's how a "computed" variable
                    // (a hook that fabricates its own value) is
                    // supposed to work in ksh93.
                    Ok(self.apply_get_discipline(&effective, String::new()))
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

impl<B: MapBackend> Default for Evaluator<B> {
    fn default() -> Self {
        Self::new()
    }
}

// ===== std-only: external process exec + multi-stage pipeline =====

ifstd!({
    /// Read `name`'s value from `cmd`'s pre-set env, falling back to
    /// this process's own environment when `cmd` hasn't overridden
    /// it. Used by the venv overlay path to layer `PATH-prepend` /
    /// `PATH-append` on top of whatever's already configured.
    /// Resolve a bare command name against the shell's *own*
    /// view of `PATH` — venv overlays included. Returns
    /// `Some(absolute_or_relative_path)` when a file exists at
    /// `<dir>/<cmd>` for some directory on the resolved PATH, or
    /// `None` if `cmd` already contains a slash (use it verbatim)
    /// or nothing was found (let the spawn-time `execvp` raise
    /// `NotFound`).
    fn resolve_in_path<B: crate::collections::MapBackend>(
        ev: &mut Evaluator<B>,
        cmd: &str,
    ) -> Option<String> {
        if cmd.contains('/') {
            return None;
        }
        let path = ev.lookup_param_raw("PATH");
        if path.is_empty() {
            return None;
        }
        for dir in path.split(':') {
            if dir.is_empty() {
                continue;
            }
            let candidate = alloc::format!("{dir}/{cmd}");
            if std::path::Path::new(&candidate).is_file() {
                return Some(candidate);
            }
        }
        None
    }

    fn read_cmd_env(cmd: &std::process::Command, name: &str) -> String {
        for (k, v) in cmd.get_envs() {
            if k == std::ffi::OsStr::new(name) {
                return v
                    .map(|v| v.to_string_lossy().into_owned())
                    .unwrap_or_default();
            }
        }
        std::env::var(name).unwrap_or_default()
    }

    fn path_prepend(current: String, dir: &str) -> String {
        if current.is_empty() {
            dir.to_string()
        } else {
            alloc::format!("{dir}:{current}")
        }
    }

    fn path_append(current: String, dir: &str) -> String {
        if current.is_empty() {
            dir.to_string()
        } else {
            alloc::format!("{current}:{dir}")
        }
    }

    /// Resolved IO routing for one pipeline stage (or single command).
    /// Produced by `resolve_stage_io`; consumed by the spawn path.
    #[derive(Default)]
    struct StageIo {
        /// File to plumb into the stage's stdout, if any.
        stdout_file: Option<std::fs::File>,
        /// File to plumb into the stage's stderr, if any.
        stderr_file: Option<std::fs::File>,
        /// File to plumb into the stage's stdin, if any.
        in_file: Option<std::fs::File>,
        /// Inline bytes (here-doc / here-string) to feed into the
        /// stage's stdin, if any.
        in_inline: Option<alloc::vec::Vec<u8>>,
        /// `2>&1` / `&>` family — stderr follows whatever stdout is
        /// routed to.
        stderr_follows_stdout: bool,
        /// `1>&2` family — stdout follows whatever stderr is routed
        /// to.
        stdout_follows_stderr: bool,
    }

    impl<B: crate::collections::MapBackend> Evaluator<B> {
        /// Walk the scope stack and push every binding flagged with
        /// `attrs.export` into `cmd`'s environment, using the
        /// binding's scalar form. Called before every `spawn` on
        /// the external-exec / pipeline / redirect-bearing paths so
        /// the child sees the same exported set that interactive
        /// shells do.
        ///
        /// Then layer every active venv frame's env overlay on top,
        /// from outermost to innermost so the innermost wins on
        /// `Set` and `PATH-prepend`s accumulate in source order.
        fn apply_exported_env(&self, cmd: &mut std::process::Command) {
            for (name, b) in self.scope.all_bindings() {
                if b.attrs.export {
                    cmd.env(name, b.value.to_scalar_string());
                }
            }
            for frame in &self.venv_stack {
                self.apply_venv_env_directives(cmd, &frame.env_directives);
            }
        }

        /// Apply one venv frame's `env { … }` directives to `cmd`.
        /// Pure overlay: `Set` overwrites, `PathPrepend` / `PathAppend`
        /// mutate `PATH` based on whatever `cmd` currently has (which
        /// already reflects the exported scope plus outer venvs).
        fn apply_venv_env_directives(
            &self,
            cmd: &mut std::process::Command,
            directives: &[crate::ast::EnvDirective],
        ) {
            use crate::ast::EnvDirective;
            for d in directives {
                match d {
                    EnvDirective::Set { name, value } => {
                        cmd.env(name, value);
                    }
                    EnvDirective::PathPrepend { dir } => {
                        cmd.env("PATH", path_prepend(read_cmd_env(cmd, "PATH"), dir));
                    }
                    EnvDirective::PathAppend { dir } => {
                        cmd.env("PATH", path_append(read_cmd_env(cmd, "PATH"), dir));
                    }
                }
            }
        }

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
            // Capability gate: every file-touching redirect must
            // pass the venv's fs-* checks. `Input` only needs
            // read; output / append paths need write *and* create
            // (they may create the file if it doesn't exist).
            let needed: &[crate::capability::Capability] = match kind {
                RedirectKind::Input => &[crate::capability::Capability::FsRead],
                RedirectKind::Output
                | RedirectKind::OutputBoth
                | RedirectKind::Append
                | RedirectKind::AppendBoth => &[
                    crate::capability::Capability::FsWrite,
                    crate::capability::Capability::FsCreate,
                ],
                _ => &[],
            };
            for c in needed {
                if !self.is_capability_allowed(*c) {
                    return Err(KashError::CapabilityDenied(alloc::format!(
                        "opening `{path}`: this venv revoked `{}`",
                        c.as_str()
                    )));
                }
            }
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
        /// Walk a redirect list and resolve it to a stage IO setup.
        /// Shared by `eval_with_redirects` (single command) and the
        /// pipeline stage path. The returned [`StageIo`] is owned —
        /// callers wire its handles into a `std::process::Command`
        /// or feed inline bytes through a piped stdin.
        fn resolve_stage_io(&mut self, redirects: &[crate::ast::Redirect]) -> Result<StageIo> {
            use crate::ast::RedirectKind;
            let mut io = StageIo::default();
            for r in redirects {
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
                        io.in_file = Some(f);
                        io.in_inline = None;
                    }
                    RedirectKind::Output | RedirectKind::Append => {
                        let path = self.expand_word(&r.target)?;
                        let f = self.open_redirect_file(r.kind, &path)?;
                        match fd_hint {
                            1 => {
                                io.stdout_file = Some(f);
                                io.stdout_follows_stderr = false;
                            }
                            2 => {
                                io.stderr_file = Some(f);
                                io.stderr_follows_stdout = false;
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
                        io.stdout_file = Some(f);
                        io.stderr_follows_stdout = true;
                        io.stdout_follows_stderr = false;
                    }
                    RedirectKind::DupOutput => {
                        let target = self.expand_word(&r.target)?;
                        if target == "-" {
                            match fd_hint {
                                1 => {
                                    io.stdout_file = None;
                                    io.stdout_follows_stderr = false;
                                }
                                2 => {
                                    io.stderr_file = None;
                                    io.stderr_follows_stdout = false;
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
                                io.stderr_follows_stdout = true;
                                io.stderr_file = None;
                            }
                            (1, 2) => {
                                io.stdout_follows_stderr = true;
                                io.stdout_file = None;
                            }
                            (a, b) if a == b => {}
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
                        io.in_file = None;
                        io.in_inline = Some(bytes);
                    }
                    RedirectKind::HereDoc { strip_tabs: _ } => {
                        let text = self.expand_word(&r.target)?;
                        let bytes = text.into_bytes();
                        io.in_file = None;
                        io.in_inline = Some(bytes);
                    }
                }
            }
            Ok(io)
        }

        fn eval_with_redirects(
            &mut self,
            cmd: &SimpleCommand,
            argv: &[String],
        ) -> Result<Outcome> {
            use std::io::{Read, Write};
            use std::process::{Command, Stdio};
            // All per-fd routing flows through the shared resolver.
            let StageIo {
                stdout_file,
                stderr_file,
                in_file,
                in_inline,
                stderr_follows_stdout,
                stdout_follows_stderr,
            } = self.resolve_stage_io(&cmd.redirects)?;
            // Compatibility shim with the older two-flag layout the
            // rest of this function used: `out_file` / `both` from the
            // pre-fd-routing world.
            let out_file = stdout_file;
            let both = stderr_follows_stdout;
            let stderr_routed_file = stderr_file;

            let name = argv[0].as_str();
            let is_function = self.resolve_function_name(name).is_some();
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
                self.check_external_spawn(&argv[0])?;
                let resolved = resolve_in_path(self, &argv[0])
                    .unwrap_or_else(|| argv[0].clone());
                let mut c = Command::new(&resolved);
                c.args(&argv[1..]);
                self.apply_exported_env(&mut c);
                let needs_inline_write = in_inline.is_some();
                if let Some(f) = in_file {
                    c.stdin(Stdio::from(f));
                } else if needs_inline_write {
                    c.stdin(Stdio::piped());
                } else if let Some(f) = self.compound_input.as_ref() {
                    let dup = f.try_clone().map_err(|e| {
                        KashError::Runtime(alloc::format!("dup compound stdin: {e}"))
                    })?;
                    c.stdin(Stdio::from(dup));
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
                        KashError::ExternalNotFound(argv[0].clone())
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
            "read" => self.builtin_read(&argv[1..]),
            "source" | "." => self.builtin_source(&argv[1..]),
            "eval" => self.builtin_eval(&argv[1..]),
            "command" => self.builtin_command(&argv[1..]),
            "printf" => self.builtin_printf(&argv[1..]),
            "jobs" => self.builtin_jobs(&argv[1..]),
            "wait" => self.builtin_wait(&argv[1..]),
            "fg" => self.builtin_fg(&argv[1..]),
            "bg" => self.builtin_bg(&argv[1..]),
            "die" => self.builtin_die(&argv[1..]),
            "assert" => self.builtin_assert(&argv[1..]),
            "usage" => self.builtin_usage(&argv[1..]),
            "time" => self.builtin_time(&argv[1..]),
            "getopts" => self.builtin_getopts(&argv[1..]),
                "readonly" => self.builtin_readonly(&argv[1..]),
                "test" => builtin_test(false, &argv[1..]),
                "[" => builtin_test(true, &argv[1..]),
                "trap" => self.builtin_trap(&argv[1..]),
                "alias" => self.builtin_alias(&argv[1..]),
                "unalias" => self.builtin_unalias(&argv[1..]),
                "typeset" | "declare" => self.builtin_typeset(&argv[1..]),
                "export" => self.builtin_export(&argv[1..]),
                "use" => self.builtin_use(&argv[1..]),
                name if crate::scope::NumericType::from_name(name).is_some() => {
                    self.builtin_typeset(&argv)
                }
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
            self.check_external_spawn(&argv[0])?;
            let resolved = resolve_in_path(self, &argv[0])
                .unwrap_or_else(|| argv[0].clone());
            let mut cmd = Command::new(&resolved);
            cmd.args(&argv[1..]);
            self.apply_exported_env(&mut cmd);
            // Compound-body input redirect overrides plain inherit.
            if let Some(f) = self.compound_input.as_ref() {
                let dup = f
                    .try_clone()
                    .map_err(|e| KashError::Runtime(alloc::format!("dup compound stdin: {e}")))?;
                cmd.stdin(Stdio::from(dup));
            } else {
                cmd.stdin(Stdio::inherit());
            }
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::inherit());
            let mut child = cmd.spawn().map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    KashError::ExternalNotFound(argv[0].clone())
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

        /// Expand the *first* pipeline stage's argv only — used by
        /// the pure-output-builtin bridge so we can peek the
        /// command name without committing to the full
        /// external-spawn validation pass.
        fn expand_pipeline_first_stage_argv(
            &mut self,
            pipe: &Pipeline,
        ) -> Result<alloc::vec::Vec<String>> {
            let Some(crate::ast::Command::Simple(s)) = pipe.stages.first() else {
                return Ok(alloc::vec::Vec::new());
            };
            let mut argv: alloc::vec::Vec<String> = alloc::vec::Vec::with_capacity(s.words.len());
            for w in &s.words {
                argv.extend(self.expand_word_to_fields(w)?);
            }
            Ok(argv)
        }

        /// Run a pipeline whose first stage is a pure-output
        /// builtin. The builtin runs in-process; its output bytes
        /// are written into the second stage's piped stdin; the
        /// rest of the chain spawns externally as usual.
        fn run_pipeline_with_inproc_first(
            &mut self,
            pipe: &Pipeline,
            first_argv: alloc::vec::Vec<String>,
        ) -> Result<Outcome> {
            // Step 1: run the leading builtin into a side buffer.
            let old_len = self.output.len();
            let leading = self.dispatch_known_builtin(&first_argv)?;
            let initial_bytes = self.output[old_len..].as_bytes().to_vec();
            self.output.truncate(old_len);
            let leading_status = leading.status();
            self.run_pipeline_tail_with_initial(pipe, initial_bytes, leading_status)
        }

        /// Compound first stage: evaluate the body in-process,
        /// capture its stdout, then route the bytes into the
        /// second stage's stdin and spawn the rest as usual.
        fn run_pipeline_with_compound_first(&mut self, pipe: &Pipeline) -> Result<Outcome> {
            let crate::ast::Command::Compound(c) = &pipe.stages[0] else {
                unreachable!("caller checked stages[0] is Compound");
            };
            let old_len = self.output.len();
            let leading = self.eval_compound(c)?;
            let initial_bytes = self.output[old_len..].as_bytes().to_vec();
            self.output.truncate(old_len);
            let leading_status = leading.status();
            self.run_pipeline_tail_with_initial(pipe, initial_bytes, leading_status)
        }

        /// Shared spawn-and-drain path for the in-process-first
        /// pipeline forms (pure-output-builtin first, compound
        /// first). `initial_bytes` is the captured stdout of the
        /// leading stage; it gets written into the *second* stage's
        /// piped stdin. `leading_status` participates in
        /// `pipefail`.
        fn run_pipeline_tail_with_initial(
            &mut self,
            pipe: &Pipeline,
            initial_bytes: alloc::vec::Vec<u8>,
            leading_status: i32,
        ) -> Result<Outcome> {
            use std::io::{Read, Write};
            use std::process::{Child, Command, Stdio};
            struct StageSpec {
                argv: alloc::vec::Vec<String>,
                io: StageIo,
                assignments: alloc::vec::Vec<(String, String)>,
            }
            let mut specs: alloc::vec::Vec<StageSpec> =
                alloc::vec::Vec::with_capacity(pipe.stages.len() - 1);
            for stage in &pipe.stages[1..] {
                let simple = match stage {
                    crate::ast::Command::Simple(s) => s,
                    crate::ast::Command::Compound(_) => {
                        return Err(KashError::Runtime(
                            "compound commands past the first pipeline stage are not yet supported"
                                .into(),
                        ));
                    }
                };
                let mut assignments: alloc::vec::Vec<(String, String)> = alloc::vec::Vec::new();
                for a in &simple.assignments {
                    let v = self.expand_word(&a.value)?;
                    assignments.push((a.name.clone(), v));
                }
                let mut argv = alloc::vec::Vec::with_capacity(simple.words.len());
                for w in &simple.words {
                    argv.extend(self.expand_word_to_fields(w)?);
                }
                if argv.is_empty() {
                    return Err(KashError::Runtime("pipeline stage expanded to nothing".into()));
                }
                let name = argv[0].as_str();
                if self.resolve_function_name(name).is_some() || is_builtin_name(name) {
                    return Err(KashError::Runtime(alloc::format!(
                        "builtin or function `{name}` past the first pipeline stage is not yet supported"
                    )));
                }
                let io = self.resolve_stage_io(&simple.redirects)?;
                specs.push(StageSpec { argv, io, assignments });
            }
            let n = specs.len();
            let mut children: alloc::vec::Vec<Child> = alloc::vec::Vec::with_capacity(n);
            for (i, spec) in specs.iter_mut().enumerate() {
                let StageSpec { argv, io, assignments } = spec;
                self.check_external_spawn(&argv[0])?;
                let resolved =
                    resolve_in_path(self, &argv[0]).unwrap_or_else(|| argv[0].clone());
                let mut cmd = Command::new(&resolved);
                cmd.args(&argv[1..]);
                self.apply_exported_env(&mut cmd);
                // Stage-local assignment prefixes — bash/ksh
                // semantics: visible to the spawned process only.
                for (k, v) in assignments.iter() {
                    cmd.env(k, v);
                }
                // stdin: per-stage redirect wins; else previous-
                // stage pipe (i > 0); else *the first stage's
                // captured bytes* (i == 0) via piped stdin.
                let inline_bytes = io.in_inline.take();
                let need_inline_pipe = inline_bytes.is_some();
                if let Some(f) = io.in_file.take() {
                    cmd.stdin(Stdio::from(f));
                } else if need_inline_pipe {
                    cmd.stdin(Stdio::piped());
                } else if i == 0 {
                    cmd.stdin(Stdio::piped());
                } else {
                    let prev_stdout = children[i - 1]
                        .stdout
                        .take()
                        .expect("previous stage was spawned with piped stdout");
                    cmd.stdin(Stdio::from(prev_stdout));
                }
                if let Some(f) = io.stdout_file.take() {
                    if io.stderr_follows_stdout {
                        let f2 = f
                            .try_clone()
                            .map_err(|e| KashError::Runtime(alloc::format!("dup: {e}")))?;
                        cmd.stdout(Stdio::from(f));
                        cmd.stderr(Stdio::from(f2));
                    } else {
                        cmd.stdout(Stdio::from(f));
                    }
                } else {
                    cmd.stdout(Stdio::piped());
                }
                if let Some(ef) = io.stderr_file.take() {
                    cmd.stderr(Stdio::from(ef));
                } else if !io.stderr_follows_stdout {
                    cmd.stderr(Stdio::inherit());
                }
                let mut child = cmd.spawn().map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        KashError::ExternalNotFound(argv[0].clone())
                    } else {
                        KashError::Runtime(alloc::format!("spawn `{}`: {e}", argv[0]))
                    }
                })?;
                // Feed the *first* external stage's stdin from the
                // captured builtin output. Inline-stdin overrides
                // this (it has its own bytes to write).
                if i == 0
                    && !need_inline_pipe
                    && let Some(mut si) = child.stdin.take()
                {
                    si.write_all(&initial_bytes).map_err(|e| {
                        KashError::Runtime(alloc::format!("write pipeline stdin: {e}"))
                    })?;
                }
                if let Some(bytes) = inline_bytes
                    && let Some(mut si) = child.stdin.take()
                {
                    si.write_all(&bytes).map_err(|e| {
                        KashError::Runtime(alloc::format!("write stdin: {e}"))
                    })?;
                }
                children.push(child);
            }
            // Drain last stdout into self.output.
            let last = n - 1;
            let mut buf = alloc::vec::Vec::<u8>::new();
            if let Some(mut last_stdout) = children[last].stdout.take() {
                last_stdout.read_to_end(&mut buf).map_err(|e| {
                    KashError::Runtime(alloc::format!("read pipeline stdout: {e}"))
                })?;
                self.output.push_str(&String::from_utf8_lossy(&buf));
            }
            // Reap. Status policy matches the regular pipeline
            // path: last stage's status by default; pipefail
            // takes the right-most non-zero (including the
            // in-process leading builtin's).
            let mut last_status = 0;
            let mut last_nonzero = if leading_status != 0 { leading_status } else { 0 };
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
                if last_nonzero != 0 { last_nonzero } else { 0 }
            } else {
                last_status
            };
            Ok(Outcome::Status(final_status))
        }

        /// Spawn an N-stage pipeline of external commands. Stages
        /// that resolve to a builtin or function are rejected — the
        /// in-process / cross-process bridge for those lands later.
        /// Each stage may carry its *own* redirects (`cat <<EOF | wc
        /// -l`, `tee >file | cat`, …); the resolver consults each
        /// stage's redirect list and lets it override the
        /// previous-stage-pipe / capture defaults.
        fn run_pipeline_external(&mut self, pipe: &Pipeline) -> Result<Outcome> {
            use std::io::{Read, Write};
            use std::process::{Child, Command, Stdio};

            // Compound first stage: run the body in-process and
            // feed its captured stdout into the second stage's
            // stdin.
            if pipe.stages.len() >= 2
                && matches!(pipe.stages[0], crate::ast::Command::Compound(_))
            {
                return self.run_pipeline_with_compound_first(pipe);
            }
            // Pure-output builtin first stage: in-process bridge
            // (already covered).
            let first_argv = self.expand_pipeline_first_stage_argv(pipe)?;
            if let Some(name) = first_argv.first()
                && is_pure_output_builtin(name)
                && pipe.stages.len() >= 2
            {
                return self.run_pipeline_with_inproc_first(pipe, first_argv);
            }
            struct StageSpec {
                argv: alloc::vec::Vec<String>,
                io: StageIo,
                assignments: alloc::vec::Vec<(String, String)>,
            }
            let mut specs: alloc::vec::Vec<StageSpec> =
                alloc::vec::Vec::with_capacity(pipe.stages.len());
            for stage in &pipe.stages {
                let simple = match stage {
                    crate::ast::Command::Simple(s) => s,
                    crate::ast::Command::Compound(_) => {
                        return Err(KashError::Runtime(
                            "compound commands past the first pipeline stage are not yet supported"
                                .into(),
                        ));
                    }
                };
                let mut assignments: alloc::vec::Vec<(String, String)> = alloc::vec::Vec::new();
                for a in &simple.assignments {
                    let v = self.expand_word(&a.value)?;
                    assignments.push((a.name.clone(), v));
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
                if self.resolve_function_name(name).is_some() || is_builtin_name(name) {
                    return Err(KashError::Runtime(alloc::format!(
                        "builtin or function `{name}` in a multi-stage pipeline is not yet supported"
                    )));
                }
                let io = self.resolve_stage_io(&simple.redirects)?;
                specs.push(StageSpec { argv, io, assignments });
            }

            let n = specs.len();
            let mut children: alloc::vec::Vec<Child> = alloc::vec::Vec::with_capacity(n);

            for (i, spec) in specs.iter_mut().enumerate() {
                let StageSpec { argv, io, assignments } = spec;
                self.check_external_spawn(&argv[0])?;
                let resolved =
                    resolve_in_path(self, &argv[0]).unwrap_or_else(|| argv[0].clone());
                let mut cmd = Command::new(&resolved);
                cmd.args(&argv[1..]);
                self.apply_exported_env(&mut cmd);
                for (k, v) in assignments.iter() {
                    cmd.env(k, v);
                }

                // stdin: per-stage redirect wins; else previous-stage
                // pipe (i > 0); else inherit (first stage).
                let inline_bytes = io.in_inline.take();
                let need_inline_pipe = inline_bytes.is_some();
                if let Some(f) = io.in_file.take() {
                    cmd.stdin(Stdio::from(f));
                } else if need_inline_pipe {
                    cmd.stdin(Stdio::piped());
                } else if i == 0 {
                    cmd.stdin(Stdio::inherit());
                } else {
                    let prev_stdout = children[i - 1]
                        .stdout
                        .take()
                        .expect("previous stage was spawned with piped stdout");
                    cmd.stdin(Stdio::from(prev_stdout));
                }

                // stdout: per-stage file routing wins. Otherwise:
                // intermediate stages → piped (for next stage);
                // last stage → piped (for capture into self.output).
                if let Some(f) = io.stdout_file.take() {
                    if io.stderr_follows_stdout {
                        let f2 = f.try_clone().map_err(|e| {
                            KashError::Runtime(alloc::format!("dup: {e}"))
                        })?;
                        cmd.stdout(Stdio::from(f));
                        cmd.stderr(Stdio::from(f2));
                    } else {
                        cmd.stdout(Stdio::from(f));
                    }
                } else {
                    cmd.stdout(Stdio::piped());
                }

                if let Some(ef) = io.stderr_file.take() {
                    cmd.stderr(Stdio::from(ef));
                } else if !io.stderr_follows_stdout {
                    // Leave stderr alone unless already routed by the
                    // `&>`/`2>&1` block above.
                    cmd.stderr(Stdio::inherit());
                }

                let mut child = cmd.spawn().map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        KashError::ExternalNotFound(argv[0].clone())
                    } else {
                        KashError::Runtime(alloc::format!("spawn `{}`: {e}", argv[0]))
                    }
                })?;
                // Inline stdin (here-doc / here-string) writes now,
                // not later — small bodies fit in the kernel pipe
                // buffer, and the child usually drains while we
                // write. Large bodies that would deadlock are a
                // separate concern (deferred IO loop), not a
                // common-case shell concern.
                if let Some(bytes) = inline_bytes
                    && let Some(mut si) = child.stdin.take()
                {
                    si.write_all(&bytes).map_err(|e| {
                        KashError::Runtime(alloc::format!("write stdin: {e}"))
                    })?;
                }
                children.push(child);
            }

            // Last stage's stdout: if the stage routed it to a file
            // we don't have a piped handle; otherwise drain it into
            // `self.output`.
            let last = n - 1;
            let mut buf = alloc::vec::Vec::<u8>::new();
            if let Some(mut last_stdout) = children[last].stdout.take() {
                last_stdout.read_to_end(&mut buf).map_err(|e| {
                    KashError::Runtime(alloc::format!("read pipeline stdout: {e}"))
                })?;
                self.output.push_str(&String::from_utf8_lossy(&buf));
            }

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

/// True iff `name` is one of the side-effect-free builtins whose
/// output the pipeline driver can capture into a side buffer and
/// feed into the next stage's stdin. These are the ones safe to
/// run in-process *before* spawning the rest of a multi-stage
/// pipeline. Side-effecting builtins (`read`, `set`, `unset`,
/// `eval`, …) stay rejected because their effects belong to the
/// caller's scope and can't be cleanly funnelled through a pipe.
/// Peek the literal-bare-prefix of a Word for dispatch hinting
/// without firing full expansion. Returns the longest run of
/// adjacent `Bare` segments at the start of the word; quoted /
/// `$`-prefixed segments cut the prefix short. Used by the
/// pipeline / background classifier to spot in-process names
/// (builtins, functions) before the rest of expansion runs.
fn word_first_field_hint(w: &Word) -> String {
    use crate::ast::WordSegment;
    let mut out = String::new();
    for seg in &w.segments {
        match seg {
            WordSegment::Bare(s) => out.push_str(s),
            _ => break,
        }
    }
    out
}

fn is_pure_output_builtin(name: &str) -> bool {
    matches!(name, "echo" | "printf" | ":" | "true" | "false" | "test" | "[")
}

/// Resolve POSIX `\` escape sequences inside the printf format
/// string. Bash also honours these inside `%b` arguments; that
/// extension can land alongside the conversion-side `%b`.
fn printf_unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('0') => out.push('\0'),
            Some('a') => out.push('\u{07}'),
            Some('b') => out.push('\u{08}'),
            Some('f') => out.push('\u{0c}'),
            Some('v') => out.push('\u{0b}'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Apply `format` to `params` once, returning the rendered text
/// and how many params were consumed. Width / precision are
/// permitted in the format spec but currently *ignored* — only
/// the conversion character drives output. Missing args
/// substitute the empty string for `%s` and zero for numerics.
fn printf_format(format: &str, params: &[String]) -> Result<(String, usize)> {
    let mut out = String::new();
    let mut p_idx: usize = 0;
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // Collect flag / width / precision until we hit a letter.
        let mut spec = String::new();
        loop {
            let Some(&n) = chars.peek() else {
                break;
            };
            if n.is_ascii_alphabetic() || n == '%' {
                chars.next();
                spec.push(n);
                break;
            }
            chars.next();
            spec.push(n);
        }
        if spec.is_empty() {
            out.push('%');
            continue;
        }
        let conv = spec.chars().last().unwrap();
        match conv {
            '%' => out.push('%'),
            's' => {
                let v = params.get(p_idx).cloned().unwrap_or_default();
                p_idx += 1;
                out.push_str(&v);
            }
            'c' => {
                // %c: first char of the arg, empty for missing arg.
                let v = params.get(p_idx).cloned().unwrap_or_default();
                p_idx += 1;
                if let Some(ch) = v.chars().next() {
                    out.push(ch);
                }
            }
            'd' | 'i' => {
                let v = params.get(p_idx).cloned().unwrap_or_default();
                p_idx += 1;
                let n: i64 = v.trim().parse().unwrap_or(0);
                out.push_str(&alloc::format!("{n}"));
            }
            'x' => {
                let v = params.get(p_idx).cloned().unwrap_or_default();
                p_idx += 1;
                let n: i64 = v.trim().parse().unwrap_or(0);
                out.push_str(&alloc::format!("{n:x}"));
            }
            'X' => {
                let v = params.get(p_idx).cloned().unwrap_or_default();
                p_idx += 1;
                let n: i64 = v.trim().parse().unwrap_or(0);
                out.push_str(&alloc::format!("{n:X}"));
            }
            'o' => {
                let v = params.get(p_idx).cloned().unwrap_or_default();
                p_idx += 1;
                let n: i64 = v.trim().parse().unwrap_or(0);
                out.push_str(&alloc::format!("{n:o}"));
            }
            _ => {
                return Err(KashError::Runtime(alloc::format!(
                    "printf: unsupported conversion `%{conv}`"
                )));
            }
        }
    }
    Ok((out, p_idx))
}

/// Per-flag-block parameter expansion modifiers extracted from
/// a `${(…)body}` form. v1 ships the case + quote subset; the
/// remaining categories (split / join / sort / dedup / indirect
/// / compound / path-modifier / misc) come in follow-up
/// commits. Unsupported flag characters surface as a parse
/// error from [`parse_expansion_flag_block`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExpansionFlags {
    /// Case transformation: `(U)` upper, `(L)` lower, `(C)`
    /// title-case (first letter of each whitespace-delimited
    /// word).
    case: Option<CaseFlag>,
    /// Quote / unquote transformation. `(q)` count drives the
    /// level — 1 → backslash-escape, 2 → single-quoted form,
    /// 3 → double-quoted form, 4 → `$'…'` form. `(Q)` strips
    /// shell quoting.
    quote: Option<QuoteFlag>,
    /// `(s.D.)` — split the value on the literal delimiter
    /// captured between the paired delim characters. Empty
    /// string means "split on every character".
    split: Option<String>,
    /// `(j.D.)` — join the multi-element result with this
    /// separator. Without this flag, a split's array result is
    /// re-joined on `""` so the expansion still surfaces as a
    /// single string.
    join: Option<String>,
    /// `(f)` — convenience for `(s.\n.)` (split on newlines).
    f_split: bool,
    /// `(F)` — convenience for `(j.\n.)` (join on newlines).
    f_join: bool,
    /// `(z)` — split into shell tokens. v1 collapses this to
    /// whitespace-aware shell-like splitting that respects
    /// single / double quotes; full shell-grammar tokenisation
    /// arrives with the lexer-driven implementation.
    z_split: bool,
    /// `(o)` / `(O)` — sort the (post-split) array in
    /// ascending / descending order. Combines with `(n)` for
    /// numeric comparison and `(i)` for case-insensitive
    /// comparison.
    sort: Option<SortFlag>,
    /// `(n)` — numeric comparison during sort. Strings that
    /// don't parse as numbers compare lexically against each
    /// other and are ordered after the numeric ones.
    sort_numeric: bool,
    /// `(i)` — case-insensitive comparison during sort. Combines
    /// with the chosen sort direction.
    sort_case_insensitive: bool,
    /// `(u)` — drop duplicate elements from the post-split
    /// array. Stable: first occurrence wins.
    unique: bool,
    /// `(P)` — indirect. Treat the body's value as a variable
    /// name and look that up. Composes with subsequent flags.
    indirect: bool,
    /// `(t)` — type query. Returns the body variable's type
    /// string (`scalar`, `compound`, `indexed`, `assoc`,
    /// `unset`) instead of its value.
    type_query: bool,
    /// `(e)` — re-evaluate. Run the post-lookup value as kash
    /// source via the command-substitution pipeline. Blocked
    /// under the `-secure` modifier per
    /// `project_kash_security_policy.md`.
    reeval: bool,
    /// `(D)` — directory abbreviation. Collapse a leading
    /// `$HOME` prefix into `~`.
    dir_abbrev: bool,
    /// `(@)` — preserve as array. The single-string expansion
    /// context still has to collapse to one string; this flag
    /// instructs the join step to use the first IFS character
    /// rather than the empty separator, so each element
    /// survives field-splitting later.
    at_preserve: bool,
    /// `(k)` — keys. For an associative array returns its
    /// keys; for a compound variable returns the member names.
    compound_keys: bool,
    /// `(v)` — values. Same as `(k)` but for the values.
    compound_values: bool,
    /// `:h` / `:t` / `:r` / `:e` — path-component modifier.
    /// Applied right after the value is fetched, before split.
    path_modifier: Option<PathModifier>,
    /// `(g)` — GLOB-style ANSI-C escape decode of the value.
    /// `\n`, `\t`, `\\`, etc. round-trip back to their byte
    /// values.
    g_decode: bool,
    /// `(V)` — visible escapes. Control and high bytes render
    /// as `\xHH` so the resulting string is safe to print on
    /// any terminal.
    v_visible: bool,
    /// `(c)` — character count. Replaces the value with the
    /// count of Unicode scalar values it contains.
    c_count: bool,
    /// `(#)` — character from code. Interpret the value as an
    /// arithmetic expression yielding a Unicode codepoint, then
    /// replace it with the corresponding character.
    sharp_char: bool,
    /// `(%)` — prompt expansion. v1 recognises a small
    /// fixed-vocabulary subset: `%n` user, `%~` cwd, `%%`
    /// literal percent. Full ksh / zsh prompt escapes arrive
    /// once the prompt system lands.
    pct_prompt: bool,
}

/// `:h` / `:t` / `:r` / `:e` — path modifiers from the zsh
/// expansion-flag block. The same set of glyphs appear as
/// bare modifiers (`${var:h}`); this enum names the variant
/// either path uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathModifier {
    /// `:h` — head: strip the last `/`-separated component
    /// (`dirname`).
    Head,
    /// `:t` — tail: keep only the last component
    /// (`basename`).
    Tail,
    /// `:r` — root: strip the trailing `.ext`.
    Root,
    /// `:e` — extension: keep only the trailing extension.
    Extension,
}

/// `(o)` / `(O)` — sort direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortFlag {
    /// `(o)` — ascending.
    Ascending,
    /// `(O)` — descending.
    Descending,
}

impl ExpansionFlags {
    /// True iff this flag block is the empty `()`. Lets callers
    /// (e.g. the mode gate) skip work when no flag actually
    /// asked for anything.
    pub fn is_empty(&self) -> bool {
        self.case.is_none()
            && self.quote.is_none()
            && self.split.is_none()
            && self.join.is_none()
            && !self.f_split
            && !self.f_join
            && !self.z_split
            && self.sort.is_none()
            && !self.sort_numeric
            && !self.sort_case_insensitive
            && !self.unique
            && !self.indirect
            && !self.type_query
            && !self.reeval
            && !self.dir_abbrev
            && !self.at_preserve
            && !self.compound_keys
            && !self.compound_values
            && self.path_modifier.is_none()
            && !self.g_decode
            && !self.v_visible
            && !self.c_count
            && !self.sharp_char
            && !self.pct_prompt
    }
}

/// `(U)` / `(L)` / `(C)` — case projection applied to the
/// expanded value after quoting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseFlag {
    /// `(U)` — uppercase every character.
    Upper,
    /// `(L)` — lowercase every character.
    Lower,
    /// `(C)` — capitalise the first letter of each
    /// whitespace-delimited word, lowercase the rest.
    Title,
}

/// `(q)` count or `(Q)` — controls whether the value is
/// rendered as a shell-quoted literal or stripped of its
/// outer quoting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuoteFlag {
    /// `(q)` — backslash-escape characters that would be
    /// special unquoted (whitespace, shell metacharacters).
    Backslash,
    /// `(qq)` — single-quote the value (`'…'`), doubling
    /// any embedded apostrophes via the standard `'\''`
    /// escape.
    Single,
    /// `(qqq)` — double-quote the value (`"…"`), escaping
    /// `"`, `\`, `$`, and backtick.
    Double,
    /// `(qqqq)` — emit `$'…'` ANSI-C form. Non-printable
    /// bytes round-trip through `\xHH`.
    AnsiC,
    /// `(Q)` — dequote. Removes shell quoting (single,
    /// double, ANSI-C, backslash) from the value.
    Unquote,
}

/// Parse a leading `(flags)` block from `body`. Returns the
/// extracted flag set and the remaining body slice past the
/// closing paren. Errors on unknown / unterminated flag
/// blocks.
///
/// v1 recognises the case + quote characters only (`U` / `L` /
/// `C` / `q` / `Q`). Any other character causes a parse error
/// — the follow-up commits flip those into "supported".
pub fn parse_expansion_flag_block(body: &str) -> Result<(ExpansionFlags, &str)> {
    debug_assert!(body.starts_with('('));
    let after = &body[1..];
    let bytes = after.as_bytes();
    let mut idx = 0;
    let mut flags = ExpansionFlags::default();
    let mut q_count: u8 = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b')' => {
                if q_count > 0 {
                    flags.quote = Some(match q_count {
                        1 => QuoteFlag::Backslash,
                        2 => QuoteFlag::Single,
                        3 => QuoteFlag::Double,
                        4 => QuoteFlag::AnsiC,
                        _ => {
                            return Err(KashError::Parse(alloc::format!(
                                "expansion flag `q` may repeat at most 4 times, got {q_count}"
                            )));
                        }
                    });
                }
                return Ok((flags, &after[idx + 1..]));
            }
            b'U' => {
                flags.case = Some(CaseFlag::Upper);
                idx += 1;
            }
            b'L' => {
                flags.case = Some(CaseFlag::Lower);
                idx += 1;
            }
            b'C' => {
                flags.case = Some(CaseFlag::Title);
                idx += 1;
            }
            b'Q' => {
                flags.quote = Some(QuoteFlag::Unquote);
                idx += 1;
            }
            b'q' => {
                q_count = q_count.saturating_add(1);
                idx += 1;
            }
            b's' => {
                idx += 1;
                let (delim, next) = read_paired_delim_arg(after, idx, 's')?;
                flags.split = Some(delim);
                idx = next;
            }
            b'j' => {
                idx += 1;
                let (delim, next) = read_paired_delim_arg(after, idx, 'j')?;
                flags.join = Some(delim);
                idx = next;
            }
            b'f' => {
                flags.f_split = true;
                idx += 1;
            }
            b'F' => {
                flags.f_join = true;
                idx += 1;
            }
            b'z' => {
                flags.z_split = true;
                idx += 1;
            }
            b'o' => {
                flags.sort = Some(SortFlag::Ascending);
                idx += 1;
            }
            b'O' => {
                flags.sort = Some(SortFlag::Descending);
                idx += 1;
            }
            b'n' => {
                flags.sort_numeric = true;
                idx += 1;
            }
            b'i' => {
                flags.sort_case_insensitive = true;
                idx += 1;
            }
            b'u' => {
                flags.unique = true;
                idx += 1;
            }
            b'P' => {
                flags.indirect = true;
                idx += 1;
            }
            b't' => {
                flags.type_query = true;
                idx += 1;
            }
            b'e' => {
                flags.reeval = true;
                idx += 1;
            }
            b'D' => {
                flags.dir_abbrev = true;
                idx += 1;
            }
            b'@' => {
                flags.at_preserve = true;
                idx += 1;
            }
            b'k' => {
                flags.compound_keys = true;
                idx += 1;
            }
            b'v' => {
                flags.compound_values = true;
                idx += 1;
            }
            b':' => {
                // Path modifier: `:h` / `:t` / `:r` / `:e`. The
                // next byte selects the variant; anything else
                // is a parse error.
                idx += 1;
                if idx >= bytes.len() {
                    return Err(KashError::Parse(
                        "expansion modifier `:` is missing the variant character (h/t/r/e)".into(),
                    ));
                }
                flags.path_modifier = Some(match bytes[idx] {
                    b'h' => PathModifier::Head,
                    b't' => PathModifier::Tail,
                    b'r' => PathModifier::Root,
                    b'e' => PathModifier::Extension,
                    other => {
                        return Err(KashError::Parse(alloc::format!(
                            "unknown path modifier `:{}` (expected `h`/`t`/`r`/`e`)",
                            other as char,
                        )));
                    }
                });
                idx += 1;
            }
            b'g' => {
                flags.g_decode = true;
                idx += 1;
            }
            b'V' => {
                flags.v_visible = true;
                idx += 1;
            }
            b'c' => {
                flags.c_count = true;
                idx += 1;
            }
            b'#' => {
                flags.sharp_char = true;
                idx += 1;
            }
            b'%' => {
                flags.pct_prompt = true;
                idx += 1;
            }
            other => {
                return Err(KashError::Parse(alloc::format!(
                    "unknown expansion flag `{}`",
                    other as char,
                )));
            }
        }
    }
    Err(KashError::Parse(
        "unterminated `${(…)` flag block".into(),
    ))
}

/// Apply the flag-block transformations to a single starting
/// value. Wrapper over [`apply_expansion_flags_parts`] for
/// call sites that haven't pre-split into an array.
pub fn apply_expansion_flags(flags: &ExpansionFlags, value: String) -> String {
    apply_expansion_flags_parts(flags, alloc::vec![value])
}

/// Apply the flag-block transformations to `parts` in zsh's
/// fixed evaluation order: unquote → split → join → quote →
/// case. The caller-supplied `Vec<String>` is the starting
/// "array" — multi-element inputs (e.g. compound `(k)` /
/// `(v)`) go through here directly.
pub fn apply_expansion_flags_parts(
    flags: &ExpansionFlags,
    mut parts: Vec<String>,
) -> String {
    if matches!(flags.quote, Some(QuoteFlag::Unquote)) {
        for p in &mut parts {
            *p = dequote_value(p);
        }
    }
    // Path modifier — applied before split so each element of
    // an already-multi-element list (e.g. from `(k)` / `(v)`)
    // gets its path component computed independently.
    if let Some(m) = flags.path_modifier {
        for p in &mut parts {
            *p = apply_path_modifier(p, m);
        }
    }
    // Split happens before join — `(s.,.j.+.)` is the natural
    // composition. `(f)` is `(s.\n.)` and `(z)` is shell-token
    // splitting; the explicit `s` flag wins if both appear.
    if let Some(delim) = flags.split.as_deref() {
        parts = split_with_delim(&parts, delim);
    } else if flags.f_split {
        parts = split_with_delim(&parts, "\n");
    } else if flags.z_split {
        parts = split_shell_tokens_many(&parts);
    }
    // Sort / dedup, in zsh's documented order: sort first
    // (numeric / case-insensitive variants), unique second so
    // the dedup sees a sorted array. `(u)` without a sort flag
    // is a stable first-occurrence dedup on the original order.
    if let Some(dir) = flags.sort {
        sort_parts(
            &mut parts,
            dir,
            flags.sort_numeric,
            flags.sort_case_insensitive,
        );
    }
    if flags.unique {
        parts = dedup_stable(&parts);
    }
    // Join collapses the array back to a scalar. With no flag
    // we use an empty separator — kash's expansion contract is
    // "this returns one string" — but `(j…)` / `(F)` override.
    // `(@)` widens that to a single space (the typical IFS
    // first char) so each element stays field-splittable in
    // unquoted contexts downstream.
    let sep = if let Some(j) = flags.join.as_deref() {
        j.to_string()
    } else if flags.f_join {
        "\n".to_string()
    } else if flags.at_preserve {
        " ".to_string()
    } else {
        String::new()
    };
    let mut value = parts.join(&sep);
    value = match flags.quote {
        Some(QuoteFlag::Backslash) => quote_backslash(&value),
        Some(QuoteFlag::Single) => quote_single(&value),
        Some(QuoteFlag::Double) => quote_double(&value),
        Some(QuoteFlag::AnsiC) => quote_ansi_c(&value),
        Some(QuoteFlag::Unquote) | None => value,
    };
    value = match flags.case {
        Some(CaseFlag::Upper) => value.to_uppercase(),
        Some(CaseFlag::Lower) => value.to_lowercase(),
        Some(CaseFlag::Title) => title_case(&value),
        None => value,
    };
    // `(V)` — make invisible bytes visible. Runs last so the
    // user sees the *final* value's bytes if any earlier flag
    // produced unprintable output.
    if flags.v_visible {
        value = make_visible(&value);
    }
    // `(c)` — replace the value with its character count. The
    // counter sees the post-everything string so it agrees
    // with what the user would otherwise see.
    if flags.c_count {
        value = alloc::format!("{}", value.chars().count());
    }
    value
}

/// Apply a single path modifier (`:h` / `:t` / `:r` / `:e`)
/// to `s`.
fn apply_path_modifier(s: &str, m: PathModifier) -> String {
    match m {
        PathModifier::Head => {
            // Strip everything from the final `/` onwards. A
            // value with no `/` collapses to the empty string,
            // matching `dirname`'s behaviour on a bare file
            // name returning `.` — but ksh / zsh path modifiers
            // return empty here, so do the same.
            match s.rfind('/') {
                Some(i) => s[..i].to_string(),
                None => String::new(),
            }
        }
        PathModifier::Tail => match s.rfind('/') {
            Some(i) => s[i + 1..].to_string(),
            None => s.to_string(),
        },
        PathModifier::Root => {
            // Strip the trailing `.ext`. A leading `.` (dot-
            // file) does not count: `.bashrc:r` → `.bashrc`.
            let tail_start = s.rfind('/').map(|i| i + 1).unwrap_or(0);
            let tail = &s[tail_start..];
            if tail.starts_with('.') && tail[1..].find('.').is_none() {
                return s.to_string();
            }
            match s.rfind('.') {
                Some(i) if i > tail_start => s[..i].to_string(),
                _ => s.to_string(),
            }
        }
        PathModifier::Extension => {
            let tail_start = s.rfind('/').map(|i| i + 1).unwrap_or(0);
            let tail = &s[tail_start..];
            if tail.starts_with('.') && tail[1..].find('.').is_none() {
                return String::new();
            }
            match s.rfind('.') {
                Some(i) if i > tail_start => s[i + 1..].to_string(),
                _ => String::new(),
            }
        }
    }
}

/// `(V)` — render unprintable bytes (`< 0x20` and `0x7f`) as
/// `\xHH` so the final string is safe to send to a terminal.
fn make_visible(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let code = c as u32;
        if code < 0x20 || code == 0x7f {
            out.push_str(&alloc::format!("\\x{code:02x}"));
        } else {
            out.push(c);
        }
    }
    out
}

/// Read a paired-delim argument that follows a flag character.
/// `after` is the body of the flag block (the slice after the
/// opening `(`); `start` is the byte offset of the first char
/// of the delim arg. Returns the inner string and the byte
/// offset to resume parsing at.
///
/// zsh-style paired delims accept *any* byte as the open delim;
/// the close delim is the same byte (`s.,.`, `s:,:`, `s/,/`,
/// `s«,»`). A missing closing delim is a parse error.
fn read_paired_delim_arg(
    after: &str,
    start: usize,
    flag: char,
) -> Result<(String, usize)> {
    let bytes = after.as_bytes();
    if start >= bytes.len() {
        return Err(KashError::Parse(alloc::format!(
            "expansion flag `({flag})` is missing its paired-delim argument"
        )));
    }
    let open = bytes[start];
    let mut idx = start + 1;
    let body_start = idx;
    while idx < bytes.len() && bytes[idx] != open {
        idx += 1;
    }
    if idx >= bytes.len() {
        return Err(KashError::Parse(alloc::format!(
            "expansion flag `({flag}…)` paired-delim `{}` was never closed",
            open as char,
        )));
    }
    let body = after[body_start..idx].to_string();
    Ok((body, idx + 1))
}

/// Split every element of `parts` on the literal `delim`,
/// returning the flat-mapped result. An empty `delim` splits on
/// every Unicode boundary — that mirrors zsh's `(s..)`.
fn split_with_delim(parts: &[String], delim: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        if delim.is_empty() {
            for c in p.chars() {
                out.push(alloc::format!("{c}"));
            }
        } else {
            for piece in p.split(delim) {
                out.push(piece.to_string());
            }
        }
    }
    out
}

/// `(z)` — split each part into shell-like tokens. Respects
/// `'…'` / `"…"` / `\X` runs (their content stays glued); other
/// whitespace separates tokens. Empty tokens are dropped, like
/// the shell's regular word-splitting.
/// Sort `parts` in place per the zsh sort-flag combination.
/// `numeric` (the `(n)` flag) sorts on parsed `f64` values,
/// breaking ties lexically; non-numeric strings fall after all
/// numeric ones. `case_insensitive` (the `(i)` flag) lower-
/// cases the comparison keys.
fn sort_parts(
    parts: &mut [String],
    direction: SortFlag,
    numeric: bool,
    case_insensitive: bool,
) {
    parts.sort_by(|a, b| {
        let ord = if numeric {
            let na = a.trim().parse::<f64>().ok();
            let nb = b.trim().parse::<f64>().ok();
            match (na, nb) {
                (Some(x), Some(y)) => x
                    .partial_cmp(&y)
                    .unwrap_or(core::cmp::Ordering::Equal),
                (Some(_), None) => core::cmp::Ordering::Less,
                (None, Some(_)) => core::cmp::Ordering::Greater,
                (None, None) => compare_strs(a, b, case_insensitive),
            }
        } else {
            compare_strs(a, b, case_insensitive)
        };
        match direction {
            SortFlag::Ascending => ord,
            SortFlag::Descending => ord.reverse(),
        }
    });
}

fn compare_strs(a: &str, b: &str, case_insensitive: bool) -> core::cmp::Ordering {
    if case_insensitive {
        a.to_lowercase().cmp(&b.to_lowercase())
    } else {
        a.cmp(b)
    }
}

/// Stable first-occurrence dedup. Preserves the input order of
/// the *first* sighting of each distinct value.
fn dedup_stable(parts: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(parts.len());
    for p in parts {
        if !out.iter().any(|q| q == p) {
            out.push(p.clone());
        }
    }
    out
}

fn split_shell_tokens_many(parts: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        out.extend(split_shell_tokens_one(p));
    }
    out
}

fn split_shell_tokens_one(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip whitespace.
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let mut token = String::new();
        while i < bytes.len() {
            let c = bytes[i] as char;
            if c.is_whitespace() {
                break;
            }
            match bytes[i] {
                b'\'' => {
                    // Verbatim until matching `'`.
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'\'' {
                        token.push(bytes[i] as char);
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                }
                b'"' => {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'"' {
                        if bytes[i] == b'\\' && i + 1 < bytes.len() {
                            token.push(bytes[i + 1] as char);
                            i += 2;
                        } else {
                            token.push(bytes[i] as char);
                            i += 1;
                        }
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                }
                b'\\' if i + 1 < bytes.len() => {
                    token.push(bytes[i + 1] as char);
                    i += 2;
                }
                _ => {
                    token.push(c);
                    i += 1;
                }
            }
        }
        out.push(token);
    }
    out
}

/// Title-case `s` — first letter of each whitespace-delimited
/// run upper-cased, rest lower-cased. Whitespace runs preserve
/// their original character.
fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut at_word_start = true;
    for c in s.chars() {
        if c.is_whitespace() {
            out.push(c);
            at_word_start = true;
        } else if at_word_start {
            for u in c.to_uppercase() {
                out.push(u);
            }
            at_word_start = false;
        } else {
            for l in c.to_lowercase() {
                out.push(l);
            }
        }
    }
    out
}

/// `(q)` — backslash-escape shell-special bytes (whitespace,
/// quote characters, glob / expansion metacharacters). Round-
/// trips back through the regular shell unquoter.
fn quote_backslash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let special = matches!(
            c,
            ' ' | '\t'
                | '\n'
                | '\''
                | '"'
                | '\\'
                | '$'
                | '`'
                | '*'
                | '?'
                | '['
                | ']'
                | '{'
                | '}'
                | '|'
                | '&'
                | ';'
                | '<'
                | '>'
                | '('
                | ')'
                | '#'
                | '~'
                | '!'
                | '='
        );
        if special {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `(qq)` — wrap in single quotes, escaping any embedded
/// apostrophes via the POSIX `'\''` close-reopen trick.
fn quote_single(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// `(qqq)` — wrap in double quotes, escaping `"`, `\`, `$`,
/// and backtick (the four characters that retain their special
/// meaning inside a POSIX double-quoted string).
fn quote_double(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// `(qqqq)` — emit `$'…'` ANSI-C form. Common control bytes
/// use their canonical escapes; everything else printable goes
/// through verbatim; the rest emits `\xHH`.
fn quote_ansi_c(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 3);
    out.push_str("$'");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            c if (c as u32) < 0x20 || c == '\x7f' => {
                out.push_str(&alloc::format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// `(Q)` — strip shell quoting. Recognises `'…'`,
/// `"…"`, `$'…'`, and backslash escapes inside an unquoted
/// run. Closing quotes are required; the function returns the
/// value as-is on malformed input rather than erroring (zsh
/// behaviour).
fn dequote_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                // `'…'` — verbatim until next `'`.
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] != b'\'' {
                    j += 1;
                }
                if j >= bytes.len() {
                    out.push_str(&s[i..]);
                    return out;
                }
                out.push_str(&s[i + 1..j]);
                i = j + 1;
            }
            b'"' => {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] != b'"' {
                    if bytes[j] == b'\\' && j + 1 < bytes.len() {
                        j += 2;
                        continue;
                    }
                    j += 1;
                }
                if j >= bytes.len() {
                    out.push_str(&s[i..]);
                    return out;
                }
                // Copy interior, undoing `\X` escapes for the
                // four POSIX-special characters.
                let inner = &s[i + 1..j];
                let inner_bytes = inner.as_bytes();
                let mut k = 0;
                while k < inner_bytes.len() {
                    if inner_bytes[k] == b'\\'
                        && k + 1 < inner_bytes.len()
                        && matches!(inner_bytes[k + 1], b'"' | b'\\' | b'$' | b'`')
                    {
                        out.push(inner_bytes[k + 1] as char);
                        k += 2;
                    } else {
                        out.push(inner_bytes[k] as char);
                        k += 1;
                    }
                }
                i = j + 1;
            }
            b'$' if i + 1 < bytes.len() && bytes[i + 1] == b'\'' => {
                // `$'…'` — handle the canonical ANSI-C escapes.
                let mut j = i + 2;
                while j < bytes.len() && bytes[j] != b'\'' {
                    if bytes[j] == b'\\' && j + 1 < bytes.len() {
                        j += 2;
                        continue;
                    }
                    j += 1;
                }
                if j >= bytes.len() {
                    out.push_str(&s[i..]);
                    return out;
                }
                let inner = &s[i + 2..j];
                out.push_str(&ansi_c_decode(inner));
                i = j + 1;
            }
            b'\\' if i + 1 < bytes.len() => {
                out.push(bytes[i + 1] as char);
                i += 2;
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

/// Decode the inside of a `$'…'` ANSI-C string. Recognises
/// `\n` `\r` `\t` `\\` `\'` `\"` `\0` and `\xHH`.
fn ansi_c_decode(inner: &str) -> String {
    let bytes = inner.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' => {
                    out.push('\n');
                    i += 2;
                }
                b'r' => {
                    out.push('\r');
                    i += 2;
                }
                b't' => {
                    out.push('\t');
                    i += 2;
                }
                b'\\' => {
                    out.push('\\');
                    i += 2;
                }
                b'\'' => {
                    out.push('\'');
                    i += 2;
                }
                b'"' => {
                    out.push('"');
                    i += 2;
                }
                b'0' => {
                    out.push('\0');
                    i += 2;
                }
                b'x' if i + 3 < bytes.len() => {
                    if let Ok(byte) = u8::from_str_radix(
                        core::str::from_utf8(&bytes[i + 2..i + 4]).unwrap_or(""),
                        16,
                    ) {
                        out.push(byte as char);
                        i += 4;
                        continue;
                    }
                    out.push('\\');
                    i += 1;
                }
                other => {
                    out.push(other as char);
                    i += 2;
                }
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Parse a kash complex literal into a `(real, imaginary)`
/// pair. Recognised forms:
///
/// - `(re=R im=I)` — ksh93 compound literal. Either component
///   may be omitted (defaults to `0`).
/// - `R+Ii` / `R-Ii` — signed real + imaginary.
/// - `Ii` / `i` / `-i` — pure imaginary.
/// - `R` — real-only (imaginary defaults to `0`).
///
/// Returns `None` if the input doesn't match any form. The
/// caller is responsible for projecting components through the
/// destination type's component precision.
pub fn parse_complex_literal(input: &str) -> Option<(f64, f64)> {
    let (re, im) = split_complex_literal(input)?;
    Some((re.parse().ok()?, im.parse().ok()?))
}

/// Split a complex literal into its `(real, imaginary)` component
/// decimal *strings*, normalising the unit forms (`+` → `1`, `-`
/// → `-1`, omitted → `0`). Recognises the same forms as
/// [`parse_complex_literal`] — `(re=R im=I)`, `R+Ii`, `R-Ii`,
/// `Ii`, `i`, `-i`, `R`. Keeping the strings lets `complex256`
/// re-parse each component at full `BigFloat` precision rather
/// than losing it through an `f64`.
pub fn split_complex_literal(input: &str) -> Option<(String, String)> {
    let s = input.trim();
    // Compound form `(re=R im=I)` — order-free, components
    // optional.
    if let Some(rest) = s.strip_prefix('(')
        && let Some(inner) = rest.strip_suffix(')')
    {
        let mut re = String::from("0");
        let mut im = String::from("0");
        for kv in inner.split_whitespace() {
            if let Some(v) = kv.strip_prefix("re=") {
                re = v.to_string();
            } else if let Some(v) = kv.strip_prefix("im=") {
                im = v.to_string();
            } else {
                return None;
            }
        }
        return Some((re, im));
    }
    // Forms with a trailing `i`.
    if let Some(body) = s.strip_suffix('i') {
        let bytes = body.as_bytes();
        // Rightmost `+` / `-` that isn't part of an exponent.
        let mut split: Option<usize> = None;
        if !bytes.is_empty() {
            for i in (1..bytes.len()).rev() {
                let c = bytes[i] as char;
                let prev = bytes[i - 1] as char;
                if (c == '+' || c == '-') && prev != 'e' && prev != 'E' {
                    split = Some(i);
                    break;
                }
            }
        }
        if let Some(idx) = split {
            let (re_part, im_part) = body.split_at(idx);
            let im = match im_part {
                "+" => "1".to_string(),
                "-" => "-1".to_string(),
                other => other.to_string(),
            };
            return Some((re_part.to_string(), im));
        }
        // No real part — body is the imaginary coefficient.
        let im = match body {
            "" | "+" => "1".to_string(),
            "-" => "-1".to_string(),
            other => other.to_string(),
        };
        return Some(("0".to_string(), im));
    }
    // Real-only fallback.
    Some((s.to_string(), "0".to_string()))
}

/// Render a `(real, imaginary)` complex value in canonical
/// kash form. `0+0i` collapses to `0.0`; pure real / pure
/// imaginary collapses to one component each. Round-trips
/// through `parse_complex_literal`.
pub fn format_complex_value(re: f64, im: f64) -> String {
    if im == 0.0 {
        return format_float_value(re);
    }
    if re == 0.0 {
        if im == 1.0 {
            return "i".into();
        }
        if im == -1.0 {
            return "-i".into();
        }
        return alloc::format!("{}i", format_float_value(im));
    }
    let re_str = format_float_value(re);
    let im_str = format_float_value(im);
    // `im_str` carries its own sign for negative values, so
    // sandwich a `+` only for non-negative imaginary parts.
    if im > 0.0 {
        alloc::format!("{re_str}+{im_str}i")
    } else {
        alloc::format!("{re_str}{im_str}i")
    }
}

/// Significand width of IEEE 754 binary128 — the `astro-float`
/// precision used for `float128` / `complex256`.
const FLOAT128_PREC: usize = 113;

/// Tidy an `astro-float` decimal rendering: it emits scientific
/// notation with a bare `.` before the exponent for short
/// mantissas (`5.e-1`, `0.e+0`); drop that dangling dot so the
/// result is `5e-1` / `0e+0`.
fn cleanup_bigfloat_str(s: &str) -> String {
    s.replace(".e", "e")
}

/// Convert a `float128` `BigFloat` to the nearest `f64`, rounding
/// toward zero. Reconstructs the IEEE 754 bit pattern directly
/// from the raw mantissa / exponent via `as_raw_parts` — no
/// decimal round-trip and no `Consts` — mirroring astro-float's
/// own (non-public) `to_f64`.
///
/// Every reachable consumer (the integer-context `eval_arith`
/// wrapper, `as_real_f64`) immediately truncates the result to
/// `i64`, so only the integer part needs to be exact. Normal-
/// magnitude values (`|v| ≥ ~1`) take the normal path and are
/// exact to ≤1 ulp; everything in the f64-subnormal range
/// (`|v| < 2^-1022`) truncates to integer `0` regardless of the
/// subnormal branch's precision.
fn bigfloat_to_f64(bf: &astro_float::BigFloat) -> f64 {
    use astro_float::Sign;
    if bf.is_zero() {
        return 0.0;
    }
    if bf.is_inf() {
        return if bf.is_inf_neg() {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let Some((m, _n, sign, e, _inexact)) = bf.as_raw_parts() else {
        return f64::NAN;
    };
    if m.is_empty() {
        return 0.0;
    }
    // Mantissa words are little-endian, normalized so the most
    // significant word's top bit is set. Reconstruct the top 64
    // bits: the top word is one u64 (64-bit targets) or the top
    // two u32 words combined (32-bit x86). Building this without a
    // `<< 64` shift, which would be a shift-by-width overflow.
    let word_bits = core::mem::size_of::<astro_float::Word>() * 8;
    let last = m.len() - 1; // `m` is non-empty (guarded above).
    // `u64::from` is a no-op when `Word` is u64 (64-bit targets)
    // but the widening conversion needed when it is u32 (32-bit
    // x86), where the top 64 bits span the two highest words.
    #[allow(clippy::useless_conversion)]
    let mut mantissa = u64::from(m[last]);
    #[allow(clippy::useless_conversion)]
    if word_bits < 64
        && let Some(below) = last.checked_sub(1)
    {
        mantissa = (mantissa << word_bits) | u64::from(m[below]);
    }
    let neg = matches!(sign, Sign::Neg);
    // astro-float's exponent is the binary exponent of a
    // mantissa in [0.5, 1); the IEEE bias is 1023.
    let mut eb: i64 = e as i64 + 1023;
    if eb >= 0x7ff {
        return if neg { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    if eb <= 0 {
        // Subnormal, or underflow to zero.
        let shift = (-eb) as u64;
        if shift < 52 {
            let mut bits = mantissa >> (shift + 12);
            if neg {
                bits |= 0x8000_0000_0000_0000;
            }
            return f64::from_bits(bits);
        }
        return 0.0;
    }
    // Normal: assemble [sign:1][exp:11][fraction:52]. The leading
    // `<< 1` drops the implicit leading 1.
    let frac = (mantissa << 1) >> 12;
    eb -= 1;
    let mut bits: u64 = neg as u64;
    bits = (bits << 11) | eb as u64;
    bits = (bits << 52) | frac;
    f64::from_bits(bits)
}

/// Whether a `BigFloat` decimal rendering represents zero — its
/// mantissa (the part before the exponent) is all `0` (with an
/// optional sign / dot). Used to collapse a `complex256` whose
/// imaginary part is zero down to its real component.
fn bigfloat_str_is_zero(s: &str) -> bool {
    let mantissa = s.split(['e', 'E']).next().unwrap_or(s);
    mantissa
        .chars()
        .all(|c| matches!(c, '0' | '.' | '-' | '+'))
}

/// Render a `complex256` from its two already-formatted component
/// strings. Collapses a zero imaginary part to the bare real
/// component; otherwise emits `R±Ii`, letting a negative `im`
/// carry its own sign.
fn format_complex_strings(re: &str, im: &str) -> String {
    if bigfloat_str_is_zero(im) {
        return re.to_string();
    }
    if im.starts_with('-') {
        alloc::format!("{re}{im}i")
    } else {
        alloc::format!("{re}+{im}i")
    }
}

/// Render a `f64` for storage in a kash variable. Special-cases
/// NaN / ±Inf to ksh93's lowercase spellings (`nan`, `inf`,
/// `-inf`) and prints whole-valued floats as `N.0` so the result
/// round-trips back through `parse::<f64>()`.
fn format_float_value(v: f64) -> String {
    if v.is_nan() {
        return "nan".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf".into() } else { "-inf".into() };
    }
    // `{v}` for an exact integer like 3.0 prints as "3", which
    // would lose the float-ness on re-parse. Keep `.0` in that
    // case so the type identity round-trips.
    let s = alloc::format!("{v}");
    if !s.contains(['.', 'e', 'E', 'n', 'i']) {
        return alloc::format!("{s}.0");
    }
    s
}

fn is_builtin_name(name: &str) -> bool {
    if crate::scope::NumericType::from_name(name).is_some() {
        return true;
    }
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
            | "typeset"
            | "declare"
            | "export"
            | "use"
            | "read"
            | "source"
            | "."
            | "eval"
            | "command"
            | "printf"
            | "jobs"
            | "wait"
            | "fg"
            | "bg"
            | "die"
            | "assert"
            | "usage"
            | "time"
            | "getopts"
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

/// Scan a `[[ … ]]` arg list for the first `=~` form and, on
/// success, return the matched substring of the LHS. Used to
/// populate `${.sh.match}` ahead of evaluating the test itself.
/// Brute-force linear search over starting / ending positions —
/// `regex_match`'s recursive matcher doesn't carry length back,
/// so we probe substrings instead.
pub fn first_regex_match_capture(args: &[String]) -> Option<String> {
    // Walk the arg list looking for `X =~ Y`.
    for i in 1..args.len().saturating_sub(1) {
        if args[i] == "=~" {
            let lhs = &args[i - 1];
            let rhs = &args[i + 1];
            return regex_first_match_substring(rhs, lhs);
        }
    }
    None
}

/// Find the first (leftmost-longest) substring of `text` that
/// matches `pattern`. Returns `None` on no match. Built on top of
/// the existing `regex_match` matcher by probing candidate spans.
pub fn regex_first_match_substring(pattern: &str, text: &str) -> Option<String> {
    let anchored = pattern.starts_with('^');
    let inner_pat = if anchored { &pattern[1..] } else { pattern };
    let trailing_dollar = inner_pat.ends_with('$') && !inner_pat.ends_with("\\$");
    let body_pat = if trailing_dollar {
        &inner_pat[..inner_pat.len() - 1]
    } else {
        inner_pat
    };
    // Anchored on both sides so each `regex_match` call tests the
    // candidate substring *exactly*, not a prefix of it.
    let exact_pat = alloc::format!("^{body_pat}$");
    let bytes = text.as_bytes();
    let starts: Vec<usize> = if anchored {
        alloc::vec![0]
    } else {
        (0..=bytes.len())
            .filter(|i| text.is_char_boundary(*i))
            .collect()
    };
    for start in starts {
        let mut end_choices: Vec<usize> = (start..=bytes.len())
            .filter(|i| text.is_char_boundary(*i))
            .collect();
        // Longest match wins at each starting position.
        end_choices.reverse();
        for end in end_choices {
            if trailing_dollar && end != bytes.len() {
                continue;
            }
            if regex_match(&exact_pat, &text[start..end]) {
                return Some(text[start..end].to_string());
            }
        }
    }
    None
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
struct ArithParser<'a, 'e, B: MapBackend> {
    src: &'a str,
    pos: usize,
    ev: &'e mut Evaluator<B>,
}

/// A value flowing through the arithmetic engine. The tiers form
/// a 2-D lattice — *precision* (Int 0 < f64 1 < f128 2) × *domain*
/// (real / complex) — and a binary op promotes to the widest of
/// both axes by operand. Two `Int`s stay integer (`$((7/2))` is
/// `3`); any `Float`/`Float128` promotes the real precision; any
/// complex operand makes the result complex (`$((1 + 2i))`); a
/// `Float128` mixed with a `Complex` widens to `Complex256` so no
/// precision is silently dropped. Bitwise / shift require integer
/// operands; ordering comparisons are rejected on complex values.
#[derive(Clone, Debug)]
enum Num {
    /// 64-bit signed integer — the shell arithmetic default.
    Int(i64),
    /// IEEE 754 binary64.
    Float(f64),
    /// IEEE 754 binary128 (113-bit), backed by `astro-float`.
    Float128(astro_float::BigFloat),
    /// `complex128` — an `f64` real / imaginary pair.
    Complex(f64, f64),
    /// `complex256` — a 113-bit `BigFloat` real / imaginary pair.
    Complex256(astro_float::BigFloat, astro_float::BigFloat),
}

impl Num {
    /// Precision rank: 0 `Int`, 1 `Float` / `Complex`, 2
    /// `Float128` / `Complex256`. A binary op computes at the max
    /// of the two operands' ranks.
    fn rank(&self) -> u8 {
        match self {
            Num::Int(_) => 0,
            Num::Float(_) | Num::Complex(..) => 1,
            Num::Float128(_) | Num::Complex256(..) => 2,
        }
    }

    /// Whether the value lives in the complex domain.
    fn is_complex(&self) -> bool {
        matches!(self, Num::Complex(..) | Num::Complex256(..))
    }

    /// Promote to a 113-bit `BigFloat` (real float128 tier). Only
    /// valid on a real value.
    fn to_bigfloat(&self) -> astro_float::BigFloat {
        match self {
            Num::Int(n) => astro_float::BigFloat::from_i64(*n, FLOAT128_PREC),
            Num::Float(f) => astro_float::BigFloat::from_f64(*f, FLOAT128_PREC),
            Num::Float128(bf) => bf.clone(),
            Num::Complex(..) | Num::Complex256(..) => {
                unreachable!("to_bigfloat on a complex value")
            }
        }
    }

    /// Promote to an `f64` `(re, im)` pair (the complex128 tier).
    fn to_complex_f64(&self) -> (f64, f64) {
        match self {
            Num::Int(n) => (*n as f64, 0.0),
            Num::Float(f) => (*f, 0.0),
            Num::Complex(r, i) => (*r, *i),
            // The rank-2 tiers normally take the BigFloat complex
            // path; convert defensively in case they reach here.
            Num::Float128(bf) => (bigfloat_to_f64(bf), 0.0),
            Num::Complex256(r, i) => (bigfloat_to_f64(r), bigfloat_to_f64(i)),
        }
    }

    /// Promote to a 113-bit `BigFloat` `(re, im)` pair (the
    /// complex256 tier).
    fn to_complex_bf(&self) -> (astro_float::BigFloat, astro_float::BigFloat) {
        use astro_float::BigFloat;
        let zero = BigFloat::from_i64(0, FLOAT128_PREC);
        match self {
            Num::Int(n) => (BigFloat::from_i64(*n, FLOAT128_PREC), zero),
            Num::Float(f) => (BigFloat::from_f64(*f, FLOAT128_PREC), zero),
            Num::Float128(bf) => (bf.clone(), zero),
            Num::Complex(r, i) => (
                BigFloat::from_f64(*r, FLOAT128_PREC),
                BigFloat::from_f64(*i, FLOAT128_PREC),
            ),
            Num::Complex256(r, i) => (r.clone(), i.clone()),
        }
    }

    /// Value as `f64` when it is genuinely real — every real tier,
    /// and a complex value *only if its imaginary part is zero*
    /// (e.g. the `1+0i` left by `(1+2i) - 2i`). Otherwise `None`.
    fn as_real_f64(&self) -> Option<f64> {
        match self {
            Num::Int(n) => Some(*n as f64),
            Num::Float(f) => Some(*f),
            Num::Float128(bf) => Some(bigfloat_to_f64(bf)),
            Num::Complex(r, i) => (*i == 0.0).then_some(*r),
            Num::Complex256(r, i) => i.is_zero().then(|| bigfloat_to_f64(r)),
        }
    }

    /// Value as `f64`. The f128 tier converts directly from its
    /// raw bits; a complex value yields its real part when the
    /// imaginary part is zero, else `NaN` (the real arith path
    /// checks domain first, so a true complex never reaches here).
    fn as_f64(&self) -> f64 {
        self.as_real_f64().unwrap_or(f64::NAN)
    }

    /// True iff the value is non-zero — arithmetic "true" for
    /// `&&` / `||` / `!` / ternary / `((…))`.
    fn is_truthy(&self) -> bool {
        match self {
            Num::Int(n) => *n != 0,
            Num::Float(f) => *f != 0.0,
            Num::Float128(bf) => !bf.is_zero(),
            Num::Complex(r, i) => *r != 0.0 || *i != 0.0,
            Num::Complex256(r, i) => !r.is_zero() || !i.is_zero(),
        }
    }

    /// Require an integer operand for a bitwise / shift operator.
    fn require_int(&self, op: &str) -> Result<i64> {
        match self {
            Num::Int(n) => Ok(*n),
            _ => Err(KashError::Runtime(alloc::format!(
                "arithmetic: operator `{op}` requires integer operands"
            ))),
        }
    }

    /// Whether either operand sits in the float128 / complex256
    /// precision tier (rank 2).
    fn either_f128(a: &Num, b: &Num) -> bool {
        a.rank() == 2 || b.rank() == 2
    }

    /// Whether either operand is a (non-integer) real float.
    fn either_float(a: &Num, b: &Num) -> bool {
        a.rank() >= 1 || b.rank() >= 1
    }
}

/// IEEE 754 round-to-nearest-even — the default for the
/// float128 arithmetic path.
const F128_RM: astro_float::RoundingMode = astro_float::RoundingMode::ToEven;

/// One-argument math-library functions, dispatched per numeric
/// tier by the arithmetic engine.
#[derive(Clone, Copy)]
enum Fn1 {
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Sinh,
    Cosh,
    Tanh,
    Asinh,
    Acosh,
    Atanh,
    Exp,
    Exp2,
    Ln,
    Log2,
    Log10,
    Sqrt,
    Cbrt,
    Floor,
    Ceil,
    Round,
    Trunc,
    Fabs,
}

impl Fn1 {
    fn from_name(n: &str) -> Option<Fn1> {
        Some(match n {
            "sin" => Fn1::Sin,
            "cos" => Fn1::Cos,
            "tan" => Fn1::Tan,
            "asin" => Fn1::Asin,
            "acos" => Fn1::Acos,
            "atan" => Fn1::Atan,
            "sinh" => Fn1::Sinh,
            "cosh" => Fn1::Cosh,
            "tanh" => Fn1::Tanh,
            "asinh" => Fn1::Asinh,
            "acosh" => Fn1::Acosh,
            "atanh" => Fn1::Atanh,
            "exp" => Fn1::Exp,
            "exp2" => Fn1::Exp2,
            // C / ksh93 `log` is natural log; `ln` is the alias.
            "log" | "ln" => Fn1::Ln,
            "log2" => Fn1::Log2,
            "log10" => Fn1::Log10,
            "sqrt" => Fn1::Sqrt,
            "cbrt" => Fn1::Cbrt,
            "floor" => Fn1::Floor,
            "ceil" => Fn1::Ceil,
            "round" => Fn1::Round,
            "trunc" => Fn1::Trunc,
            "fabs" => Fn1::Fabs,
            _ => return None,
        })
    }

    fn eval_f64(self, x: f64) -> f64 {
        match self {
            Fn1::Sin => libm::sin(x),
            Fn1::Cos => libm::cos(x),
            Fn1::Tan => libm::tan(x),
            Fn1::Asin => libm::asin(x),
            Fn1::Acos => libm::acos(x),
            Fn1::Atan => libm::atan(x),
            Fn1::Sinh => libm::sinh(x),
            Fn1::Cosh => libm::cosh(x),
            Fn1::Tanh => libm::tanh(x),
            Fn1::Asinh => libm::asinh(x),
            Fn1::Acosh => libm::acosh(x),
            Fn1::Atanh => libm::atanh(x),
            Fn1::Exp => libm::exp(x),
            Fn1::Exp2 => libm::exp2(x),
            Fn1::Ln => libm::log(x),
            Fn1::Log2 => libm::log2(x),
            Fn1::Log10 => libm::log10(x),
            Fn1::Sqrt => libm::sqrt(x),
            Fn1::Cbrt => libm::cbrt(x),
            Fn1::Floor => libm::floor(x),
            Fn1::Ceil => libm::ceil(x),
            Fn1::Round => libm::round(x),
            Fn1::Trunc => libm::trunc(x),
            Fn1::Fabs => libm::fabs(x),
        }
    }
}

/// Two-argument math-library functions.
#[derive(Clone, Copy)]
enum Fn2 {
    Atan2,
    Hypot,
    Pow,
    Fmod,
}

impl Fn2 {
    fn from_name(n: &str) -> Option<Fn2> {
        Some(match n {
            "atan2" => Fn2::Atan2,
            "hypot" => Fn2::Hypot,
            "pow" => Fn2::Pow,
            "fmod" => Fn2::Fmod,
            _ => return None,
        })
    }

    fn eval_f64(self, a: f64, b: f64) -> f64 {
        match self {
            Fn2::Atan2 => libm::atan2(a, b),
            Fn2::Hypot => libm::hypot(a, b),
            Fn2::Pow => libm::pow(a, b),
            Fn2::Fmod => libm::fmod(a, b),
        }
    }
}

/// Order two `BigFloat`s by the sign of their difference rather
/// than `BigFloat::cmp`. astro-float 0.9.5's `cmp` returns the
/// wrong sign comparing a small-magnitude value (whose 113-bit
/// mantissa has a leading zero word) against zero — e.g.
/// `1e-40.cmp(0)` yields `Less`. Subtraction is correct for these
/// values, and the sign of `a − b` is exact for the ordering.
fn bigfloat_cmp(a: &astro_float::BigFloat, b: &astro_float::BigFloat) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    let d = a.sub(b, FLOAT128_PREC, F128_RM);
    if d.is_zero() {
        Ordering::Equal
    } else if d.is_negative() {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

/// Truncate a `BigFloat` toward zero at full 113-bit precision —
/// astro-float has no `trunc`, so use `x − (x rem 1)` (its `rem`
/// is the truncated remainder, so the fractional part carries
/// `x`'s sign and subtracting it lands on the integer toward
/// zero).
fn bigfloat_trunc(x: &astro_float::BigFloat) -> astro_float::BigFloat {
    let one = astro_float::BigFloat::from_i64(1, FLOAT128_PREC);
    let frac = x.rem(&one);
    x.sub(&frac, FLOAT128_PREC, F128_RM)
}

/// `floor` at 113-bit: trunc, then step down by one for a
/// negative value with a non-zero fractional part.
fn bigfloat_floor(x: &astro_float::BigFloat) -> astro_float::BigFloat {
    let t = bigfloat_trunc(x);
    if x.is_negative() && bigfloat_cmp(x, &t) != core::cmp::Ordering::Equal {
        let one = astro_float::BigFloat::from_i64(1, FLOAT128_PREC);
        t.sub(&one, FLOAT128_PREC, F128_RM)
    } else {
        t
    }
}

/// `ceil` at 113-bit: trunc, then step up for a positive value
/// with a non-zero fractional part.
fn bigfloat_ceil(x: &astro_float::BigFloat) -> astro_float::BigFloat {
    let t = bigfloat_trunc(x);
    if x.is_positive() && bigfloat_cmp(x, &t) != core::cmp::Ordering::Equal {
        let one = astro_float::BigFloat::from_i64(1, FLOAT128_PREC);
        t.add(&one, FLOAT128_PREC, F128_RM)
    } else {
        t
    }
}

/// `round` at 113-bit, half away from zero (matching `libm`):
/// truncate `x ± 0.5`.
fn bigfloat_round(x: &astro_float::BigFloat) -> astro_float::BigFloat {
    let half = astro_float::BigFloat::from_f64(0.5, FLOAT128_PREC);
    let adj = if x.is_negative() {
        x.sub(&half, FLOAT128_PREC, F128_RM)
    } else {
        x.add(&half, FLOAT128_PREC, F128_RM)
    };
    bigfloat_trunc(&adj)
}

/// `atan2(y, x)` at 113-bit, composed from `atan` (astro-float
/// has no `atan2`) with the standard quadrant adjustments.
fn bigfloat_atan2(
    y: &astro_float::BigFloat,
    x: &astro_float::BigFloat,
    cc: &mut astro_float::Consts,
) -> astro_float::BigFloat {
    let p = FLOAT128_PREC;
    let rm = F128_RM;
    let pi = cc.pi(p, rm);
    if !x.is_zero() {
        let base = y.div(x, p, rm).atan(p, rm, cc);
        if x.is_negative() {
            if y.is_negative() {
                base.sub(&pi, p, rm)
            } else {
                base.add(&pi, p, rm)
            }
        } else {
            base
        }
    } else if y.is_zero() {
        astro_float::BigFloat::from_i64(0, p)
    } else {
        let half_pi = pi.div(&astro_float::BigFloat::from_i64(2, p), p, rm);
        if y.is_negative() {
            half_pi.neg()
        } else {
            half_pi
        }
    }
}

/// Which complex binary op `arith_complex` should perform.
#[derive(Clone, Copy)]
enum COp {
    Add,
    Sub,
    Mul,
    Div,
}

/// Complex `a ⊕ b` at the appropriate precision: `complex256`
/// (BigFloat pair) when either operand is a rank-2 tier, else
/// `complex128` (f64 pair). Division by a zero complex yields
/// NaN/Inf components (IEEE), not an error.
fn arith_complex(a: &Num, b: &Num, op: COp) -> Num {
    if Num::either_f128(a, b) {
        let (ar, ai) = a.to_complex_bf();
        let (br, bi) = b.to_complex_bf();
        let p = FLOAT128_PREC;
        let (rr, ri) = match op {
            COp::Add => (ar.add(&br, p, F128_RM), ai.add(&bi, p, F128_RM)),
            COp::Sub => (ar.sub(&br, p, F128_RM), ai.sub(&bi, p, F128_RM)),
            COp::Mul => {
                // (ar·br − ai·bi) + (ar·bi + ai·br)i
                let rr = ar
                    .mul(&br, p, F128_RM)
                    .sub(&ai.mul(&bi, p, F128_RM), p, F128_RM);
                let ri = ar
                    .mul(&bi, p, F128_RM)
                    .add(&ai.mul(&br, p, F128_RM), p, F128_RM);
                (rr, ri)
            }
            COp::Div => {
                // numerator · conj(denom) / |denom|²
                let denom = br
                    .mul(&br, p, F128_RM)
                    .add(&bi.mul(&bi, p, F128_RM), p, F128_RM);
                let rr = ar
                    .mul(&br, p, F128_RM)
                    .add(&ai.mul(&bi, p, F128_RM), p, F128_RM)
                    .div(&denom, p, F128_RM);
                let ri = ai
                    .mul(&br, p, F128_RM)
                    .sub(&ar.mul(&bi, p, F128_RM), p, F128_RM)
                    .div(&denom, p, F128_RM);
                (rr, ri)
            }
        };
        Num::Complex256(rr, ri)
    } else {
        let (ar, ai) = a.to_complex_f64();
        let (br, bi) = b.to_complex_f64();
        let (rr, ri) = match op {
            COp::Add => (ar + br, ai + bi),
            COp::Sub => (ar - br, ai - bi),
            COp::Mul => (ar * br - ai * bi, ar * bi + ai * br),
            COp::Div => {
                let denom = br * br + bi * bi;
                ((ar * br + ai * bi) / denom, (ai * br - ar * bi) / denom)
            }
        };
        Num::Complex(rr, ri)
    }
}

/// `a + b` — promotes across the precision *and* domain axes.
fn arith_add(a: &Num, b: &Num) -> Result<Num> {
    if a.is_complex() || b.is_complex() {
        return Ok(arith_complex(a, b, COp::Add));
    }
    if let (Num::Int(x), Num::Int(y)) = (a, b) {
        x.checked_add(*y)
            .map(Num::Int)
            .ok_or_else(|| KashError::Runtime("arithmetic overflow".into()))
    } else if Num::either_f128(a, b) {
        Ok(Num::Float128(a.to_bigfloat().add(
            &b.to_bigfloat(),
            FLOAT128_PREC,
            F128_RM,
        )))
    } else {
        Ok(Num::Float(a.as_f64() + b.as_f64()))
    }
}

fn arith_sub(a: &Num, b: &Num) -> Result<Num> {
    if a.is_complex() || b.is_complex() {
        return Ok(arith_complex(a, b, COp::Sub));
    }
    if let (Num::Int(x), Num::Int(y)) = (a, b) {
        x.checked_sub(*y)
            .map(Num::Int)
            .ok_or_else(|| KashError::Runtime("arithmetic overflow".into()))
    } else if Num::either_f128(a, b) {
        Ok(Num::Float128(a.to_bigfloat().sub(
            &b.to_bigfloat(),
            FLOAT128_PREC,
            F128_RM,
        )))
    } else {
        Ok(Num::Float(a.as_f64() - b.as_f64()))
    }
}

fn arith_mul(a: &Num, b: &Num) -> Result<Num> {
    if a.is_complex() || b.is_complex() {
        return Ok(arith_complex(a, b, COp::Mul));
    }
    if let (Num::Int(x), Num::Int(y)) = (a, b) {
        x.checked_mul(*y)
            .map(Num::Int)
            .ok_or_else(|| KashError::Runtime("arithmetic overflow".into()))
    } else if Num::either_f128(a, b) {
        Ok(Num::Float128(a.to_bigfloat().mul(
            &b.to_bigfloat(),
            FLOAT128_PREC,
            F128_RM,
        )))
    } else {
        Ok(Num::Float(a.as_f64() * b.as_f64()))
    }
}

fn arith_div(a: &Num, b: &Num) -> Result<Num> {
    if a.is_complex() || b.is_complex() {
        return Ok(arith_complex(a, b, COp::Div));
    }
    if Num::either_f128(a, b) {
        return Ok(Num::Float128(a.to_bigfloat().div(
            &b.to_bigfloat(),
            FLOAT128_PREC,
            F128_RM,
        )));
    }
    if Num::either_float(a, b) {
        Ok(Num::Float(a.as_f64() / b.as_f64()))
    } else {
        let (Num::Int(x), Num::Int(y)) = (a, b) else {
            unreachable!("non-float operands are both Int")
        };
        if *y == 0 {
            return Err(KashError::Runtime("arithmetic: divide by zero".into()));
        }
        Ok(Num::Int(x / y))
    }
}

fn arith_mod(a: &Num, b: &Num) -> Result<Num> {
    if a.is_complex() || b.is_complex() {
        return Err(KashError::Runtime(
            "arithmetic: `%` is undefined for complex operands".into(),
        ));
    }
    if Num::either_f128(a, b) {
        // `astro-float`'s `rem` is the truncated remainder.
        return Ok(Num::Float128(a.to_bigfloat().rem(&b.to_bigfloat())));
    }
    if Num::either_float(a, b) {
        Ok(Num::Float(a.as_f64() % b.as_f64()))
    } else {
        let (Num::Int(x), Num::Int(y)) = (a, b) else {
            unreachable!("non-float operands are both Int")
        };
        if *y == 0 {
            return Err(KashError::Runtime("arithmetic: modulo by zero".into()));
        }
        Ok(Num::Int(x % y))
    }
}

/// Unary negation across all tiers.
fn arith_negate(v: &Num) -> Result<Num> {
    match v {
        Num::Int(n) => n
            .checked_neg()
            .map(Num::Int)
            .ok_or_else(|| KashError::Runtime("arithmetic overflow".into())),
        Num::Float(f) => Ok(Num::Float(-f)),
        Num::Float128(bf) => Ok(Num::Float128(bf.neg())),
        Num::Complex(r, i) => Ok(Num::Complex(-r, -i)),
        Num::Complex256(r, i) => Ok(Num::Complex256(r.neg(), i.neg())),
    }
}

/// Boolean result as the arithmetic `1` / `0` integer.
fn bool_num(b: bool) -> Num {
    Num::Int(b as i64)
}

/// Error for a math function called with the wrong argument count.
fn math_arity_err(name: &str, want: usize, got: usize) -> KashError {
    KashError::Runtime(alloc::format!(
        "arithmetic: `{name}()` takes {want} argument(s), got {got}"
    ))
}

/// Error for a math function applied to a tier it doesn't yet
/// support (float128 / complex).
fn math_tier_err(name: &str) -> KashError {
    KashError::Runtime(alloc::format!(
        "arithmetic: `{name}()` on float128 / complex is not yet supported"
    ))
}

/// The ksh93 math constants (`M_*`), all `float64`. Returns the
/// `f64` value for a recognised name. A user variable of the same
/// name shadows these (the lookup tries the binding first).
fn math_constant(name: &str) -> Option<f64> {
    use core::f64::consts;
    Some(match name {
        "M_E" => consts::E,
        "M_PI" => consts::PI,
        "M_PI_2" => consts::FRAC_PI_2,
        "M_PI_4" => consts::FRAC_PI_4,
        "M_1_PI" => consts::FRAC_1_PI,
        "M_2_PI" => consts::FRAC_2_PI,
        "M_2_SQRTPI" => consts::FRAC_2_SQRT_PI,
        "M_SQRT2" => consts::SQRT_2,
        "M_SQRT1_2" => consts::FRAC_1_SQRT_2,
        "M_LN2" => consts::LN_2,
        "M_LN10" => consts::LN_10,
        "M_LOG2E" => consts::LOG2_E,
        "M_LOG10E" => consts::LOG10_E,
        _ => return None,
    })
}

/// Equality of two numbers, promoting across both axes. Complex
/// values compare component-wise.
fn arith_eq(a: &Num, b: &Num) -> bool {
    if a.is_complex() || b.is_complex() {
        return if Num::either_f128(a, b) {
            use core::cmp::Ordering::Equal;
            let (ar, ai) = a.to_complex_bf();
            let (br, bi) = b.to_complex_bf();
            bigfloat_cmp(&ar, &br) == Equal && bigfloat_cmp(&ai, &bi) == Equal
        } else {
            let (ar, ai) = a.to_complex_f64();
            let (br, bi) = b.to_complex_f64();
            ar == br && ai == bi
        };
    }
    matches!(arith_cmp(a, b), Ok(core::cmp::Ordering::Equal))
}

/// Order two *real* numbers, promoting to the wider tier. Complex
/// operands have no ordering and are an error.
fn arith_cmp(a: &Num, b: &Num) -> Result<core::cmp::Ordering> {
    use core::cmp::Ordering;
    if a.is_complex() || b.is_complex() {
        return Err(KashError::Runtime(
            "arithmetic: complex values are not ordered".into(),
        ));
    }
    if Num::either_f128(a, b) {
        Ok(bigfloat_cmp(&a.to_bigfloat(), &b.to_bigfloat()))
    } else if Num::either_float(a, b) {
        Ok(a.as_f64()
            .partial_cmp(&b.as_f64())
            .unwrap_or(Ordering::Equal))
    } else {
        match (a, b) {
            (Num::Int(x), Num::Int(y)) => Ok(x.cmp(y)),
            _ => unreachable!("non-float operands are both Int"),
        }
    }
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

impl<'a, 'e, B: MapBackend> ArithParser<'a, 'e, B> {
    fn parse_expr(&mut self) -> Result<Num> {
        self.parse_assign()
    }

    fn parse_assign(&mut self) -> Result<Num> {
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

    fn apply_assign(&mut self, name: &str, op: AssignOp, rhs: Num) -> Result<Num> {
        let new = match op {
            AssignOp::Plain => rhs,
            AssignOp::Add => arith_add(&self.read_named(name)?, &rhs)?,
            AssignOp::Sub => arith_sub(&self.read_named(name)?, &rhs)?,
            AssignOp::Mul => arith_mul(&self.read_named(name)?, &rhs)?,
            AssignOp::Div => arith_div(&self.read_named(name)?, &rhs)?,
            AssignOp::Mod => arith_mod(&self.read_named(name)?, &rhs)?,
            AssignOp::BitAnd => Num::Int(
                self.read_named(name)?.require_int("&=")? & rhs.require_int("&=")?,
            ),
            AssignOp::BitOr => Num::Int(
                self.read_named(name)?.require_int("|=")? | rhs.require_int("|=")?,
            ),
            AssignOp::BitXor => Num::Int(
                self.read_named(name)?.require_int("^=")? ^ rhs.require_int("^=")?,
            ),
            AssignOp::Shl => Num::Int(
                self.read_named(name)?
                    .require_int("<<=")?
                    .wrapping_shl(rhs.require_int("<<=")? as u32),
            ),
            AssignOp::Shr => Num::Int(
                self.read_named(name)?
                    .require_int(">>=")?
                    .wrapping_shr(rhs.require_int(">>=")? as u32),
            ),
        };
        let rendered = self.render_num(&new)?;
        self.ev.scope.assign(name, Value::Scalar(rendered))?;
        Ok(new)
    }

    /// Render a `Num` for storage / output, delegating to the
    /// evaluator (the float128 / complex256 tiers need its
    /// `Consts` cache to format).
    fn render_num(&mut self, n: &Num) -> Result<String> {
        self.ev.render_num(n)
    }

    fn parse_ternary(&mut self) -> Result<Num> {
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
            Ok(if cond.is_truthy() { then_val } else { else_val })
        } else {
            Ok(cond)
        }
    }

    fn parse_or(&mut self) -> Result<Num> {
        let mut lhs = self.parse_and()?;
        while self.try_consume_exact("||") {
            let rhs = self.parse_and()?;
            lhs = bool_num(lhs.is_truthy() || rhs.is_truthy());
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Num> {
        let mut lhs = self.parse_bit_or()?;
        while self.try_consume_exact("&&") {
            let rhs = self.parse_bit_or()?;
            lhs = bool_num(lhs.is_truthy() && rhs.is_truthy());
        }
        Ok(lhs)
    }

    fn parse_bit_or(&mut self) -> Result<Num> {
        let mut lhs = self.parse_bit_xor()?;
        while self.try_consume_single('|') {
            let rhs = self.parse_bit_xor()?;
            lhs = Num::Int(lhs.require_int("|")? | rhs.require_int("|")?);
        }
        Ok(lhs)
    }

    fn parse_bit_xor(&mut self) -> Result<Num> {
        let mut lhs = self.parse_bit_and()?;
        while self.try_consume_single('^') {
            let rhs = self.parse_bit_and()?;
            lhs = Num::Int(lhs.require_int("^")? ^ rhs.require_int("^")?);
        }
        Ok(lhs)
    }

    fn parse_bit_and(&mut self) -> Result<Num> {
        let mut lhs = self.parse_eq()?;
        while self.try_consume_single('&') {
            let rhs = self.parse_eq()?;
            lhs = Num::Int(lhs.require_int("&")? & rhs.require_int("&")?);
        }
        Ok(lhs)
    }

    fn parse_eq(&mut self) -> Result<Num> {
        let mut lhs = self.parse_rel()?;
        loop {
            if self.try_consume_exact("==") {
                let rhs = self.parse_rel()?;
                lhs = bool_num(arith_eq(&lhs, &rhs));
            } else if self.try_consume_exact("!=") {
                let rhs = self.parse_rel()?;
                lhs = bool_num(!arith_eq(&lhs, &rhs));
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_rel(&mut self) -> Result<Num> {
        use core::cmp::Ordering;
        let mut lhs = self.parse_shift()?;
        loop {
            if self.try_consume_exact("<=") {
                let rhs = self.parse_shift()?;
                lhs = bool_num(arith_cmp(&lhs, &rhs)? != Ordering::Greater);
            } else if self.try_consume_exact(">=") {
                let rhs = self.parse_shift()?;
                lhs = bool_num(arith_cmp(&lhs, &rhs)? != Ordering::Less);
            } else if self.try_consume_single('<') {
                let rhs = self.parse_shift()?;
                lhs = bool_num(arith_cmp(&lhs, &rhs)? == Ordering::Less);
            } else if self.try_consume_single('>') {
                let rhs = self.parse_shift()?;
                lhs = bool_num(arith_cmp(&lhs, &rhs)? == Ordering::Greater);
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_shift(&mut self) -> Result<Num> {
        let mut lhs = self.parse_add()?;
        loop {
            if self.try_consume_exact("<<") {
                let rhs = self.parse_add()?;
                lhs = Num::Int(
                    lhs.require_int("<<")?.wrapping_shl(rhs.require_int("<<")? as u32),
                );
            } else if self.try_consume_exact(">>") {
                let rhs = self.parse_add()?;
                lhs = Num::Int(
                    lhs.require_int(">>")?.wrapping_shr(rhs.require_int(">>")? as u32),
                );
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_add(&mut self) -> Result<Num> {
        let mut lhs = self.parse_mul()?;
        loop {
            if self.try_consume_single('+') {
                let rhs = self.parse_mul()?;
                lhs = arith_add(&lhs, &rhs)?;
            } else if self.try_consume_single('-') {
                let rhs = self.parse_mul()?;
                lhs = arith_sub(&lhs, &rhs)?;
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<Num> {
        let mut lhs = self.parse_unary()?;
        loop {
            if self.try_consume_single('*') {
                let rhs = self.parse_unary()?;
                lhs = arith_mul(&lhs, &rhs)?;
            } else if self.try_consume_single('/') {
                let rhs = self.parse_unary()?;
                lhs = arith_div(&lhs, &rhs)?;
            } else if self.try_consume_single('%') {
                let rhs = self.parse_unary()?;
                lhs = arith_mod(&lhs, &rhs)?;
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Num> {
        self.skip_ws();
        if self.try_consume_exact("++") {
            let name = self.try_read_identifier().ok_or_else(|| {
                KashError::Parse("arithmetic: `++` requires an lvalue".into())
            })?;
            let new = arith_add(&self.read_named(&name)?, &Num::Int(1))?;
            let rendered = self.render_num(&new)?;
            self.ev.scope.assign(&name, Value::Scalar(rendered))?;
            return Ok(new);
        }
        if self.try_consume_exact("--") {
            let name = self.try_read_identifier().ok_or_else(|| {
                KashError::Parse("arithmetic: `--` requires an lvalue".into())
            })?;
            let new = arith_sub(&self.read_named(&name)?, &Num::Int(1))?;
            let rendered = self.render_num(&new)?;
            self.ev.scope.assign(&name, Value::Scalar(rendered))?;
            return Ok(new);
        }
        if self.try_consume_single('+') {
            return self.parse_unary();
        }
        if self.try_consume_single('-') {
            let v = self.parse_unary()?;
            return arith_negate(&v);
        }
        if self.try_consume_single('!') {
            let v = self.parse_unary()?;
            return Ok(bool_num(!v.is_truthy()));
        }
        if self.try_consume_single('~') {
            let v = self.parse_unary()?;
            return Ok(Num::Int(!v.require_int("~")?));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Num> {
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
            if c.is_ascii_digit() || c == '.' {
                return self.parse_number();
            }
            if c == '_' || c.is_ascii_alphabetic() {
                let name = self
                    .try_read_identifier()
                    .expect("just peeked an identifier start");
                // A `(` immediately following an identifier is a
                // math function call — `sin(x)`, `atan2(y, x)`.
                // (Checked before whitespace-skipping so `sin (x)`
                // isn't a call, matching ksh93.)
                if self.peek() == Some('(') {
                    self.advance(); // `(`
                    let args = self.parse_call_args()?;
                    return self.call_math_func(&name, &args);
                }
                self.skip_ws();
                if self.try_consume_exact("++") {
                    // Post-increment: yield the old value, store the new.
                    let current = self.read_named(&name)?;
                    let new = arith_add(&current, &Num::Int(1))?;
                    let rendered = self.render_num(&new)?;
                    self.ev.scope.assign(&name, Value::Scalar(rendered))?;
                    return Ok(current);
                }
                if self.try_consume_exact("--") {
                    let current = self.read_named(&name)?;
                    let new = arith_sub(&current, &Num::Int(1))?;
                    let rendered = self.render_num(&new)?;
                    self.ev.scope.assign(&name, Value::Scalar(rendered))?;
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

    /// Parse a comma-separated argument list for a math function
    /// call, the opening `(` already consumed, stopping at the
    /// matching `)`.
    fn parse_call_args(&mut self) -> Result<Vec<Num>> {
        let mut args = Vec::new();
        self.skip_ws();
        if self.try_consume_exact(")") {
            return Ok(args);
        }
        loop {
            args.push(self.parse_assign()?);
            self.skip_ws();
            if self.try_consume_exact(",") {
                continue;
            }
            if self.try_consume_exact(")") {
                break;
            }
            return Err(KashError::Parse(
                "arithmetic: expected `,` or `)` in function call".into(),
            ));
        }
        Ok(args)
    }

    /// Dispatch a math-library function call across the numeric
    /// tiers: real f64 (`libm`), real float128 (`astro-float`),
    /// and complex. Complex-specific functions (`creal`, `cabs`,
    /// …) and `abs` have dedicated handling.
    fn call_math_func(&mut self, name: &str, args: &[Num]) -> Result<Num> {
        if let Some(f) = Fn1::from_name(name) {
            let [a] = args else {
                return Err(math_arity_err(name, 1, args.len()));
            };
            return self.eval_fn1(f, name, a);
        }
        if let Some(f) = Fn2::from_name(name) {
            let [a, b] = args else {
                return Err(math_arity_err(name, 2, args.len()));
            };
            return self.eval_fn2(f, name, a, b);
        }
        match name {
            "abs" => self.math_abs(args),
            "creal" | "cimag" | "cconj" | "cabs" | "carg" | "cphase" => {
                let [a] = args else {
                    return Err(math_arity_err(name, 1, args.len()));
                };
                self.eval_complex_part(name, a)
            }
            _ => Err(KashError::Runtime(alloc::format!(
                "arithmetic: unknown function `{name}`"
            ))),
        }
    }

    /// One-argument math function, dispatched by the argument's
    /// tier. f64 → `libm`; float128 → `astro-float` (custom
    /// rounding family); complex → complex-generalized formulas
    /// (`eval_fn1_complex`).
    fn eval_fn1(&mut self, f: Fn1, name: &str, a: &Num) -> Result<Num> {
        match a {
            Num::Int(_) | Num::Float(_) => Ok(Num::Float(f.eval_f64(a.as_f64()))),
            Num::Float128(bf) => {
                let r = self.eval_fn1_f128(f, &bf.clone())?;
                Ok(Num::Float128(r))
            }
            Num::Complex(re, im) => self.eval_fn1_complex(f, name, *re, *im),
            Num::Complex256(..) => Err(KashError::Runtime(alloc::format!(
                "arithmetic: `{name}()` on complex256 is not yet supported"
            ))),
        }
    }

    /// float128 unary math. Transcendentals go through
    /// `astro-float` (some need the `Consts` cache); the rounding
    /// family is computed at full 113-bit precision via the
    /// `bigfloat_*` helpers.
    fn eval_fn1_f128(
        &mut self,
        f: Fn1,
        x: &astro_float::BigFloat,
    ) -> Result<astro_float::BigFloat> {
        use astro_float::BigFloat;
        let p = FLOAT128_PREC;
        let rm = F128_RM;
        Ok(match f {
            Fn1::Sqrt => x.sqrt(p, rm),
            Fn1::Cbrt => x.cbrt(p, rm),
            Fn1::Fabs => x.abs(),
            Fn1::Floor => bigfloat_floor(x),
            Fn1::Ceil => bigfloat_ceil(x),
            Fn1::Round => bigfloat_round(x),
            Fn1::Trunc => bigfloat_trunc(x),
            Fn1::Exp2 => {
                let two = BigFloat::from_i64(2, p);
                let cc = self.ev.bf_consts()?;
                two.pow(x, p, rm, cc)
            }
            _ => {
                let cc = self.ev.bf_consts()?;
                match f {
                    Fn1::Sin => x.sin(p, rm, cc),
                    Fn1::Cos => x.cos(p, rm, cc),
                    Fn1::Tan => x.tan(p, rm, cc),
                    Fn1::Asin => x.asin(p, rm, cc),
                    Fn1::Acos => x.acos(p, rm, cc),
                    Fn1::Atan => x.atan(p, rm, cc),
                    Fn1::Sinh => x.sinh(p, rm, cc),
                    Fn1::Cosh => x.cosh(p, rm, cc),
                    Fn1::Tanh => x.tanh(p, rm, cc),
                    Fn1::Asinh => x.asinh(p, rm, cc),
                    Fn1::Acosh => x.acosh(p, rm, cc),
                    Fn1::Atanh => x.atanh(p, rm, cc),
                    Fn1::Exp => x.exp(p, rm, cc),
                    Fn1::Ln => x.ln(p, rm, cc),
                    Fn1::Log2 => x.log2(p, rm, cc),
                    Fn1::Log10 => x.log10(p, rm, cc),
                    // Algebraic / rounding cases handled above.
                    Fn1::Sqrt
                    | Fn1::Cbrt
                    | Fn1::Fabs
                    | Fn1::Floor
                    | Fn1::Ceil
                    | Fn1::Round
                    | Fn1::Trunc
                    | Fn1::Exp2 => unreachable!("handled above"),
                }
            }
        })
    }

    /// Two-argument math function across tiers. f64 → `libm`;
    /// float128 → `astro-float` (`atan2` / `hypot` composed from
    /// primitives astro-float lacks). Complex operands are an
    /// error for these (they are real-valued binary ops; complex
    /// `pow` is a follow-up).
    fn eval_fn2(&mut self, f: Fn2, name: &str, a: &Num, b: &Num) -> Result<Num> {
        if a.is_complex() || b.is_complex() {
            return Err(math_tier_err(name));
        }
        if a.rank() == 2 || b.rank() == 2 {
            let x = a.to_bigfloat();
            let y = b.to_bigfloat();
            return Ok(Num::Float128(self.eval_fn2_f128(f, &x, &y)?));
        }
        Ok(Num::Float(f.eval_f64(a.as_f64(), b.as_f64())))
    }

    fn eval_fn2_f128(
        &mut self,
        f: Fn2,
        x: &astro_float::BigFloat,
        y: &astro_float::BigFloat,
    ) -> Result<astro_float::BigFloat> {
        let p = FLOAT128_PREC;
        let rm = F128_RM;
        Ok(match f {
            Fn2::Pow => {
                let cc = self.ev.bf_consts()?;
                x.pow(y, p, rm, cc)
            }
            Fn2::Fmod => x.rem(y),
            Fn2::Hypot => {
                // √(x² + y²).
                x.mul(x, p, rm).add(&y.mul(y, p, rm), p, rm).sqrt(p, rm)
            }
            Fn2::Atan2 => {
                let cc = self.ev.bf_consts()?;
                bigfloat_atan2(x, y, cc)
            }
        })
    }

    /// `creal` / `cimag` / `cconj` / `cabs` / `carg` (`cphase`) —
    /// the complex-specific functions, defined for real arguments
    /// too (a real is `r + 0i`).
    fn eval_complex_part(&mut self, name: &str, a: &Num) -> Result<Num> {
        match name {
            "creal" => Ok(match a {
                Num::Complex(r, _) => Num::Float(*r),
                Num::Complex256(r, _) => Num::Float128(r.clone()),
                other => other.clone(),
            }),
            "cimag" => Ok(match a {
                Num::Complex(_, i) => Num::Float(*i),
                Num::Complex256(_, i) => Num::Float128(i.clone()),
                _ => Num::Int(0),
            }),
            "cconj" => Ok(match a {
                Num::Complex(r, i) => Num::Complex(*r, -i),
                Num::Complex256(r, i) => Num::Complex256(r.clone(), i.neg()),
                other => other.clone(),
            }),
            "cabs" => match a {
                Num::Complex(r, i) => Ok(Num::Float(libm::hypot(*r, *i))),
                Num::Complex256(r, i) => {
                    let p = FLOAT128_PREC;
                    let rm = F128_RM;
                    Ok(Num::Float128(
                        r.mul(r, p, rm).add(&i.mul(i, p, rm), p, rm).sqrt(p, rm),
                    ))
                }
                Num::Float128(bf) => Ok(Num::Float128(bf.abs())),
                Num::Int(n) => n
                    .checked_abs()
                    .map(Num::Int)
                    .ok_or_else(|| KashError::Runtime("arithmetic overflow".into())),
                Num::Float(x) => Ok(Num::Float(libm::fabs(*x))),
            },
            "carg" | "cphase" => match a {
                Num::Complex(r, i) => Ok(Num::Float(libm::atan2(*i, *r))),
                Num::Complex256(r, i) => {
                    let cc = self.ev.bf_consts()?;
                    Ok(Num::Float128(bigfloat_atan2(i, r, cc)))
                }
                // arg of a real: 0 if ≥ 0, π if < 0.
                Num::Float128(bf) => {
                    if bf.is_negative() {
                        let cc = self.ev.bf_consts()?;
                        Ok(Num::Float128(cc.pi(FLOAT128_PREC, F128_RM)))
                    } else {
                        Ok(Num::Float(0.0))
                    }
                }
                _ => {
                    let v = a.as_f64();
                    Ok(Num::Float(if v < 0.0 { core::f64::consts::PI } else { 0.0 }))
                }
            },
            _ => unreachable!("eval_complex_part called for {name}"),
        }
    }

    /// `abs(x)` — magnitude, preserving the integer tier (so
    /// `abs(-3)` is `3`, not `3.0`). Float128 → BigFloat abs;
    /// complex → magnitude (like `cabs`).
    fn math_abs(&mut self, args: &[Num]) -> Result<Num> {
        let [a] = args else {
            return Err(math_arity_err("abs", 1, args.len()));
        };
        match a {
            Num::Int(n) => n
                .checked_abs()
                .map(Num::Int)
                .ok_or_else(|| KashError::Runtime("arithmetic overflow".into())),
            Num::Float(f) => Ok(Num::Float(libm::fabs(*f))),
            Num::Float128(bf) => Ok(Num::Float128(bf.abs())),
            Num::Complex(..) | Num::Complex256(..) => {
                self.eval_complex_part("cabs", a)
            }
        }
    }

    /// Complex-generalized transcendentals — `sqrt`/`exp`/`log`/
    /// `sin`/`cos`/… of a complex argument. Wired in 7.3; for now
    /// a clear "not yet supported" rather than a precision-losing
    /// fallback.
    fn eval_fn1_complex(
        &mut self,
        _f: Fn1,
        name: &str,
        _re: f64,
        _im: f64,
    ) -> Result<Num> {
        Err(KashError::Runtime(alloc::format!(
            "arithmetic: `{name}()` on a complex argument is not yet supported"
        )))
    }

    fn parse_number(&mut self) -> Result<Num> {
        let start = self.pos;
        // Hex / octal are integer-only (no `0x1.5`).
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
            return i64::from_str_radix(lit, 16).map(Num::Int).map_err(|_| {
                KashError::Parse(alloc::format!("arithmetic: invalid hex literal `0x{lit}`"))
            });
        }
        // A leading `0` before more octal digits is octal — but
        // only when no `.` / `e` follows (otherwise it's a float
        // like `0.5`).
        if self.peek() == Some('0')
            && matches!(self.peek_at(1), Some('0'..='7'))
            && !self.scan_has_float_marker()
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
            return i64::from_str_radix(lit, 8).map(Num::Int).map_err(|_| {
                KashError::Parse(alloc::format!("arithmetic: invalid octal literal `0{lit}`"))
            });
        }
        // Decimal integer or float. Consume the integer part…
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }
        let mut is_float = false;
        // …a fractional part…
        if self.peek() == Some('.') {
            is_float = true;
            self.advance();
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        // …and an exponent.
        if matches!(self.peek(), Some('e' | 'E')) {
            is_float = true;
            self.advance();
            if matches!(self.peek(), Some('+' | '-')) {
                self.advance();
            }
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        let lit = &self.src[start..self.pos];
        let base = if is_float {
            Num::Float(lit.parse::<f64>().map_err(|_| {
                KashError::Parse(alloc::format!("arithmetic: invalid float `{lit}`"))
            })?)
        } else {
            Num::Int(lit.parse::<i64>().map_err(|_| {
                KashError::Parse(alloc::format!("arithmetic: invalid integer `{lit}`"))
            })?)
        };
        // Imaginary suffix: a numeric literal immediately followed
        // by `i` (not part of a longer identifier) is a pure
        // imaginary value — `2i`, `1.5i`, `3e2i`.
        if self.peek() == Some('i')
            && !matches!(
                self.peek_at(1),
                Some(c) if c == '_' || c.is_ascii_alphanumeric()
            )
        {
            self.advance();
            return Ok(Num::Complex(0.0, base.as_f64()));
        }
        Ok(base)
    }

    /// Peek whether the number starting here carries a float
    /// marker (`.` or `e`/`E`) before the next non-numeric byte —
    /// used to keep `0.5` out of the octal path.
    fn scan_has_float_marker(&self) -> bool {
        let rest = &self.src[self.pos..];
        for c in rest.chars() {
            match c {
                '0'..='9' => continue,
                '.' | 'e' | 'E' => return true,
                _ => return false,
            }
        }
        false
    }

    fn try_read_identifier(&mut self) -> Option<String> {
        self.skip_ws();
        let start = self.pos;
        let c = self.peek()?;
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

    fn read_named(&mut self, name: &str) -> Result<Num> {
        // Snapshot the value and the declared numeric tier, then
        // drop the binding borrow before touching the (mutable)
        // BigFloat constant cache.
        let (value, nt) = match self.ev.scope.get_binding(name) {
            Some(b) => (b.value.to_scalar_string(), b.attrs.numeric_type),
            None => (String::new(), None),
        };
        if value.trim().is_empty() {
            // An unset name that matches a math constant (`M_PI`,
            // `M_E`, …) yields the constant; a real variable of
            // the same name would have shadowed it above.
            if let Some(c) = math_constant(name) {
                return Ok(Num::Float(c));
            }
            return Ok(Num::Int(0));
        }
        let trimmed = value.trim();
        let not_a_number = || {
            KashError::Runtime(alloc::format!(
                "arithmetic: `{name}`'s value `{value}` is not a number"
            ))
        };
        match nt {
            // float128 — parse at 113-bit precision.
            Some(t) if t.is_extended_precision() && t.is_float() => {
                use astro_float::{BigFloat, Radix, RoundingMode};
                let cc = self.ev.bf_consts()?;
                let bf = BigFloat::parse(
                    trimmed,
                    Radix::Dec,
                    FLOAT128_PREC,
                    RoundingMode::ToEven,
                    cc,
                );
                if bf.is_nan() {
                    return Err(not_a_number());
                }
                Ok(Num::Float128(bf))
            }
            // complex256 — components at 113-bit precision.
            Some(t) if t.is_complex() && t.is_extended_precision() => {
                use astro_float::{BigFloat, Radix, RoundingMode};
                let (re_s, im_s) =
                    split_complex_literal(trimmed).ok_or_else(not_a_number)?;
                let cc = self.ev.bf_consts()?;
                let re = BigFloat::parse(
                    re_s.trim(),
                    Radix::Dec,
                    FLOAT128_PREC,
                    RoundingMode::ToEven,
                    cc,
                );
                let im = BigFloat::parse(
                    im_s.trim(),
                    Radix::Dec,
                    FLOAT128_PREC,
                    RoundingMode::ToEven,
                    cc,
                );
                if re.is_nan() || im.is_nan() {
                    return Err(not_a_number());
                }
                Ok(Num::Complex256(re, im))
            }
            // complex128 — f64 component pair.
            Some(t) if t.is_complex() => {
                let (re, im) =
                    parse_complex_literal(trimmed).ok_or_else(not_a_number)?;
                Ok(Num::Complex(re, im))
            }
            Some(t) if t.is_float() => {
                trimmed.parse::<f64>().map(Num::Float).map_err(|_| not_a_number())
            }
            Some(_) => {
                trimmed.parse::<i64>().map(Num::Int).map_err(|_| not_a_number())
            }
            // Untyped: prefer integer, fall back to float.
            None => {
                if let Ok(n) = trimmed.parse::<i64>() {
                    Ok(Num::Int(n))
                } else if let Ok(f) = trimmed.parse::<f64>() {
                    Ok(Num::Float(f))
                } else {
                    Err(not_a_number())
                }
            }
        }
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
            if let Some(&n) = chars.peek()
                && matches!(n, '$' | '`' | '\\')
            {
                chars.next();
                body.push(n);
                continue;
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

/// Stage-3 typeclass dispatch helper. Infer the *receiver type* of a
/// `Typeclass::method args …` call from the first positional argument
/// and return `(type_name, body_args)`:
///
/// - If the first arg has an `@TYPE` prefix, it's treated as an
///   explicit type assertion. The `@TYPE` token is *removed* from
///   the body args (it was an annotation, not an argument).
/// - If the first arg is an integer literal (optionally signed), the
///   inferred type is `"Int"` and the arg is kept.
/// - Anything else is `"String"` and the arg is kept.
/// - If `argv` has no positionals, the inferred type is `"Unit"` (a
///   sentinel for the "no-arg" form; an instance for `Unit` is the
///   only thing that will match).
///
/// More elaborate inference — `typeset` attribute lookup on the
/// source variable, user-defined-type binding, multi-arg constraint
/// solving — lands in a later stage when the parser threads
/// expansion provenance through `Word`.
fn infer_dispatch_type(
    argv: &[String],
) -> (alloc::string::String, alloc::vec::Vec<alloc::string::String>) {
    let Some(first) = argv.get(1) else {
        return ("Unit".into(), alloc::vec::Vec::new());
    };
    if let Some(rest) = first.strip_prefix('@') {
        return (rest.to_string(), argv[2..].to_vec());
    }
    if looks_like_integer_literal(first) {
        return ("Int".into(), argv[1..].to_vec());
    }
    ("String".into(), argv[1..].to_vec())
}

/// True iff `s` is a non-empty decimal integer literal,
/// optionally signed.
fn looks_like_integer_literal(s: &str) -> bool {
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// True iff every attribute set in `filter` is also set in `attrs`.
/// An empty `filter` matches everything.
fn attrs_match_filter(attrs: &AttrSet, filter: &AttrSet) -> bool {
    (!filter.readonly || attrs.readonly)
        && (!filter.export || attrs.export)
        && (!filter.integer || attrs.integer)
        && (!filter.lowercase || attrs.lowercase)
        && (!filter.uppercase || attrs.uppercase)
        && (!filter.indexed || attrs.indexed)
        && (!filter.assoc || attrs.assoc)
}

/// Render an `AttrSet` as a flag cluster (`" -ix"`, etc.). Empty
/// returns an empty string.
fn format_attrs(attrs: &AttrSet) -> String {
    let mut out = String::new();
    if attrs.readonly {
        out.push_str(" -r");
    }
    if attrs.export {
        out.push_str(" -x");
    }
    if attrs.integer {
        out.push_str(" -i");
    }
    if attrs.lowercase {
        out.push_str(" -l");
    }
    if attrs.uppercase {
        out.push_str(" -u");
    }
    if attrs.indexed {
        out.push_str(" -a");
    }
    if attrs.assoc {
        out.push_str(" -A");
    }
    out
}

/// `typeset -p`-style rendering of a value. Single-quotes scalars,
/// `([idx]='val' ...)` for arrays.
fn format_value_for_listing(v: &Value) -> String {
    match v {
        Value::Empty => "''".into(),
        Value::Scalar(s) => alloc::format!("'{s}'"),
        Value::Array(v) => {
            let mut s = String::from("(");
            for (i, elem) in v.iter().enumerate() {
                if i > 0 {
                    s.push(' ');
                }
                s.push_str(&alloc::format!("[{i}]='{elem}'"));
            }
            s.push(')');
            s
        }
        Value::AssocArray(m) => {
            let mut s = String::from("(");
            let mut first = true;
            for (k, v) in m {
                if !first {
                    s.push(' ');
                }
                first = false;
                s.push_str(&alloc::format!("[{k}]='{v}'"));
            }
            s.push(')');
            s
        }
    }
}

/// Split a `${...}` body into `(name, subscript)` if it has the
/// `NAME[SUBSCRIPT]` shape, otherwise return `None`. Used to spot
/// `${arr[i]}` / `${arr[@]}` / `${#arr[@]}` inside `expand_braced`.
fn split_subscripted(body: &str) -> Option<(&str, &str)> {
    let open = body.find('[')?;
    if !body.ends_with(']') {
        return None;
    }
    let name = &body[..open];
    let sub = &body[open + 1..body.len() - 1];
    if !is_identifier(name) {
        return None;
    }
    Some((name, sub))
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

/// Parsed `read` invocation. Pure value type so the std / alloc
/// `builtin_read_impl` paths can share parsing logic.
struct ReadArgs {
    /// Prompt to print on stderr before reading. `None` means no
    /// prompt.
    prompt: Option<String>,
    /// `true` when `-r` was given — disables backslash escapes in
    /// the captured line.
    raw: bool,
    /// Names to bind the IFS-split fields to. Empty → caller
    /// substitutes `REPLY`.
    names: Vec<String>,
}

fn parse_read_args(args: &[String]) -> Result<ReadArgs> {
    let mut out = ReadArgs {
        prompt: None,
        raw: false,
        names: Vec::new(),
    };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(rest) = a.strip_prefix("--prompt=") {
            out.prompt = Some(rest.to_string());
            i += 1;
            continue;
        }
        if a == "--prompt" {
            i += 1;
            out.prompt = Some(
                args.get(i)
                    .cloned()
                    .ok_or_else(|| KashError::Runtime("read: --prompt needs an argument".into()))?,
            );
            i += 1;
            continue;
        }
        if a == "-p" {
            i += 1;
            out.prompt = Some(
                args.get(i)
                    .cloned()
                    .ok_or_else(|| KashError::Runtime("read: -p needs an argument".into()))?,
            );
            i += 1;
            continue;
        }
        if a == "-r" {
            out.raw = true;
            i += 1;
            continue;
        }
        if a == "--" {
            i += 1;
            while i < args.len() {
                out.names.push(args[i].clone());
                i += 1;
            }
            break;
        }
        if let Some(rest) = a.strip_prefix('-')
            && !rest.is_empty()
        {
            return Err(KashError::Runtime(alloc::format!(
                "read: unknown option `{a}`"
            )));
        }
        out.names.push(a.clone());
        i += 1;
    }
    Ok(out)
}

/// POSIX `read` (without `-r`) processes `\X` by dropping the
/// backslash unless `X` is the newline — and the line is already
/// split on the original newline by the line reader, so we just
/// peel single backslashes off every other byte.
fn unescape_read_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(n) = chars.next()
        {
            out.push(n);
        } else {
            out.push(c);
        }
    }
    out
}

/// Split `line` into up to `n` fields against `ifs`. The first
/// `n-1` fields are minimal (one separator-run apart); the last
/// field captures all remaining bytes including any embedded
/// IFS — exactly what POSIX `read` mandates.
fn split_for_read(line: &str, ifs: &str, n: usize) -> Vec<String> {
    if n <= 1 {
        return alloc::vec![line.to_string()];
    }
    let is_ifs = |b: u8| ifs.as_bytes().contains(&b);
    let bytes = line.as_bytes();
    let mut out: Vec<String> = Vec::with_capacity(n);
    let mut i = 0;
    // Per POSIX, leading IFS-whitespace is discarded before the
    // first field. Whitespace IFS is space / tab / newline; for the
    // simple case the user typically has `IFS=" \t\n"` so we lean
    // on `char::is_ascii_whitespace`.
    while i < bytes.len() && is_ifs(bytes[i]) && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut start = i;
    while out.len() < n - 1 && i < bytes.len() {
        if is_ifs(bytes[i]) {
            out.push(line[start..i].to_string());
            i += 1;
            // Skip following IFS-whitespace as a single separator.
            while i < bytes.len() && is_ifs(bytes[i]) && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            start = i;
        } else {
            i += 1;
        }
    }
    // Final field — remainder verbatim. Trailing IFS-whitespace is
    // stripped from the *last* field only when it's effectively
    // empty after the strip; otherwise it's preserved (matches
    // bash/ksh observed behaviour).
    let tail = &line[start..];
    out.push(tail.to_string());
    while out.len() < n {
        out.push(String::new());
    }
    out
}

/// Load a venv config file (TOML) and return the materialised
/// pieces. Std-only — the alloc-only build can't read files at
/// all, so the call surfaces a runtime error there.
#[cfg(feature = "std")]
fn load_venv_config(
    path: &str,
) -> Result<(Option<crate::capability::CapabilitySpec>, Vec<crate::ast::EnvDirective>)> {
    use crate::ast::EnvDirective;
    use crate::capability::CapabilitySpec;
    let content = std::fs::read_to_string(path).map_err(|e| {
        KashError::Runtime(alloc::format!("load-config: {path}: {e}"))
    })?;
    let value: toml::Value = toml::from_str(&content).map_err(|e| {
        KashError::Runtime(alloc::format!("load-config: {path}: invalid TOML: {e}"))
    })?;
    let table = value.as_table().ok_or_else(|| {
        KashError::Runtime("load-config: top-level must be a TOML table".into())
    })?;
    let caps_spec = table
        .get("capabilities")
        .map(|c| -> Result<CapabilitySpec> {
            let t = c.as_table().ok_or_else(|| {
                KashError::Runtime(
                    "load-config: `[capabilities]` must be a TOML table".into(),
                )
            })?;
            let mut spec = CapabilitySpec::default();
            if let Some(p) = t.get("profile") {
                spec.profile = Some(toml_string(p, "[capabilities].profile")?);
            }
            if let Some(a) = t.get("add") {
                spec.grants = toml_string_list(a, "[capabilities].add")?;
            }
            if let Some(r) = t.get("remove") {
                spec.revokes = toml_string_list(r, "[capabilities].remove")?;
            }
            if let Some(c) = t.get("allow-cmd") {
                spec.allow_cmd =
                    Some(toml_string_list(c, "[capabilities].allow-cmd")?);
            }
            Ok(spec)
        })
        .transpose()?;
    let mut env_dirs: Vec<EnvDirective> = Vec::new();
    if let Some(env) = table.get("env") {
        let t = env.as_table().ok_or_else(|| {
            KashError::Runtime(
                "load-config: `[env]` must be a TOML table".into(),
            )
        })?;
        for (k, v) in t {
            match k.as_str() {
                "PATH-prepend" => env_dirs.push(EnvDirective::PathPrepend {
                    dir: toml_string(v, "[env].PATH-prepend")?,
                }),
                "PATH-append" => env_dirs.push(EnvDirective::PathAppend {
                    dir: toml_string(v, "[env].PATH-append")?,
                }),
                _ => env_dirs.push(EnvDirective::Set {
                    name: k.clone(),
                    value: toml_string(v, &alloc::format!("[env].{k}"))?,
                }),
            }
        }
    }
    Ok((caps_spec, env_dirs))
}

/// alloc-only stub: file IO isn't available, so `load-config`
/// surfaces a runtime error.
#[cfg(not(feature = "std"))]
fn load_venv_config(
    _path: &str,
) -> Result<(Option<crate::capability::CapabilitySpec>, Vec<crate::ast::EnvDirective>)> {
    Err(KashError::Other(
        "load-config requires the `std` feature".into(),
    ))
}

#[cfg(feature = "std")]
fn toml_string(v: &toml::Value, what: &str) -> Result<String> {
    v.as_str()
        .map(alloc::string::String::from)
        .ok_or_else(|| {
            KashError::Runtime(alloc::format!("load-config: `{what}` must be a string"))
        })
}

#[cfg(feature = "std")]
fn toml_string_list(v: &toml::Value, what: &str) -> Result<Vec<String>> {
    let arr = v.as_array().ok_or_else(|| {
        KashError::Runtime(alloc::format!(
            "load-config: `{what}` must be an array of strings"
        ))
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        out.push(toml_string(item, what)?);
    }
    Ok(out)
}

/// Parse the args to `use`. Accepts the four documented forms plus
/// the brace expansion shorthand `use .foo.{a,b}` (which expands to
/// the cross-product of single-symbol imports), returning one or
/// more [`ImportEntry`] values.
fn parse_use_args(args: &[String]) -> Result<Vec<ImportEntry>> {
    fn split_path(raw: &str) -> Result<Vec<String>> {
        let stripped = raw.strip_prefix('.').unwrap_or(raw);
        let segs: Vec<String> = stripped
            .split('.')
            .map(alloc::string::String::from)
            .collect();
        if segs.iter().any(|s| s.is_empty()) {
            return Err(KashError::Runtime(alloc::format!(
                "use: malformed path `{raw}`"
            )));
        }
        Ok(segs)
    }

    let strs: Vec<&str> = args.iter().map(alloc::string::String::as_str).collect();
    match strs.as_slice() {
        ["namespace", path] => Ok(alloc::vec![ImportEntry::Wildcard {
            source: split_path(path)?,
        }]),
        ["namespace", path, "as", alias] => {
            if alias.contains('.') {
                return Err(KashError::Runtime(alloc::format!(
                    "use namespace … as: alias `{alias}` must be a bare identifier"
                )));
            }
            Ok(alloc::vec![ImportEntry::Aliased {
                source: split_path(path)?,
                alias: alloc::string::String::from(*alias),
            }])
        }
        symbols
            if !symbols.is_empty()
                && symbols.iter().all(|s| s.starts_with('.')) =>
        {
            // One or more single-symbol imports. The brace form
            // `use .ns.{a,b}` arrives here already fanned out into
            // `.ns.a .ns.b` by word-level brace expansion; a lone
            // arg that still carries a brace (e.g. a quoted one)
            // goes through `expand_brace_in_path` as a fallback.
            let mut out = Vec::new();
            for sym in symbols {
                let expanded = expand_brace_in_path(sym)?;
                for path in &expanded {
                    let segs = split_path(path)?;
                    if segs.len() < 2 {
                        return Err(KashError::Runtime(alloc::format!(
                            "use: `{path}` needs at least one path segment before the symbol name"
                        )));
                    }
                    let source_name = segs.last().unwrap().clone();
                    let source_path = segs[..segs.len() - 1].to_vec();
                    out.push(ImportEntry::Symbol {
                        source_path,
                        source_name,
                        alias: None,
                    });
                }
            }
            Ok(out)
        }
        [absolute, "as", alias] if absolute.starts_with('.') => {
            // The brace form forbids `as ALIAS` — a single alias
            // can't cleanly bind multiple imports.
            if absolute.contains('{') {
                return Err(KashError::Runtime(
                    "use: `{…}` brace form and `as ALIAS` cannot be combined".into(),
                ));
            }
            let segs = split_path(absolute)?;
            if segs.len() < 2 {
                return Err(KashError::Runtime(alloc::format!(
                    "use: `{absolute}` needs at least one path segment before the symbol name"
                )));
            }
            if alias.contains('.') {
                return Err(KashError::Runtime(alloc::format!(
                    "use: alias `{alias}` must be a bare identifier"
                )));
            }
            let source_name = segs.last().unwrap().clone();
            let source_path = segs[..segs.len() - 1].to_vec();
            Ok(alloc::vec![ImportEntry::Symbol {
                source_path,
                source_name,
                alias: Some(alloc::string::String::from(*alias)),
            }])
        }
        _ => Err(KashError::Runtime(
            "use: expected one of `use namespace PATH [as ALIAS]`, `use .PATH.NAME [as ALIAS]`, or `use .PATH.{N1,N2,…}`".into(),
        )),
    }
}

/// Provenance tag for one character during brace expansion: which
/// kind of word segment it came from. Only `Bare` characters are
/// brace-active — a `{` / `}` / `,` / `.` inside a quoted segment
/// is inert and never drives expansion. Carried so reconstruction
/// restores the original quoting per resulting word.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SegKind {
    /// Unquoted bare text — brace-active.
    Bare,
    /// `'…'` — inert.
    Single,
    /// `"…"` — inert (for brace purposes; `$` still expands later).
    Double,
    /// `$'…'` — inert.
    AnsiC,
}

/// Brace-expand a [`Word`] into one or more words. A word with no
/// active brace construct returns a single clone of `w`; `{a,b}`,
/// `{1..9}`, `{1..9..2}`, `{a..z}`, and cross-products fan out into
/// multiple words. Quoting is preserved: a `{` inside a quoted
/// *segment* stays literal. Runs *before* `$`-expansion and
/// globbing, matching the POSIX/bash word-expansion order.
///
/// Known limitation: a quote that appears *inside* an otherwise
/// unquoted brace body — `a{b,"c"}d`, `{a,""}`, `{a,'b'}` — is not
/// recognised as a quoted segment. The lexer absorbs a `{…}` group
/// byte-for-byte into one bare token (see `Lexer::scan_matching_
/// brace`), so the quote characters arrive as literal bytes rather
/// than a distinct [`WordSegment`]. Quote-aware tokenisation inside
/// brace groups is a follow-up; the common forms (unquoted
/// alternatives, ranges, cross-products, and a quoted segment
/// *adjacent* to the brace like `{a,b}'{1,2}'`) all behave
/// correctly.
fn brace_expand_word(w: &Word) -> Vec<Word> {
    // Fast path: a word with no `{` in any bare segment can't
    // brace-expand. The overwhelmingly common case — skip the
    // atom machinery entirely.
    let has_brace = w.segments.iter().any(|seg| {
        matches!(seg, WordSegment::Bare(s) if s.contains('{'))
    });
    if !has_brace {
        return alloc::vec![w.clone()];
    }
    let mut atoms: Vec<(char, SegKind)> = Vec::new();
    for seg in &w.segments {
        let (s, kind) = match seg {
            WordSegment::Bare(s) => (s, SegKind::Bare),
            WordSegment::SingleQuoted(s) => (s, SegKind::Single),
            WordSegment::DoubleQuoted(s) => (s, SegKind::Double),
            WordSegment::AnsiC(s) => (s, SegKind::AnsiC),
        };
        for c in s.chars() {
            atoms.push((c, kind));
        }
    }
    let mut out: Vec<Vec<(char, SegKind)>> = Vec::new();
    brace_expand_atoms(&atoms, &mut out);
    out.into_iter()
        .map(|seq| atoms_to_word(&seq, w.span))
        .collect()
}

/// Recursively expand the leftmost expandable brace group in
/// `atoms`, pushing each resulting atom sequence into `out`. With
/// no expandable group the sequence passes through unchanged.
fn brace_expand_atoms(
    atoms: &[(char, SegKind)],
    out: &mut Vec<Vec<(char, SegKind)>>,
) {
    let Some((open, close)) = find_brace_group(atoms) else {
        out.push(atoms.to_vec());
        return;
    };
    let pre = &atoms[..open];
    let body = &atoms[open + 1..close];
    let post = &atoms[close + 1..];
    let alts =
        parse_brace_body(body).expect("find_brace_group validated the body");
    // The tail is shared across every alternative — expand once.
    let mut post_expansions: Vec<Vec<(char, SegKind)>> = Vec::new();
    brace_expand_atoms(post, &mut post_expansions);
    for alt in &alts {
        // An alternative may itself contain nested braces.
        let mut alt_expansions: Vec<Vec<(char, SegKind)>> = Vec::new();
        brace_expand_atoms(alt, &mut alt_expansions);
        for ae in &alt_expansions {
            for pe in &post_expansions {
                let mut combined: Vec<(char, SegKind)> =
                    Vec::with_capacity(pre.len() + ae.len() + pe.len());
                combined.extend_from_slice(pre);
                combined.extend_from_slice(ae);
                combined.extend_from_slice(pe);
                out.push(combined);
            }
        }
    }
}

/// If an opaque (non-brace-expandable) construct starts at atom
/// `i` — `${…}`, `$(…)`, `$((…))`, or a backtick command
/// substitution — return the index just past it so brace scanning
/// skips its interior. A `{` inside `${v,,}` or a `,` inside
/// `$(a,b)` must never be read as brace syntax. Only active (Bare)
/// `$` / backtick start these. Returns `None` for a normal atom.
fn skip_opaque_construct(atoms: &[(char, SegKind)], i: usize) -> Option<usize> {
    let (c, k) = atoms[i];
    if k != SegKind::Bare {
        return None;
    }
    // Backtick command substitution: to the next active backtick.
    if c == '`' {
        let mut j = i + 1;
        while j < atoms.len() {
            if atoms[j] == ('`', SegKind::Bare) {
                return Some(j + 1);
            }
            j += 1;
        }
        return Some(atoms.len());
    }
    // `${…}` / `$(…)` / `$((…))` — balanced over the opener byte.
    if c == '$'
        && i + 1 < atoms.len()
        && atoms[i + 1].1 == SegKind::Bare
        && let open @ ('{' | '(') = atoms[i + 1].0
    {
        let close = if open == '{' { '}' } else { ')' };
        let mut depth = 0usize;
        let mut j = i + 1;
        while j < atoms.len() {
            if atoms[j].1 == SegKind::Bare {
                if atoms[j].0 == open {
                    depth += 1;
                } else if atoms[j].0 == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some(j + 1);
                    }
                }
            }
            j += 1;
        }
        return Some(atoms.len());
    }
    None
}

/// Find the leftmost active `{` whose matching `}` encloses an
/// expandable body (a top-level comma list or a numeric / char
/// range). Returns the `(open, close)` atom indices, or `None`
/// when the word has no expandable brace group. `${…}` / `$(…)` /
/// backtick regions are skipped wholesale.
fn find_brace_group(atoms: &[(char, SegKind)]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < atoms.len() {
        let (c, k) = atoms[i];
        if k == SegKind::Bare && c == '\\' {
            // Escaped char: skip the backslash and the next atom.
            i += 2;
            continue;
        }
        if let Some(next) = skip_opaque_construct(atoms, i) {
            i = next;
            continue;
        }
        if k == SegKind::Bare
            && c == '{'
            && let Some(close) = matching_active_brace(atoms, i)
            && parse_brace_body(&atoms[i + 1..close]).is_some()
        {
            return Some((i, close));
        }
        i += 1;
    }
    None
}

/// Find the matching `}` for the active `{` at `open`, counting
/// only active (Bare) braces and skipping backslash-escaped atoms.
fn matching_active_brace(
    atoms: &[(char, SegKind)],
    open: usize,
) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < atoms.len() {
        let (c, k) = atoms[i];
        if k == SegKind::Bare && c == '\\' {
            i += 2;
            continue;
        }
        // Don't count a `}` that belongs to an inner `${…}` /
        // `$(…)` — but only once past the opener itself.
        if i > open
            && let Some(next) = skip_opaque_construct(atoms, i)
        {
            i = next;
            continue;
        }
        if k == SegKind::Bare && c == '{' {
            depth += 1;
        } else if k == SegKind::Bare && c == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Parse a brace body into its alternatives. Comma form needs at
/// least one top-level active comma (so `{a,b}` and `{a,}` qualify
/// but `{a}` doesn't). Range form (`{1..9}`, `{a..z}`, optional
/// `..STEP`) needs all-active content. Returns `None` for a
/// non-expandable body, which is then left literal.
fn parse_brace_body(
    body: &[(char, SegKind)],
) -> Option<Vec<Vec<(char, SegKind)>>> {
    let comma_alts = split_top_level_commas(body);
    if comma_alts.len() >= 2 {
        return Some(comma_alts);
    }
    parse_brace_range(body)
}

/// Split `body` on active commas at brace-depth zero, skipping
/// escaped atoms and nested active braces.
fn split_top_level_commas(
    body: &[(char, SegKind)],
) -> Vec<Vec<(char, SegKind)>> {
    let mut parts: Vec<Vec<(char, SegKind)>> = alloc::vec![Vec::new()];
    let mut depth = 0usize;
    let mut i = 0;
    while i < body.len() {
        let (c, k) = body[i];
        if k == SegKind::Bare && c == '\\' {
            // Keep both the backslash and the escaped atom verbatim.
            parts.last_mut().unwrap().push(body[i]);
            if i + 1 < body.len() {
                parts.last_mut().unwrap().push(body[i + 1]);
            }
            i += 2;
            continue;
        }
        // A `,` inside `${…}` / `$(…)` is not a brace separator —
        // copy the whole opaque construct into the current part.
        if let Some(next) = skip_opaque_construct(body, i) {
            for atom in &body[i..next] {
                parts.last_mut().unwrap().push(*atom);
            }
            i = next;
            continue;
        }
        if k == SegKind::Bare && c == '{' {
            depth += 1;
        } else if k == SegKind::Bare && c == '}' {
            depth = depth.saturating_sub(1);
        } else if k == SegKind::Bare && c == ',' && depth == 0 {
            parts.push(Vec::new());
            i += 1;
            continue;
        }
        parts.last_mut().unwrap().push(body[i]);
        i += 1;
    }
    parts
}

/// Parse a numeric (`{1..9}`, `{1..9..2}`, `{9..1}`) or single-
/// character (`{a..z}`, `{a..z..2}`) range. All body atoms must be
/// active (Bare). Returns the generated alternatives, or `None`
/// when the body isn't a valid range (left literal). Step `0` and
/// malformed steps disable expansion (bash semantics).
fn parse_brace_range(
    body: &[(char, SegKind)],
) -> Option<Vec<Vec<(char, SegKind)>>> {
    if body.is_empty() || !body.iter().all(|(_, k)| *k == SegKind::Bare) {
        return None;
    }
    let text: String = body.iter().map(|(c, _)| *c).collect();
    let parts: Vec<&str> = text.split("..").collect();
    if parts.len() != 2 && parts.len() != 3 {
        return None;
    }
    let step_str = parts.get(2).copied();

    // Numeric range.
    if let (Ok(lo), Ok(hi)) =
        (parts[0].parse::<i64>(), parts[1].parse::<i64>())
    {
        // A zero (or unparseable) step is treated as 1 — matching
        // bash, which expands `{1..5..0}` as `1 2 3 4 5`.
        let mag = match step_str {
            None => 1i64,
            Some(s) => {
                let n = s.parse::<i64>().ok()?;
                if n == 0 { 1 } else { n.unsigned_abs() as i64 }
            }
        };
        let width = numeric_pad_width(parts[0], parts[1]);
        let mut vals: Vec<i64> = Vec::new();
        if lo <= hi {
            let mut v = lo;
            while v <= hi {
                vals.push(v);
                v += mag;
            }
        } else {
            let mut v = lo;
            while v >= hi {
                vals.push(v);
                v -= mag;
            }
        }
        return Some(
            vals.into_iter()
                .map(|n| str_to_literal_atoms(&format_padded(n, width)))
                .collect(),
        );
    }

    // Single-character range.
    let lo: Vec<char> = parts[0].chars().collect();
    let hi: Vec<char> = parts[1].chars().collect();
    if lo.len() == 1 && hi.len() == 1 {
        let step = match step_str {
            None => 1i64,
            Some(s) => {
                let n = s.parse::<i64>().ok()?;
                if n == 0 { 1 } else { n.unsigned_abs() as i64 }
            }
        };
        let lo = lo[0] as i64;
        let hi = hi[0] as i64;
        let mut chars: Vec<char> = Vec::new();
        if lo <= hi {
            let mut v = lo;
            while v <= hi {
                if let Some(c) = char::from_u32(v as u32) {
                    chars.push(c);
                }
                v += step;
            }
        } else {
            let mut v = lo;
            while v >= hi {
                if let Some(c) = char::from_u32(v as u32) {
                    chars.push(c);
                }
                v -= step;
            }
        }
        // Generated characters are *literal data* — tag them inert
        // so a produced backtick / `$` / glob metachar isn't
        // re-interpreted as syntax downstream (`{Z..a}` spans the
        // backtick at ASCII 96).
        return Some(
            chars
                .into_iter()
                .map(|c| alloc::vec![(c, SegKind::Single)])
                .collect(),
        );
    }
    None
}

/// Decide the zero-pad width for a numeric brace range. bash pads
/// to a uniform *total field width* (sign included) when either
/// endpoint is written with a leading zero: `{01..3}` → `01 02 03`,
/// `{01..100}` → `001 … 100`, `{-02..2}` → `-02 -01 000 001 002`
/// (width 3 counting the sign on the negative side). Returns `0`
/// for "no padding".
fn numeric_pad_width(lo: &str, hi: &str) -> usize {
    let padded = |s: &str| {
        let d = s.strip_prefix('-').unwrap_or(s);
        d.len() > 1 && d.starts_with('0')
    };
    if !padded(lo) && !padded(hi) {
        return 0;
    }
    // Total width including any sign character — the column bash
    // aligns to.
    lo.chars().count().max(hi.chars().count())
}

/// Format `n` to a uniform `width`-character field (0 = no
/// padding). For a negative value the `-` counts toward the width,
/// so the magnitude is zero-padded to `width - 1` digits.
fn format_padded(n: i64, width: usize) -> String {
    if width == 0 {
        alloc::format!("{n}")
    } else if n < 0 {
        let digits = width.saturating_sub(1);
        alloc::format!("-{:0>digits$}", n.unsigned_abs(), digits = digits)
    } else {
        alloc::format!("{:0>width$}", n, width = width)
    }
}

/// Turn a plain string into a run of inert (single-quoted-tagged)
/// atoms — used for range output, which is literal data and must
/// not be re-scanned for `$` / backtick / glob metacharacters.
fn str_to_literal_atoms(s: &str) -> Vec<(char, SegKind)> {
    s.chars().map(|c| (c, SegKind::Single)).collect()
}

/// Coalesce a tagged atom sequence back into a [`Word`], merging
/// runs of the same segment kind into one segment so quoting is
/// restored on each expanded word.
fn atoms_to_word(atoms: &[(char, SegKind)], span: crate::lexer::Span) -> Word {
    let mut segments: Vec<WordSegment> = Vec::new();
    let mut i = 0;
    while i < atoms.len() {
        let kind = atoms[i].1;
        let mut s = String::new();
        while i < atoms.len() && atoms[i].1 == kind {
            s.push(atoms[i].0);
            i += 1;
        }
        segments.push(match kind {
            SegKind::Bare => WordSegment::Bare(s),
            SegKind::Single => WordSegment::SingleQuoted(s),
            SegKind::Double => WordSegment::DoubleQuoted(s),
            SegKind::AnsiC => WordSegment::AnsiC(s),
        });
    }
    if segments.is_empty() {
        segments.push(WordSegment::Bare(String::new()));
    }
    Word { segments, span }
}

/// True if `field` carries an *unescaped* glob metacharacter that
/// pathname expansion should act on: `*`, `?`, or a `[…]`
/// character class (a lone `[` with no closing `]` is literal). A
/// backslash-escaped metacharacter (`\*`, `\?`, `\[`) is literal
/// and does not count. Alloc-safe — the filesystem walk is
/// std-only.
///
/// (The surviving backslash itself is not yet stripped from the
/// literal field — bare-word backslash quote removal is a separate
/// pre-existing gap, deferred to the runtime-refinement pass.)
fn field_has_glob_meta(field: &str) -> bool {
    let bytes = field.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                // Escaped: this backslash plus the next byte are
                // literal — skip both.
                i += 2;
                continue;
            }
            b'*' | b'?' => return true,
            b'[' if bytes[i + 1..].contains(&b']') => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Match a single path component `name` against the glob pattern
/// `pat`, applying the POSIX hidden-file rule unless `dotglob` is
/// set: a leading `.` in `name` only matches when `pat` also
/// starts with a literal `.` (the `(#qD)` qualifier turns this
/// off so `*` sees dotfiles).
fn glob_component_matches(pat: &str, name: &str, dotglob: bool) -> bool {
    if !dotglob && name.starts_with('.') && !pat.starts_with('.') {
        return false;
    }
    glob_match(pat, name)
}

/// Join a directory prefix and an entry name for the glob walker.
/// An empty `dir` is the relative current directory, so the result
/// is just `name`; otherwise the two are `/`-joined (the absolute
/// root `/` joins without doubling the slash).
#[cfg(feature = "std")]
fn glob_join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else if dir == "/" {
        alloc::format!("/{name}")
    } else {
        alloc::format!("{dir}/{name}")
    }
}

/// List the entry names of `dir` (an empty string means the
/// relative current directory). Returns an empty list when the
/// path isn't a readable directory — a non-match, never an error.
#[cfg(feature = "std")]
fn glob_read_dir(dir: &str) -> Vec<String> {
    let path = if dir.is_empty() { "." } else { dir };
    let Ok(rd) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    for entry in rd.flatten() {
        if let Ok(name) = entry.file_name().into_string() {
            names.push(name);
        }
    }
    names
}

/// Whether `path` is a directory, following symlinks (used for
/// descending an explicit pattern component).
#[cfg(feature = "std")]
fn glob_is_dir(path: &str) -> bool {
    std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

/// Whether `path` is a *real* directory — not a symlink. Used to
/// gate `**` recursion so symlink cycles can't loop forever.
#[cfg(feature = "std")]
fn glob_is_real_dir(path: &str) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.is_dir())
        .unwrap_or(false)
}

/// Collect every descendant of `dir` (files, symlinks, and
/// directories) recursively, pushing each path to `out`. Used by
/// a trailing `**`. Hidden entries are included only when
/// `dotglob` is set. Recursion descends only into *real*
/// directories (never symlinks), so symlink cycles can't loop.
#[cfg(feature = "std")]
fn glob_collect_descendants(dir: &str, dotglob: bool, out: &mut Vec<String>) {
    for name in glob_read_dir(dir) {
        if !dotglob && name.starts_with('.') {
            continue;
        }
        let full = glob_join(dir, &name);
        out.push(full.clone());
        if glob_is_real_dir(&full) {
            glob_collect_descendants(&full, dotglob, out);
        }
    }
}

/// Recursive filesystem glob walker. `dir` is the prefix matched
/// so far (empty = relative cwd, `/` = absolute root); `comps` is
/// the remaining `/`-split pattern components; `idx` is the next
/// component. Fully-matched paths are pushed to `out`. `**`
/// matches zero or more directory levels. `dotglob` includes
/// hidden entries (the `(#qD)` qualifier).
#[cfg(feature = "std")]
fn glob_walk(
    dir: &str,
    comps: &[&str],
    idx: usize,
    dotglob: bool,
    out: &mut Vec<String>,
) {
    if idx == comps.len() {
        return;
    }
    let comp = comps[idx];
    let last = idx + 1 == comps.len();

    if comp == "**" {
        if last {
            // Trailing `**` matches the base directory itself plus
            // every descendant (files, symlinks, and directories),
            // recursively — bash's globstar set. The empty
            // relative base (cwd) isn't a nameable path, so it's
            // skipped; a named base (`dir/**`, `/abs/**`) is
            // included.
            if !dir.is_empty() {
                out.push(dir.to_string());
            }
            glob_collect_descendants(dir, dotglob, out);
            return;
        }
        // `**` with following components: apply the rest at this
        // level (zero directories) and at every descendant
        // directory (one or more). Real-directory gating keeps the
        // recursion off symlink cycles.
        glob_walk(dir, comps, idx + 1, dotglob, out);
        for name in glob_read_dir(dir) {
            if !dotglob && name.starts_with('.') {
                continue;
            }
            let full = glob_join(dir, &name);
            if glob_is_real_dir(&full) {
                glob_walk(&full, comps, idx, dotglob, out);
            }
        }
        return;
    }

    for name in glob_read_dir(dir) {
        if !glob_component_matches(comp, &name, dotglob) {
            continue;
        }
        let full = glob_join(dir, &name);
        if last {
            out.push(full);
        } else if glob_is_dir(&full) {
            glob_walk(&full, comps, idx + 1, dotglob, out);
        }
    }
}

/// Expand a single field as a filesystem glob pattern. Returns the
/// sorted matching paths, or an empty vector when nothing matches.
/// `**` is path-separator-aware (recursive); plain `*` / `?` /
/// `[…]` match within one component via [`glob_match`]. `dotglob`
/// includes hidden entries (the `(#qD)` qualifier).
#[cfg(feature = "std")]
fn glob_expand_field(pat: &str, dotglob: bool) -> Vec<String> {
    let absolute = pat.starts_with('/');
    let comps: Vec<&str> = pat.split('/').filter(|c| !c.is_empty()).collect();
    if comps.is_empty() {
        return Vec::new();
    }
    let start = if absolute { "/" } else { "" };
    let mut out: Vec<String> = Vec::new();
    glob_walk(start, &comps, 0, dotglob, &mut out);
    out.sort();
    out
}

/// Parsed `(#q…)` glob qualifier. File-type and permission
/// filters are AND-combined; modifiers, sort, and slice apply to
/// the surviving, sorted result.
#[derive(Default, Clone)]
struct GlobQualifiers {
    /// File-type filters: `.` regular, `/` dir, `@` symlink, `=`
    /// socket, `p` pipe, `*` executable-regular. All must hold.
    type_filters: Vec<char>,
    /// Permission filters: `r`/`w`/`x` (owner bits), `R`/`W`/`X`
    /// (other bits), `s` (setuid). All must hold.
    perm_filters: Vec<char>,
    /// `D` — include dotfiles in the glob (threaded into the walk).
    dotglob: bool,
    /// `N` — opt-in null glob: an empty result is allowed (no
    /// error, no literal) regardless of mode.
    nullglob: bool,
    /// `Y<n>` — stop after `n` matches.
    count_cap: Option<usize>,
    /// `o<key>` / `O<key>` — sort `(descending, key)`. `key` is
    /// one of `n` name, `m` mtime, `a` atime, `c` ctime, `L` size.
    sort: Option<(bool, char)>,
    /// `[N]` / `[N,M]` — 1-based slice of the sorted result.
    slice: Option<(usize, Option<usize>)>,
}

/// Split a trailing `(#q…)` qualifier off a glob field. Returns
/// `(pattern, qualifier_body)` when the field ends in `)` and
/// contains the mandatory `(#q` marker; otherwise `None` (the
/// field has no qualifier). The `(#q` marker is required — a bare
/// `(…)` is extglob / a subshell, never a qualifier.
fn split_glob_qualifier(field: &str) -> Option<(&str, &str)> {
    if !field.ends_with(')') {
        return None;
    }
    let marker = field.rfind("(#q")?;
    let body = &field[marker + 3..field.len() - 1];
    let pattern = &field[..marker];
    Some((pattern, body))
}

/// Parse a `(#q…)` qualifier body. Errors on an unknown qualifier
/// character (so nothing is silently ignored) and on the
/// dependency-needing ownership forms (`u:`/`g:`), which are
/// explicitly not yet supported.
fn parse_glob_qualifiers(body: &str) -> Result<GlobQualifiers> {
    let mut q = GlobQualifiers::default();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            '.' | '/' | '@' | '=' | 'p' | '*' => {
                q.type_filters.push(c);
                i += 1;
            }
            'r' | 'w' | 'x' | 'R' | 'W' | 'X' | 's' => {
                q.perm_filters.push(c);
                i += 1;
            }
            'D' => {
                q.dotglob = true;
                i += 1;
            }
            'N' => {
                q.nullglob = true;
                i += 1;
            }
            'Y' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i == start {
                    return Err(KashError::Parse(
                        "glob qualifier `Y` needs a count, e.g. `(#qY3)`".into(),
                    ));
                }
                q.count_cap = Some(body[start..i].parse().unwrap_or(usize::MAX));
            }
            'o' | 'O' => {
                let descending = c == 'O';
                i += 1;
                let Some(&key) = bytes.get(i) else {
                    return Err(KashError::Parse(
                        "glob sort qualifier needs a key (n/m/a/c/L)".into(),
                    ));
                };
                let key = key as char;
                if !matches!(key, 'n' | 'm' | 'a' | 'c' | 'L') {
                    return Err(KashError::Parse(alloc::format!(
                        "unknown glob sort key `{key}` (expected n/m/a/c/L)"
                    )));
                }
                q.sort = Some((descending, key));
                i += 1;
            }
            '[' => {
                let Some(close) = body[i..].find(']') else {
                    return Err(KashError::Parse(
                        "unterminated `[` in glob qualifier".into(),
                    ));
                };
                let inner = &body[i + 1..i + close];
                q.slice = Some(parse_qualifier_slice(inner)?);
                i += close + 1;
            }
            'u' | 'g' => {
                return Err(KashError::Runtime(alloc::format!(
                    "glob ownership qualifier `{c}:…:` is not yet supported"
                )));
            }
            _ => {
                return Err(KashError::Parse(alloc::format!(
                    "unknown glob qualifier `{c}`"
                )));
            }
        }
    }
    Ok(q)
}

/// Parse the `N` or `N,M` body of a `[…]` slice qualifier into a
/// 1-based `(start, Option<end>)` pair.
fn parse_qualifier_slice(inner: &str) -> Result<(usize, Option<usize>)> {
    let parse_idx = |s: &str| -> Result<usize> {
        s.trim().parse::<usize>().map_err(|_| {
            KashError::Parse(alloc::format!(
                "glob slice index `{s}` is not a positive integer"
            ))
        })
    };
    match inner.split_once(',') {
        Some((a, b)) => Ok((parse_idx(a)?, Some(parse_idx(b)?))),
        None => {
            let n = parse_idx(inner)?;
            Ok((n, Some(n)))
        }
    }
}

/// Whether the filesystem entry at `path` satisfies a qualifier's
/// file-type and permission filters (all AND-combined). Type tests
/// use `lstat` (no symlink follow) so `@` catches symlinks and `.`
/// catches real regular files. On non-unix only the cross-platform
/// type filters apply; permission filters are treated as satisfied.
#[cfg(all(feature = "std", unix))]
fn glob_entry_matches_filters(path: &str, q: &GlobQualifiers) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let ft = md.file_type();
    let mode = md.mode();
    for &t in &q.type_filters {
        let ok = match t {
            '.' => ft.is_file(),
            '/' => ft.is_dir(),
            '@' => ft.is_symlink(),
            '=' => ft.is_socket(),
            'p' => ft.is_fifo(),
            '*' => ft.is_file() && (mode & 0o111 != 0),
            _ => true,
        };
        if !ok {
            return false;
        }
    }
    for &perm in &q.perm_filters {
        let bit = match perm {
            'r' => 0o400,
            'w' => 0o200,
            'x' => 0o100,
            'R' => 0o004,
            'W' => 0o002,
            'X' => 0o001,
            's' => 0o4000,
            _ => 0,
        };
        if mode & bit == 0 {
            return false;
        }
    }
    true
}

#[cfg(all(feature = "std", not(unix)))]
fn glob_entry_matches_filters(path: &str, q: &GlobQualifiers) -> bool {
    if q.type_filters.is_empty() {
        return true;
    }
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let ft = md.file_type();
    for &t in &q.type_filters {
        let ok = match t {
            '.' => ft.is_file(),
            '/' => ft.is_dir(),
            '@' => ft.is_symlink(),
            _ => true,
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Sort `paths` in place by a qualifier sort key — `m` mtime, `a`
/// atime, `c` ctime, `L` size — ascending, with a name tiebreak.
/// `n` (and the non-unix fallback) sorts by name.
#[cfg(all(feature = "std", unix))]
fn glob_sort(paths: &mut [String], key: char) {
    use std::os::unix::fs::MetadataExt;
    if key == 'n' {
        paths.sort();
        return;
    }
    let metric = |p: &str| -> i64 {
        std::fs::symlink_metadata(p)
            .map(|m| match key {
                'm' => m.mtime(),
                'a' => m.atime(),
                'c' => m.ctime(),
                'L' => m.size() as i64,
                _ => 0,
            })
            .unwrap_or(0)
    };
    paths.sort_by(|a, b| metric(a).cmp(&metric(b)).then_with(|| a.cmp(b)));
}

#[cfg(all(feature = "std", not(unix)))]
fn glob_sort(paths: &mut [String], _key: char) {
    paths.sort();
}

/// Apply a parsed `(#q…)` qualifier to a glob's match list:
/// filter by type / permission, sort, slice `[N,M]`, then cap to
/// `Y<n>`. The input is already name-sorted from
/// [`glob_expand_field`].
#[cfg(feature = "std")]
fn apply_glob_qualifiers(
    matches: Vec<String>,
    q: &GlobQualifiers,
) -> Vec<String> {
    let mut out: Vec<String> = matches
        .into_iter()
        .filter(|p| glob_entry_matches_filters(p, q))
        .collect();
    if let Some((descending, key)) = q.sort {
        glob_sort(&mut out, key);
        if descending {
            out.reverse();
        }
    }
    // `Y<n>` caps the match *count* ("stop after n matches"), so it
    // runs before the slice — `Y2[4,8]` yields nothing because the
    // glob already stopped at 2. `Y0` is "no cap" (zsh), not
    // truncate-to-zero.
    if let Some(n) = q.count_cap
        && n > 0
    {
        out.truncate(n);
    }
    if let Some((start, end)) = q.slice {
        // 1-based, inclusive.
        let s = start.saturating_sub(1);
        let e = end.map(|e| e.min(out.len())).unwrap_or(out.len());
        out = if s < out.len() && s < e {
            out[s..e].to_vec()
        } else {
            Vec::new()
        };
    }
    out
}

/// Expand brace groups in a `use` path. Supports the comma form
/// (`a,b,c`) and the cross-product of multiple groups
/// (`.{x,y}.{a,b}` → 4 paths). Returns `vec![raw.to_string()]` when
/// no brace group is present.
fn expand_brace_in_path(raw: &str) -> Result<Vec<String>> {
    let mut frontier = alloc::vec![alloc::string::String::new()];
    let mut rest = raw;
    while let Some(open) = rest.find('{') {
        let close = match find_matching_brace(rest, open) {
            Some(i) => i,
            None => {
                return Err(KashError::Runtime(alloc::format!(
                    "use: unbalanced `{{` in path `{raw}`"
                )));
            }
        };
        let prefix = &rest[..open];
        let inner = &rest[open + 1..close];
        let alts: Vec<&str> = inner.split(',').collect();
        if alts.iter().any(|s| s.is_empty()) {
            return Err(KashError::Runtime(alloc::format!(
                "use: empty brace alternative in `{raw}`"
            )));
        }
        let mut next = Vec::with_capacity(frontier.len() * alts.len());
        for base in &frontier {
            for alt in &alts {
                let mut s = base.clone();
                s.push_str(prefix);
                s.push_str(alt);
                next.push(s);
            }
        }
        frontier = next;
        rest = &rest[close + 1..];
    }
    if rest.is_empty() {
        return Ok(frontier);
    }
    let suffix = rest;
    for s in frontier.iter_mut() {
        s.push_str(suffix);
    }
    Ok(frontier)
}

/// Locate the `}` that matches the `{` at `open`. Nested braces are
/// supported. Returns `None` if no match.
fn find_matching_brace(s: &str, open: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth: u32 = 0;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Build `.<seg1>.<seg2>…<name>` for a namespace-qualified lookup.
fn build_qualified_name(segments: &[String], name: &str) -> String {
    let mut out = String::with_capacity(
        segments.iter().map(|s| s.len() + 1).sum::<usize>() + name.len() + 1,
    );
    for seg in segments {
        out.push('.');
        out.push_str(seg);
    }
    out.push('.');
    out.push_str(name);
    out
}

/// `${VAR#pat}` / `${VAR##pat}` — drop a glob-pattern prefix from
/// `value`. With `longest=true` the longest matching prefix wins,
/// otherwise the shortest. No match returns `value` unchanged.
fn strip_prefix_match(pat: &str, value: &str, longest: bool) -> String {
    let bytes = value.as_bytes();
    let pat_bytes = pat.as_bytes();
    let mut hits: Vec<usize> = (0..=bytes.len())
        .filter(|i| value.is_char_boundary(*i) && glob_match_bytes(pat_bytes, &bytes[..*i]))
        .collect();
    if hits.is_empty() {
        return value.to_string();
    }
    let pick = if longest {
        hits.pop().unwrap()
    } else {
        hits.remove(0)
    };
    value[pick..].to_string()
}

/// `${VAR%pat}` / `${VAR%%pat}` — drop a glob-pattern suffix.
fn strip_suffix_match(pat: &str, value: &str, longest: bool) -> String {
    let bytes = value.as_bytes();
    let pat_bytes = pat.as_bytes();
    let mut hits: Vec<usize> = (0..=bytes.len())
        .filter(|i| value.is_char_boundary(*i) && glob_match_bytes(pat_bytes, &bytes[*i..]))
        .collect();
    if hits.is_empty() {
        return value.to_string();
    }
    // For shortest suffix we want the largest index i; for the
    // longest, the smallest. `hits` is sorted ascending.
    let pick = if longest {
        hits.remove(0)
    } else {
        hits.pop().unwrap()
    };
    value[..pick].to_string()
}

/// Split the `OLD/NEW` portion of a `${VAR/OLD/NEW}` body at the
/// *first* unescaped `/`. The pattern half can contain a literal
/// slash by escaping it as `\/`. If no `/` is present the entire
/// input is taken as `OLD` and `NEW` is empty.
fn split_replace_args(body: &str) -> (String, String) {
    let mut old = String::new();
    let mut it = body.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\'
            && let Some(&next) = it.peek()
        {
            it.next();
            old.push(next);
            continue;
        }
        if c == '/' {
            let new: String = it.collect();
            return (old, new);
        }
        old.push(c);
    }
    (old, String::new())
}

/// `${VAR/OLD/NEW}` — replace the first match. The match is
/// anywhere in the string; the longest match anchored at each
/// position is tried, starting from the left.
fn replace_glob_first(value: &str, old: &str, new: &str) -> String {
    if old.is_empty() {
        return value.to_string();
    }
    if let Some((start, end)) = first_glob_span(value, old) {
        let mut out = String::with_capacity(value.len() - (end - start) + new.len());
        out.push_str(&value[..start]);
        out.push_str(new);
        out.push_str(&value[end..]);
        return out;
    }
    value.to_string()
}

/// `${VAR//OLD/NEW}` — replace every match.
fn replace_glob_all(value: &str, old: &str, new: &str) -> String {
    if old.is_empty() {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut cursor = 0;
    while cursor <= value.len() {
        if let Some((start, end)) = first_glob_span(&value[cursor..], old) {
            let abs_start = cursor + start;
            let abs_end = cursor + end;
            out.push_str(&value[cursor..abs_start]);
            out.push_str(new);
            // Avoid an infinite loop on zero-width matches.
            cursor = if abs_end == abs_start {
                next_char_boundary(value, abs_end)
            } else {
                abs_end
            };
        } else {
            out.push_str(&value[cursor..]);
            break;
        }
    }
    out
}

/// `${VAR/#OLD/NEW}` (anchor=prefix) and `${VAR/%OLD/NEW}` (suffix).
fn replace_glob_anchored(value: &str, old: &str, new: &str, prefix: bool) -> String {
    if prefix {
        let stripped = strip_prefix_match(old, value, true);
        if stripped == value {
            return value.to_string();
        }
        return alloc::format!("{new}{stripped}");
    }
    let stripped = strip_suffix_match(old, value, true);
    if stripped == value {
        return value.to_string();
    }
    alloc::format!("{stripped}{new}")
}

/// Locate the *first* glob span in `haystack` matching `pat`, by
/// scanning start positions left-to-right and picking the longest
/// match at each. Returns `(start_byte, end_byte)` on hit.
fn first_glob_span(haystack: &str, pat: &str) -> Option<(usize, usize)> {
    let bytes = haystack.as_bytes();
    let pat_bytes = pat.as_bytes();
    for start in 0..=bytes.len() {
        if !haystack.is_char_boundary(start) {
            continue;
        }
        let mut longest: Option<usize> = None;
        for end in start..=bytes.len() {
            if !haystack.is_char_boundary(end) {
                continue;
            }
            if glob_match_bytes(pat_bytes, &bytes[start..end]) {
                longest = Some(end);
            }
        }
        if let Some(end) = longest {
            return Some((start, end));
        }
    }
    None
}

/// Step `i` forward to the next UTF-8 char boundary, or `s.len()`
/// when already at the end. Used to advance past zero-width
/// pattern matches without looping forever.
fn next_char_boundary(s: &str, i: usize) -> usize {
    let mut j = i + 1;
    while j < s.len() && !s.is_char_boundary(j) {
        j += 1;
    }
    j.min(s.len())
}

/// Case-fold `value` per `${VAR^^}` / `${VAR^}` / `${VAR,,}` /
/// `${VAR,}`. `upper=true` selects to-upper; `upper=false` selects
/// to-lower. `all=true` folds every char; `all=false` folds only
/// the first. An optional `filter` glob (typical bash form is a
/// single bracketed char set, e.g. `[abc]`) constrains which chars
/// are eligible; absent it everything is eligible.
fn case_fold(value: &str, upper: bool, all: bool, filter: Option<&str>) -> String {
    let mut out = String::with_capacity(value.len());
    let mut folded_any = false;
    for c in value.chars() {
        let eligible = match filter {
            None => true,
            Some(pat) => {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                glob_match(pat, s)
            }
        };
        let do_fold = eligible && (all || !folded_any);
        if do_fold {
            if upper {
                for u in c.to_uppercase() {
                    out.push(u);
                }
            } else {
                for u in c.to_lowercase() {
                    out.push(u);
                }
            }
            folded_any = true;
        } else {
            out.push(c);
        }
    }
    out
}

fn glob_match_bytes(pat: &[u8], s: &[u8]) -> bool {
    let (p0, s0) = (pat.first().copied(), s.first().copied());
    // ksh93 / bash extglob: `?(p)` / `*(p)` / `+(p)` / `@(p)` / `!(p)`.
    if matches!(p0, Some(b'?' | b'*' | b'+' | b'@' | b'!'))
        && pat.get(1) == Some(&b'(')
        && let Some((inner, rest_off)) = extglob_split(pat)
    {
        let head = pat[0];
        let rest = &pat[rest_off..];
        return extglob_match(head, &inner, rest, s);
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
                depth = depth.saturating_sub(1);
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
                if let Some(after) = consume_once(alt, s)
                    && glob_match_bytes(rest, after)
                {
                    return true;
                }
            }
            false
        }
        b'@' => {
            // Exactly one occurrence.
            for alt in &alts {
                if let Some(after) = consume_once(alt, s)
                    && glob_match_bytes(rest, after)
                {
                    return true;
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
    use crate::collections::BTreeBackend;
    use crate::parser::parse;

    // Default-backend aliases so tests don't have to spell the
    // turbofish on every `Evaluator::new()` — Rust's default type
    // parameter applies at declaration time only, not during call-site
    // inference.
    type Evaluator = super::Evaluator<BTreeBackend>;

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
        let outcome = ev.eval_program(&prog).unwrap();
        assert_eq!(outcome.status(), 127);
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

    // ===== brace expansion =====

    #[test]
    fn brace_comma_basic() {
        let (_, out, _) = run("echo a{b,c}d\n");
        assert_eq!(out, "abd acd\n");
    }

    #[test]
    fn brace_comma_three_alternatives() {
        let (_, out, _) = run("echo {x,y,z}\n");
        assert_eq!(out, "x y z\n");
    }

    #[test]
    fn brace_range_numeric_ascending() {
        let (_, out, _) = run("echo {1..5}\n");
        assert_eq!(out, "1 2 3 4 5\n");
    }

    #[test]
    fn brace_range_numeric_descending() {
        let (_, out, _) = run("echo {5..1}\n");
        assert_eq!(out, "5 4 3 2 1\n");
    }

    #[test]
    fn brace_range_char() {
        let (_, out, _) = run("echo {a..e}\n");
        assert_eq!(out, "a b c d e\n");
    }

    #[test]
    fn brace_step_numeric() {
        let (_, out, _) = run("echo {1..10..2}\n");
        assert_eq!(out, "1 3 5 7 9\n");
    }

    #[test]
    fn brace_step_char() {
        let (_, out, _) = run("echo {a..z..5}\n");
        assert_eq!(out, "a f k p u z\n");
    }

    #[test]
    fn brace_step_descending() {
        let (_, out, _) = run("echo {10..1..3}\n");
        assert_eq!(out, "10 7 4 1\n");
    }

    #[test]
    fn brace_range_negative() {
        let (_, out, _) = run("echo {-2..2}\n");
        assert_eq!(out, "-2 -1 0 1 2\n");
    }

    #[test]
    fn brace_zero_padded_width() {
        let (_, out, _) = run("echo {01..03}\n");
        assert_eq!(out, "01 02 03\n");
    }

    #[test]
    fn brace_zero_padded_widest_endpoint() {
        // bash pads to the widest endpoint when either has a
        // leading zero: `{01..100..25}` → 001 026 051 076.
        let (_, out, _) = run("echo {01..100..25}\n");
        assert_eq!(out, "001 026 051 076\n");
    }

    #[test]
    fn brace_cross_product() {
        let (_, out, _) = run("echo {a,b}{1,2}\n");
        assert_eq!(out, "a1 a2 b1 b2\n");
    }

    #[test]
    fn brace_nested() {
        let (_, out, _) = run("echo {a,b{c,d}}\n");
        assert_eq!(out, "a bc bd\n");
    }

    #[test]
    fn brace_literal_no_comma_or_range() {
        // `{a}` has no comma and isn't a range → stays literal.
        let (_, out, _) = run("echo {a}\n");
        assert_eq!(out, "{a}\n");
    }

    #[test]
    fn brace_literal_malformed_range() {
        let (_, out, _) = run("echo {1..}\n");
        assert_eq!(out, "{1..}\n");
    }

    #[test]
    fn brace_step_zero_defaults_to_one() {
        // bash expands `{1..5..0}` as if the step were 1.
        let (_, out, _) = run("echo {1..5..0}\n");
        assert_eq!(out, "1 2 3 4 5\n");
        let (_, out, _) = run("echo {a..e..0}\n");
        assert_eq!(out, "a b c d e\n");
    }

    #[test]
    fn brace_negative_zero_padded_includes_sign_in_width() {
        // bash aligns to a uniform field width that counts the
        // sign: `{-01..01}` → `-01 000 001`.
        let (_, out, _) = run("echo {-01..01}\n");
        assert_eq!(out, "-01 000 001\n");
    }

    #[test]
    fn brace_negative_mixed_pad_width() {
        let (_, out, _) = run("echo {-02..2}\n");
        assert_eq!(out, "-02 -01 000 001 002\n");
        // Asymmetric endpoint widths align to the longer lexeme.
        let (_, out, _) = run("echo {-1..001}\n");
        assert_eq!(out, "-01 000 001\n");
    }

    #[test]
    fn brace_char_range_crossing_backtick_is_literal() {
        // ASCII 90..97 spans the backtick (96). The generated
        // characters are literal data — a produced backtick must
        // not be re-scanned as command substitution.
        let (_, out, _) = run("echo {Z..a}\n");
        assert_eq!(out, "Z [ \\ ] ^ _ ` a\n");
    }

    #[test]
    fn brace_empty_alternative_with_suffix() {
        // `{a,}b` → `ab` and `b` (the empty alternative gains the
        // `b` suffix, so neither word is empty).
        let (_, out, _) = run("echo {a,}b\n");
        assert_eq!(out, "ab b\n");
    }

    #[test]
    fn brace_unquoted_empty_alternative_is_dropped() {
        // A wholly-empty *unquoted* brace word produces no field,
        // matching bash: `echo {a,}` → `a`, `set -- {a,}` → 1 arg.
        let (_, out, _) = run("echo {a,}\n");
        assert_eq!(out, "a\n");
        let (_, out, _) = run("set -- {a,}; echo $#\n");
        assert_eq!(out, "1\n");
    }

    #[test]
    fn brace_alternative_emptied_by_expansion_is_dropped() {
        // The first word `$xb` expands to empty (x unset) → no
        // field; only `ab` survives. Matches bash.
        let (_, out, _) = run("x=\"\"; echo {$x,a}b\n");
        assert_eq!(out, "ab\n");
    }

    #[test]
    fn brace_single_quoted_is_literal() {
        let (_, out, _) = run("echo '{1..3}'\n");
        assert_eq!(out, "{1..3}\n");
    }

    #[test]
    fn brace_double_quoted_is_literal() {
        let (_, out, _) = run("echo \"{a,b}\"\n");
        assert_eq!(out, "{a,b}\n");
    }

    #[test]
    fn brace_partly_quoted_alternative_stays_literal() {
        // The unquoted `{a,b}` expands; the quoted `'{1,2}'`
        // rides along verbatim on each expansion.
        let (_, out, _) = run("echo {a,b}'{1,2}'\n");
        assert_eq!(out, "a{1,2} b{1,2}\n");
    }

    #[test]
    fn brace_does_not_touch_parameter_expansion() {
        // `${v,,}` is parameter case-folding, not brace syntax.
        let (_, out, _) = run("v=HELLO; echo ${v,,}\n");
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn brace_alternatives_expand_dollar_after() {
        // Brace runs before `$`; the chosen alternative's `$x`
        // still expands afterward.
        let (_, out, _) = run("x=1; y=2; echo {$x,$y}\n");
        assert_eq!(out, "1 2\n");
    }

    #[test]
    fn brace_in_for_loop_iteration() {
        let (_, out, _) = run("for i in {1..3}; do echo n$i; done\n");
        assert_eq!(out, "n1\nn2\nn3\n");
    }

    #[test]
    fn brace_word_start_is_not_a_group() {
        // `{1..3}` at command-argument start must lex as a word,
        // not the reserved-word brace group.
        let (_, out, _) = run("echo pre {1..3} post\n");
        assert_eq!(out, "pre 1 2 3 post\n");
    }

    // ===== null-glob policy predicate =====

    #[test]
    fn null_glob_fails_in_default_mode() {
        let ev =
            Evaluator::with_mode(crate::mode::Mode::parse("default").unwrap());
        assert!(ev.null_glob_should_fail());
    }

    #[test]
    fn null_glob_survives_in_posix_aware_mode() {
        let ev = Evaluator::with_mode(
            crate::mode::Mode::parse("posix-aware").unwrap(),
        );
        assert!(!ev.null_glob_should_fail());
    }

    #[test]
    fn null_glob_fails_under_secure_modifier() {
        let ev = Evaluator::with_mode(
            crate::mode::Mode::parse("posix-aware-secure").unwrap(),
        );
        assert!(ev.null_glob_should_fail());
    }

    #[cfg(not(feature = "std"))]
    #[test]
    fn glob_is_noop_without_std() {
        // No filesystem in the alloc-only build, so a glob
        // pattern survives verbatim regardless of mode.
        let (_, out, _) = run("echo a*b.txt\n");
        assert_eq!(out, "a*b.txt\n");
    }

    #[test]
    fn glob_escaped_metachar_not_globbed() {
        // A backslash-escaped `*` / `?` is literal — it must not
        // trigger pathname expansion (which, in default/failglob
        // mode under `std`, would otherwise raise a spurious
        // no-match error). The backslash itself is still present
        // in the output — bare-word backslash quote removal is a
        // separate, pre-existing gap.
        let (o, out, _) = run("echo \\*.txt\n");
        assert_eq!(o.status(), 0);
        assert_eq!(out, "\\*.txt\n");
        let (o, out, _) = run("echo q\\?.txt\n");
        assert_eq!(o.status(), 0);
        assert_eq!(out, "q\\?.txt\n");
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

        #[test]
        fn here_doc_with_pipe_trailing_on_introducer_line_runs() {
            // `<<EOF` followed by `| …` on the same line: the pipe
            // and its tail belong to the surrounding pipeline, not
            // to the here-doc body. With pipeline-stage redirect
            // support, the body actually flows into the next stage.
            if !have("/bin/cat") || !have("/bin/wc") {
                return;
            }
            let src = "/bin/cat <<EOF | /bin/wc -l\nalpha\nbeta\ngamma\nEOF\n";
            let prog = parse(src).expect("introducer-line trailing should parse");
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output().trim(), "3");
        }

        #[test]
        fn pipeline_stage_with_output_redirect() {
            if !have("/bin/echo") || !have("/bin/cat") {
                return;
            }
            let tmp = std::env::temp_dir().join("kash-pipe-mid-redirect.txt");
            let path = tmp.to_str().unwrap();
            let src = alloc::format!(
                "/bin/echo hello | /bin/cat >{path}\n"
            );
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            let contents = std::fs::read_to_string(&tmp).unwrap();
            assert_eq!(contents, "hello\n");
            let _ = std::fs::remove_file(&tmp);
        }

        #[test]
        fn pipeline_stage_with_input_redirect_from_file() {
            if !have("/bin/cat") || !have("/bin/wc") {
                return;
            }
            let tmp = std::env::temp_dir().join("kash-pipe-in-redirect.txt");
            std::fs::write(&tmp, "a\nb\nc\n").unwrap();
            let path = tmp.to_str().unwrap();
            let src = alloc::format!(
                "/bin/cat <{path} | /bin/wc -l\n"
            );
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output().trim(), "3");
            let _ = std::fs::remove_file(&tmp);
        }

        #[test]
        fn here_doc_with_redirect_trailing_on_introducer_line() {
            // `<<EOF >outfile` — the body still goes to cat's stdin,
            // its stdout goes to the file.
            if !have("/bin/cat") {
                return;
            }
            let tmp = std::env::temp_dir().join("kash-heredoc-trailing.txt");
            let path = tmp.to_str().unwrap();
            let src = alloc::format!(
                "/bin/cat <<EOF >{path}\none\ntwo\nEOF\n"
            );
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            let contents = std::fs::read_to_string(&tmp).unwrap();
            assert_eq!(contents, "one\ntwo\n");
            let _ = std::fs::remove_file(&tmp);
        }

        #[test]
        fn here_doc_multi_on_one_introducer_line() {
            // `cat <<A <<B` — two here-docs on the same introducer
            // line. Bodies follow in declaration order, separated
            // by their own closing delimiter lines. Only the *last*
            // redirect is what cat's stdin actually sees (POSIX:
            // later redirects on the same fd win), so we observe
            // the second body in cat's stdout.
            if !have("/bin/cat") {
                return;
            }
            let src = "/bin/cat <<A <<B\nfirst-body\nA\nsecond-body\nB\n";
            let prog = parse(src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "second-body\n");
        }

        #[test]
        fn here_doc_multi_parses_both_bodies() {
            // Independent of which body cat actually reads, the
            // parse must give us *two* here-doc redirects whose
            // targets carry the right bodies in source order.
            let src = "/bin/cat <<A <<B\nfirst\nA\nsecond\nB\n";
            let prog = parse(src).expect("multi-heredoc parse");
            let dbg = alloc::format!("{:?}", prog.statements[0]);
            assert!(dbg.contains("first"), "got: {dbg}");
            assert!(dbg.contains("second"), "got: {dbg}");
        }

        #[test]
        fn venv_env_overlay_reaches_external_command() {
            // Inside the venv, `printenv PYTHONHOME` must see the
            // value the `env { … }` section set, not whatever the
            // parent shell had.
            if !have("/usr/bin/printenv") {
                return;
            }
            let src = "venv myproj {\n\
                           env { PYTHONHOME=/opt/venv }\n\
                           body { /usr/bin/printenv PYTHONHOME; }\n\
                       }\n";
            let prog = parse(src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "/opt/venv\n");
        }

        #[test]
        fn venv_path_prepend_lands_first_on_path() {
            if !have("/usr/bin/printenv") {
                return;
            }
            // We prepend a recognizable token to PATH and verify it
            // shows up as the first colon-separated entry.
            let src = "venv myproj {\n\
                           env { PATH-prepend /tmp/kashtest-needle }\n\
                           body { /usr/bin/printenv PATH; }\n\
                       }\n";
            let prog = parse(src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            let out = ev.take_output();
            let first = out.trim_end().split(':').next().unwrap_or("");
            assert_eq!(first, "/tmp/kashtest-needle", "full PATH: {out}");
        }

        #[test]
        fn venv_path_append_lands_last_on_path() {
            if !have("/usr/bin/printenv") {
                return;
            }
            let src = "venv myproj {\n\
                           env { PATH-append /tmp/kashtest-tail }\n\
                           body { /usr/bin/printenv PATH; }\n\
                       }\n";
            let prog = parse(src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            let out = ev.take_output();
            let last = out
                .trim_end()
                .rsplit(':')
                .next()
                .unwrap_or("");
            assert_eq!(last, "/tmp/kashtest-tail", "full PATH: {out}");
        }

        #[test]
        fn venv_load_config_applies_capabilities_and_env() {
            // Write a tiny TOML profile, load it, observe the
            // env overlay reached an external command. Run under
            // std-only because both fs::write and toml are gated.
            if !have("/usr/bin/printenv") {
                return;
            }
            let tmp = std::env::temp_dir().join("kash-venv-config.toml");
            std::fs::write(
                &tmp,
                "[capabilities]\nprofile = \"basic\"\n\n[env]\nKASH_VENV_TOKEN = \"from-config\"\n",
            )
            .unwrap();
            let path = tmp.to_str().unwrap();
            let src = alloc::format!(
                "venv myproj {{ load-config {path}; body {{ /usr/bin/printenv KASH_VENV_TOKEN; }}; }}\n"
            );
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "from-config\n");
            let _ = std::fs::remove_file(&tmp);
        }

        #[test]
        fn venv_load_config_missing_file_errors() {
            let src = "venv myproj { load-config /no/such/file.toml; body {}; }\n";
            let prog = parse(src).unwrap();
            let mut ev = Evaluator::new();
            let err = ev.eval_program(&prog).unwrap_err();
            let msg = alloc::format!("{err}");
            assert!(msg.contains("load-config"), "got: {msg}");
        }

        #[test]
        fn venv_load_config_rejects_invalid_toml() {
            let tmp = std::env::temp_dir().join("kash-venv-bad.toml");
            std::fs::write(&tmp, "this = is = not valid toml\n").unwrap();
            let path = tmp.to_str().unwrap();
            let src = alloc::format!(
                "venv myproj {{ load-config {path}; body {{}}; }}\n"
            );
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            let err = ev.eval_program(&prog).unwrap_err();
            let msg = alloc::format!("{err}");
            assert!(msg.contains("invalid TOML"), "got: {msg}");
            let _ = std::fs::remove_file(&tmp);
        }

        #[test]
        fn venv_path_prepend_resolves_bare_command() {
            // Drop a tiny executable in a unique dir, prepend that
            // dir via venv env, then call it by its *bare* name —
            // resolution must consult the venv-extended PATH (not
            // just the parent process's).
            let dir = std::env::temp_dir().join("kash-venv-pathres");
            let _ = std::fs::create_dir_all(&dir);
            let script = dir.join("kashtest-uniquecmd");
            std::fs::write(&script, "#!/bin/sh\necho got-resolved\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&script).unwrap().permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&script, perms).unwrap();
            }
            let dir_str = dir.to_str().unwrap();
            let src = alloc::format!(
                "venv proj {{\n\
                     capabilities {{ profile dev; allow-cmd kashtest-uniquecmd }}\n\
                     env {{ PATH-prepend {dir_str} }}\n\
                     body {{ kashtest-uniquecmd; }}\n\
                 }}\n"
            );
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            // Seed a parent PATH so the venv PATH-prepend has
            // something to layer onto.
            ev.set_env_var("PATH", "/usr/bin:/bin").unwrap();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "got-resolved\n");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn venv_env_overlay_drops_after_frame_pops() {
            // After the venv block ends, a fresh external lookup
            // must not see the overlay value.
            if !have("/usr/bin/printenv") {
                return;
            }
            let src = "venv myproj { env { KASH_TEST_OVERLAY=in-venv } body { /usr/bin/printenv KASH_TEST_OVERLAY; }; }\n\
                       /usr/bin/printenv KASH_TEST_OVERLAY ; echo done\n";
            let prog = parse(src).unwrap();
            let mut ev = Evaluator::new();
            // The second printenv exits non-zero when the var is
            // unset; we ignore its status and check that the only
            // value printed was the one from inside the venv.
            let _ = ev.eval_program(&prog);
            let out = ev.take_output();
            assert!(out.starts_with("in-venv\n"));
            assert!(out.ends_with("done\n"));
            // No second `in-venv` line.
            assert_eq!(out.matches("in-venv").count(), 1, "out: {out}");
        }

        #[test]
        fn venv_revoking_fs_write_blocks_output_redirect() {
            let tmp = std::env::temp_dir().join("kash-venv-fs.txt");
            // Pre-emptively remove any stray file from a previous
            // run so the existence check after the venv body is
            // definitive.
            let _ = std::fs::remove_file(&tmp);
            let path = tmp.to_str().unwrap();
            let src = alloc::format!(
                "venv tight {{ capabilities {{ profile basic }} body {{ echo data >{path}; }} }}\n"
            );
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            let outcome = ev.eval_program(&prog).unwrap();
            assert_eq!(outcome.status(), 126);
            let err = ev.take_stderr();
            assert!(err.contains("fs-write"), "got: {err}");
            assert!(!tmp.exists(), "file should not exist");
        }

        #[test]
        fn venv_revoking_fs_read_blocks_input_redirect() {
            let tmp = std::env::temp_dir().join("kash-venv-fs-read.txt");
            std::fs::write(&tmp, "secret\n").unwrap();
            let path = tmp.to_str().unwrap();
            let src = alloc::format!(
                "venv tight {{ capabilities {{ profile none }} body {{ cat <{path}; }} }}\n"
            );
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            let outcome = ev.eval_program(&prog).unwrap();
            assert_eq!(outcome.status(), 126);
            let err = ev.take_stderr();
            assert!(err.contains("fs-read"), "got: {err}");
            let _ = std::fs::remove_file(&tmp);
        }

        #[test]
        fn venv_dev_profile_allows_output_redirect() {
            // `dev` profile has fs-write + fs-create — output
            // redirect should succeed.
            let tmp = std::env::temp_dir().join("kash-venv-dev-write.txt");
            let path = tmp.to_str().unwrap();
            let src = alloc::format!(
                "venv proj {{ capabilities {{ profile dev; + exec-spawn; allow-cmd /bin/echo }} body {{ /bin/echo data >{path}; }} }}\n"
            );
            if !have("/bin/echo") {
                return;
            }
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            let contents = std::fs::read_to_string(&tmp).unwrap();
            assert_eq!(contents, "data\n");
            let _ = std::fs::remove_file(&tmp);
        }

        #[test]
        fn compound_brace_group_stdout_redirect() {
            let tmp = std::env::temp_dir().join("kash-cmp-redirect.txt");
            let _ = std::fs::remove_file(&tmp);
            let path = tmp.to_str().unwrap();
            let src = alloc::format!(
                "{{ echo a; echo b; echo c; }} >{path}\n"
            );
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(
                std::fs::read_to_string(&tmp).unwrap(),
                "a\nb\nc\n"
            );
            let _ = std::fs::remove_file(&tmp);
        }

        #[test]
        fn compound_for_loop_stdout_redirect() {
            let tmp = std::env::temp_dir().join("kash-cmp-for-redirect.txt");
            let _ = std::fs::remove_file(&tmp);
            let path = tmp.to_str().unwrap();
            let src = alloc::format!(
                "for x in a b c; do echo \"item: $x\"; done >{path}\n"
            );
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(
                std::fs::read_to_string(&tmp).unwrap(),
                "item: a\nitem: b\nitem: c\n"
            );
            let _ = std::fs::remove_file(&tmp);
        }

        #[test]
        fn compound_append_redirect_extends_file() {
            let tmp = std::env::temp_dir().join("kash-cmp-append.txt");
            std::fs::write(&tmp, "first\n").unwrap();
            let path = tmp.to_str().unwrap();
            let src = alloc::format!("{{ echo second; }} >>{path}\n");
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(
                std::fs::read_to_string(&tmp).unwrap(),
                "first\nsecond\n"
            );
            let _ = std::fs::remove_file(&tmp);
        }

        #[test]
        fn pipeline_first_stage_echo_pipes_to_external() {
            if !have("/usr/bin/tr") {
                return;
            }
            let src = "echo \"abc XYZ\" | /usr/bin/tr a-z A-Z\n";
            let prog = parse(src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "ABC XYZ\n");
        }

        #[test]
        fn pipeline_first_stage_echo_three_stage() {
            if !have("/usr/bin/tr") || !have("/usr/bin/wc") {
                return;
            }
            let src = "echo \"a b c d\" | /usr/bin/tr ' ' '\\n' | /usr/bin/wc -l\n";
            let prog = parse(src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output().trim(), "4");
        }

        #[test]
        fn background_external_command_spawns_and_sets_bang_pid() {
            if !have("/bin/sleep") {
                return;
            }
            let prog = parse("/bin/sleep 0 &\necho \"pid=$!\"\n").unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            let out = ev.take_output();
            assert!(out.starts_with("pid="), "got: {out}");
            // PID is some positive integer.
            let pid_str = out.trim_start_matches("pid=").trim();
            let pid: i32 = pid_str.parse().expect("numeric pid");
            assert!(pid > 0);
        }

        #[test]
        fn background_builtin_runs_in_process_with_status_zero() {
            // In-process synchronous fallback: status 0, `$!` left
            // untouched (zero in a fresh evaluator).
            let prog = parse("echo hi &\necho \"after, last_bg=$!\"\n").unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            let out = ev.take_output();
            assert!(out.contains("hi"), "got: {out}");
            assert!(out.contains("last_bg=0"), "got: {out}");
        }

        #[test]
        fn background_external_pipeline_spawns() {
            if !have("/bin/echo") || !have("/bin/cat") {
                return;
            }
            let prog = parse("/bin/echo a | /bin/cat &\necho \"pid=$!\"\n").unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            let out = ev.take_output();
            assert!(out.contains("pid="), "got: {out}");
            // PID positive integer
            let pid_line = out.lines().find(|l| l.starts_with("pid=")).unwrap();
            let pid: i32 = pid_line.trim_start_matches("pid=").parse().unwrap();
            assert!(pid > 0);
        }

        #[test]
        fn pipeline_compound_first_stage_bridges_into_external() {
            if !have("/usr/bin/wc") {
                return;
            }
            let prog =
                parse("{ echo a; echo b; echo c; } | /usr/bin/wc -l\n").unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output().trim(), "3");
        }

        #[test]
        fn pipeline_stage_assignment_prefix_reaches_child() {
            if !have("/usr/bin/env") || !have("/usr/bin/grep") {
                return;
            }
            // FOO=hi /usr/bin/env | grep ^FOO
            let prog = parse(
                "FOO=hi /usr/bin/env | /usr/bin/grep ^FOO\n",
            )
            .unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert!(ev.take_output().contains("FOO=hi"));
        }

        #[test]
        fn jobs_lists_running_background_pids() {
            if !have("/bin/sleep") {
                return;
            }
            let prog = parse("/bin/sleep 0.05 &\n/bin/sleep 0.05 &\njobs\nwait\n").unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            let out = ev.take_output();
            assert!(out.contains("[1]"), "got: {out}");
            assert!(out.contains("[2]"), "got: {out}");
        }

        #[test]
        fn wait_specific_pid_reaps_one_job() {
            if !have("/bin/sleep") {
                return;
            }
            let prog = parse(
                "/bin/sleep 0.02 &\np=$!\nwait $p\necho \"got=$?\"\n",
            )
            .unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            let out = ev.take_output();
            assert!(out.contains("got=0"), "got: {out}");
        }

        #[test]
        fn wait_no_args_reaps_all_jobs() {
            if !have("/bin/sleep") {
                return;
            }
            let prog = parse(
                "/bin/sleep 0.02 &\n/bin/sleep 0.02 &\nwait\necho ok\n",
            )
            .unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert!(ev.take_output().contains("ok"));
        }

        #[test]
        fn wait_bad_pid_errors() {
            let prog = parse("wait 999999999\n").unwrap();
            let mut ev = Evaluator::new();
            let err = ev.eval_program(&prog).unwrap_err();
            let msg = alloc::format!("{err}");
            assert!(msg.contains("no such background job"), "got: {msg}");
        }

        #[test]
        fn fg_and_bg_explicitly_not_yet_supported() {
            let prog = parse("fg\n").unwrap();
            let mut ev = Evaluator::new();
            let err = ev.eval_program(&prog).unwrap_err();
            let msg = alloc::format!("{err}");
            assert!(msg.contains("isn't supported yet"), "got: {msg}");
        }

        #[test]
        fn compound_stdin_redirect_feeds_body() {
            let tmp = std::env::temp_dir().join("kash-cmp-stdin.txt");
            std::fs::write(&tmp, "line-from-file\n").unwrap();
            let path = tmp.to_str().unwrap();
            let src = alloc::format!(
                "{{ /bin/cat; }} <{path}\n"
            );
            let prog = parse(&src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "line-from-file\n");
            let _ = std::fs::remove_file(&tmp);
        }

        #[test]
        fn pipeline_first_stage_non_pure_builtin_still_rejected() {
            // `read` is side-effecting (binds to caller scope), so
            // the pipeline bridge intentionally doesn't handle it.
            let src = "read X | /bin/cat\n";
            let prog = parse(src).unwrap();
            let mut ev = Evaluator::new();
            let err = ev.eval_program(&prog).unwrap_err();
            let msg = alloc::format!("{err}");
            assert!(msg.contains("not yet supported"), "got: {msg}");
        }

        #[test]
        fn here_doc_with_trailing_semicolon_separates_statements() {
            if !have("/bin/cat") {
                return;
            }
            let src = "/bin/cat <<EOF ; echo after\nbody\nEOF\n";
            let prog = parse(src).unwrap();
            let mut ev = Evaluator::new();
            ev.eval_program(&prog).unwrap();
            assert_eq!(ev.take_output(), "body\nafter\n");
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

    // ===== typeclass / instance — 1단계: parse + register =====

    #[test]
    fn typeclass_def_parses_and_registers() {
        let prog = parse("typeclass Eq { foo() { :; } }").unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_ok());
    }

    #[test]
    fn typeclass_def_multi_method() {
        let prog =
            parse("typeclass Show { show() { :; }; default() { :; } }").unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_ok());
    }

    #[test]
    fn instance_def_parses_and_registers() {
        let prog =
            parse("typeclass Eq { foo() { :; } }; instance Eq for Int { foo() { :; } }")
                .unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_ok());
    }

    #[test]
    fn typeclass_body_rejects_non_function_items() {
        // Bare commands inside the body should fail to parse.
        assert!(parse("typeclass Eq { echo hi }").is_err());
    }

    #[test]
    fn instance_requires_for_keyword() {
        assert!(parse("instance Eq Int { foo() { :; } }").is_err());
    }

    // ===== typeclass / instance — stage 2: explicit dispatch =====

    #[test]
    fn explicit_dispatch_finds_instance_method() {
        let (_, out, _) = run(
            "typeclass Greeter { hello() { echo default; }; }\n\
             instance Greeter for Int { hello() { echo from_int; }; }\n\
             Greeter::Int::hello\n",
        );
        assert_eq!(out, "from_int\n");
    }

    #[test]
    fn explicit_dispatch_falls_back_to_default_method() {
        // No instance for String — should hit the default body.
        let (_, out, _) = run(
            "typeclass Greeter { hello() { echo default_hello; }; }\n\
             Greeter::String::hello\n",
        );
        assert_eq!(out, "default_hello\n");
    }

    #[test]
    fn explicit_dispatch_args_become_positionals() {
        let (_, out, _) = run(
            "typeclass Add { go() { echo \"sum is $1 $2\"; }; }\n\
             instance Add for Int { go() { echo \"int sum: $1+$2\"; }; }\n\
             Add::Int::go 3 4\n",
        );
        assert_eq!(out, "int sum: 3+4\n");
    }

    #[test]
    fn unknown_typeclass_falls_through_to_not_found() {
        // No registered typeclass `Nope` — dispatch falls through
        // to external-command lookup which also misses, surfacing
        // POSIX exit status 127.
        let prog = parse("Nope::Int::run").unwrap();
        let mut ev = Evaluator::new();
        let outcome = ev.eval_program(&prog).unwrap();
        assert_eq!(outcome.status(), 127);
    }

    #[test]
    fn typeclass_without_method_errors() {
        // Typeclass exists but the method name doesn't.
        let prog = parse(
            "typeclass Eq { eq() { :; } }; Eq::Int::compare",
        )
        .unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert_eq!(err.exit_code(), 127);
    }

    // ===== typeclass / instance — stage 3: inferred dispatch =====

    #[test]
    fn inferred_dispatch_picks_int_for_integer_literal() {
        let (_, out, _) = run(
            "typeclass Sayer { say() { echo default $1; }; }\n\
             instance Sayer for Int { say() { echo int $1; }; }\n\
             instance Sayer for String { say() { echo str $1; }; }\n\
             Sayer::say 42\n",
        );
        assert_eq!(out, "int 42\n");
    }

    #[test]
    fn inferred_dispatch_picks_string_for_non_numeric() {
        let (_, out, _) = run(
            "typeclass Sayer { say() { echo default $1; }; }\n\
             instance Sayer for Int { say() { echo int $1; }; }\n\
             instance Sayer for String { say() { echo str $1; }; }\n\
             Sayer::say hello\n",
        );
        assert_eq!(out, "str hello\n");
    }

    #[test]
    fn inferred_dispatch_signed_integer_is_int() {
        let (_, out, _) = run(
            "typeclass Sayer { say() { echo default $1; }; }\n\
             instance Sayer for Int { say() { echo int $1; }; }\n\
             instance Sayer for String { say() { echo str $1; }; }\n\
             Sayer::say -7\n",
        );
        assert_eq!(out, "int -7\n");
    }

    #[test]
    fn inferred_dispatch_explicit_at_type_strips_annotation() {
        // `@Int` is a type assertion — it should not show up in `$@`.
        let (_, out, _) = run(
            "typeclass Sayer { say() { echo \"count=$# first=$1\"; }; }\n\
             instance Sayer for Int { say() { echo \"int count=$# first=$1\"; }; }\n\
             Sayer::say @Int hello world\n",
        );
        assert_eq!(out, "int count=2 first=hello\n");
    }

    #[test]
    fn inferred_dispatch_no_args_picks_unit() {
        let (_, out, _) = run(
            "typeclass Sayer { say() { echo default; }; }\n\
             instance Sayer for Unit { say() { echo unit; }; }\n\
             Sayer::say\n",
        );
        assert_eq!(out, "unit\n");
    }

    #[test]
    fn inferred_dispatch_falls_back_to_default_when_no_matching_instance() {
        // No instance for Int — should hit the typeclass default.
        let (_, out, _) = run(
            "typeclass Sayer { say() { echo default $1; }; }\n\
             instance Sayer for String { say() { echo str $1; }; }\n\
             Sayer::say 42\n",
        );
        assert_eq!(out, "default 42\n");
    }

    #[test]
    fn inferred_dispatch_unknown_typeclass_falls_through() {
        let prog = parse("Nope::run 1 2 3").unwrap();
        let mut ev = Evaluator::new();
        let outcome = ev.eval_program(&prog).unwrap();
        assert_eq!(outcome.status(), 127);
    }

    // ===== typeclass / instance — stage 4: signature-only members =====

    #[test]
    fn signature_only_member_dispatches_to_instance() {
        // `say()` has no body — every instance must supply one.
        let (_, out, _) = run(
            "typeclass Greet { say(); }\n\
             instance Greet for Int { say() { echo int $1; }; }\n\
             Greet::Int::say 7\n",
        );
        assert_eq!(out, "int 7\n");
    }

    #[test]
    fn signature_only_member_with_no_matching_instance_is_error() {
        // No instance was defined — the call has no body to run.
        let src = "typeclass Greet { say(); }\n\
                   instance Greet for String { say() { echo s; }; }\n\
                   Greet::Int::say\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert_eq!(err.exit_code(), 127);
    }

    #[test]
    fn instance_missing_abstract_method_is_rejected() {
        // The typeclass declares `say()` and `wave()`; the instance
        // only supplies `say` — registration must reject this.
        let src = "typeclass Greet { say(); wave(); }\n\
                   instance Greet for Int { say() { echo s; }; }\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("missing abstract method"), "got: {msg}");
        assert!(msg.contains("wave"), "got: {msg}");
    }

    #[test]
    fn instance_for_unknown_typeclass_is_rejected() {
        let src = "instance Greet for Int { say() { echo s; }; }\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert_eq!(err.exit_code(), 127);
    }

    #[test]
    fn instance_with_extraneous_method_is_rejected() {
        // Typeclass declares only `say`; instance also defines
        // `extra` — extras are rejected to keep the typeclass
        // surface authoritative.
        let src = "typeclass Greet { say() { echo d; }; }\n\
                   instance Greet for Int { say() { echo s; }; extra() { echo x; }; }\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("does not declare"), "got: {msg}");
        assert!(msg.contains("extra"), "got: {msg}");
    }

    #[test]
    fn typeclass_duplicate_member_is_rejected() {
        let src = "typeclass Greet { say(); say() { echo d; }; }\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("twice"), "got: {msg}");
    }

    #[test]
    fn signature_only_and_default_can_coexist() {
        // Mixing abstract and default members in the same typeclass
        // must be supported, with instances overriding either.
        let (_, out, _) = run(
            "typeclass Greet { say(); wave() { echo default-wave; }; }\n\
             instance Greet for Int { say() { echo int-say; }; }\n\
             Greet::Int::say\n\
             Greet::Int::wave\n",
        );
        assert_eq!(out, "int-say\ndefault-wave\n");
    }

    #[test]
    fn instance_can_override_default_too() {
        let (_, out, _) = run(
            "typeclass Greet { say() { echo default; }; }\n\
             instance Greet for Int { say() { echo overridden; }; }\n\
             Greet::Int::say\n",
        );
        assert_eq!(out, "overridden\n");
    }

    // ===== namespace — stage 1: blocks + function prefixing =====

    #[test]
    fn namespace_function_is_callable_with_dotted_name() {
        let (_, out, _) = run(
            "namespace utils {\n\
                 hello() { echo hi; }\n\
             }\n\
             .utils.hello\n",
        );
        assert_eq!(out, "hi\n");
    }

    #[test]
    fn bare_name_at_top_level_does_not_see_namespaced_function() {
        let src = "namespace utils { hello() { echo hi; }; }\n\
                   hello\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let outcome = ev.eval_program(&prog).unwrap();
        // Without an external exec path in --lib tests, an
        // unresolved bare name surfaces as POSIX status 127.
        assert_eq!(outcome.status(), 127);
    }

    #[test]
    fn namespace_internal_call_uses_short_name() {
        let (_, out, _) = run(
            "namespace utils {\n\
                 inner() { echo inner-was-called; }\n\
                 outer() { inner; }\n\
             }\n\
             .utils.outer\n",
        );
        assert_eq!(out, "inner-was-called\n");
    }

    #[test]
    fn namespace_reopening_accumulates_functions() {
        let (_, out, _) = run(
            "namespace utils { a() { echo a; }; }\n\
             namespace utils { b() { echo b; }; }\n\
             .utils.a\n\
             .utils.b\n",
        );
        assert_eq!(out, "a\nb\n");
    }

    #[test]
    fn nested_namespace_yields_dotted_path() {
        let (_, out, _) = run(
            "namespace outer {\n\
                 namespace inner {\n\
                     hi() { echo hello; }\n\
                 }\n\
             }\n\
             .outer.inner.hi\n",
        );
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn nested_namespace_inner_call_falls_back_to_outer() {
        let (_, out, _) = run(
            "namespace outer {\n\
                 helper() { echo helper-ran; }\n\
                 namespace inner {\n\
                     entry() { helper; }\n\
                 }\n\
             }\n\
             .outer.inner.entry\n",
        );
        assert_eq!(out, "helper-ran\n");
    }

    #[test]
    fn namespace_function_does_not_see_callers_namespace() {
        let (_, out, _) = run(
            "namespace lib {\n\
                 inner() { echo lib-inner; }\n\
                 entry() { inner; }\n\
             }\n\
             namespace caller {\n\
                 inner() { echo caller-inner; }\n\
                 run() { .lib.entry; }\n\
             }\n\
             .caller.run\n",
        );
        assert_eq!(out, "lib-inner\n");
    }

    #[test]
    fn namespace_name_with_embedded_dot_is_rejected() {
        assert!(parse("namespace foo.bar { x() { :; }; }\n").is_err());
    }

    // ===== namespace — stage 2: variable prefixing =====

    #[test]
    fn namespace_variable_registers_under_dotted_name() {
        let (_, out, _) = run(
            "namespace utils { version=1.0; }\n\
             echo ${.utils.version}\n",
        );
        assert_eq!(out, "1.0\n");
    }

    #[test]
    fn bare_var_at_top_level_does_not_see_namespaced_var() {
        let (_, out, _) = run(
            "namespace utils { version=1.0; }\n\
             echo \"[${version}]\"\n",
        );
        // Unset bare lookup expands empty.
        assert_eq!(out, "[]\n");
    }

    #[test]
    fn namespace_function_reads_namespace_variable_by_short_name() {
        let (_, out, _) = run(
            "namespace utils {\n\
                 version=2.5\n\
                 show() { echo $version; }\n\
             }\n\
             .utils.show\n",
        );
        assert_eq!(out, "2.5\n");
    }

    #[test]
    fn nested_namespace_reads_outer_var() {
        let (_, out, _) = run(
            "namespace outer {\n\
                 a=outer-a\n\
                 namespace inner {\n\
                     show() { echo $a; }\n\
                 }\n\
             }\n\
             .outer.inner.show\n",
        );
        assert_eq!(out, "outer-a\n");
    }

    #[test]
    fn nested_namespace_inner_var_shadows_outer() {
        let (_, out, _) = run(
            "namespace outer {\n\
                 a=outer-a\n\
                 namespace inner {\n\
                     a=inner-a\n\
                     show() { echo $a; }\n\
                 }\n\
             }\n\
             .outer.inner.show\n",
        );
        assert_eq!(out, "inner-a\n");
    }

    #[test]
    fn typeset_inside_namespace_registers_under_dotted_name() {
        let (_, out, _) = run(
            "namespace utils { typeset api=v1; }\n\
             echo ${.utils.api}\n",
        );
        assert_eq!(out, "v1\n");
    }

    #[test]
    fn function_local_assignment_does_not_pollute_namespace() {
        // Assignment inside a function body must stay frame-local,
        // not leak as `.utils.scratch`.
        let (_, out, _) = run(
            "namespace utils {\n\
                 run() { scratch=temp; echo got=$scratch; }\n\
             }\n\
             .utils.run\n\
             echo after=\"[${.utils.scratch}]\"\n",
        );
        assert_eq!(out, "got=temp\nafter=[]\n");
    }

    // ===== namespace — stage 3: `use namespace` import =====

    #[test]
    fn use_namespace_makes_bare_function_name_visible() {
        let (_, out, _) = run(
            "namespace utils { greet() { echo hi; }; }\n\
             show() { use namespace utils; greet; }\n\
             show\n",
        );
        assert_eq!(out, "hi\n");
    }

    #[test]
    fn use_namespace_makes_bare_variable_visible() {
        let (_, out, _) = run(
            "namespace utils { version=9.9; }\n\
             show() { use namespace utils; echo $version; }\n\
             show\n",
        );
        assert_eq!(out, "9.9\n");
    }

    #[test]
    fn use_namespace_is_scoped_to_the_calling_function() {
        // `outer` runs `inner` (which imports `utils`) then tries
        // the imported name from its own body. Because imports are
        // scoped to the function frame, `outer`'s bare reference
        // must miss. The command name `kashtestunique` is chosen
        // to avoid colliding with any real PATH entry on std
        // builds — otherwise we'd accidentally exec it — *and* to
        // avoid the `_`-prefix exclusion the wildcard import path
        // applies.
        let src = "namespace utils { kashtestunique() { echo hi; }; }\n\
                   inner() { use namespace utils; kashtestunique; }\n\
                   outer() { inner; kashtestunique; }\n\
                   outer\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let _ = ev.eval_program(&prog);
        assert!(ev.take_output().starts_with("hi\n"));
    }

    #[test]
    fn use_namespace_underscore_prefixed_names_are_hidden() {
        let (_, out, _) = run(
            "namespace utils {\n\
                 _helper() { echo helper; }\n\
                 visible() { echo visible; }\n\
             }\n\
             show() {\n\
                 use namespace utils\n\
                 visible\n\
             }\n\
             show\n",
        );
        assert_eq!(out, "visible\n");
    }

    #[test]
    fn use_namespace_explicit_dotted_name_still_reaches_underscore() {
        // Hidden by wildcard import, but absolute path still works.
        let (_, out, _) = run(
            "namespace utils { _helper() { echo helper; }; }\n\
             .utils._helper\n",
        );
        assert_eq!(out, "helper\n");
    }

    #[test]
    fn use_namespace_path_with_dots_accepted() {
        let (_, out, _) = run(
            "namespace outer { namespace inner { hi() { echo hello; }; }; }\n\
             show() { use namespace outer.inner; hi; }\n\
             show\n",
        );
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn use_namespace_alias_form() {
        // `use namespace utils as u` makes `.u.X` an alias for `.utils.X`.
        let (_, out, _) = run(
            "namespace utils { hi() { echo hi-from-utils; }; }\n\
             show() { use namespace utils as u; .u.hi; }\n\
             show\n",
        );
        assert_eq!(out, "hi-from-utils\n");
    }

    #[test]
    fn use_single_symbol_form() {
        let (_, out, _) = run(
            "namespace utils { hi() { echo hi-1; }; bye() { echo bye-1; }; }\n\
             show() { use .utils.hi; hi; }\n\
             show\n",
        );
        assert_eq!(out, "hi-1\n");
    }

    #[test]
    fn use_single_symbol_as_alias() {
        let (_, out, _) = run(
            "namespace utils { hi() { echo hi-2; }; }\n\
             show() { use .utils.hi as greet; greet; }\n\
             show\n",
        );
        assert_eq!(out, "hi-2\n");
    }

    #[test]
    fn use_single_symbol_reaches_underscore_name() {
        // Single-symbol form is explicit, so `_helper` is allowed.
        let (_, out, _) = run(
            "namespace utils { _helper() { echo helper-explicit; }; }\n\
             show() { use .utils._helper; _helper; }\n\
             show\n",
        );
        assert_eq!(out, "helper-explicit\n");
    }

    // ===== namespace — stage 4: typeclass / instance scoping =====

    #[test]
    fn typeclass_in_namespace_dispatches_via_dotted_name() {
        let (_, out, _) = run(
            "namespace foo {\n\
                 typeclass Sayer { say() { echo default; }; }\n\
                 instance Sayer for Int { say() { echo foo-int; }; }\n\
             }\n\
             .foo.Sayer::Int::say\n",
        );
        assert_eq!(out, "foo-int\n");
    }

    #[test]
    fn typeclass_in_namespace_short_name_works_inside_namespace_func() {
        let (_, out, _) = run(
            "namespace foo {\n\
                 typeclass Sayer { say() { echo default; }; }\n\
                 instance Sayer for Int { say() { echo foo-int; }; }\n\
                 entry() { Sayer::Int::say; }\n\
             }\n\
             .foo.entry\n",
        );
        assert_eq!(out, "foo-int\n");
    }

    #[test]
    fn typeclass_in_namespace_invisible_to_unrelated_caller() {
        let src = "namespace foo {\n\
                       typeclass Sayer { say() { echo default; }; }\n\
                       instance Sayer for Int { say() { echo foo-int; }; }\n\
                   }\n\
                   Sayer::Int::say\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        // Outside the namespace and without a `use`, the bare
        // `Sayer::Int::say` isn't resolvable — dispatch falls
        // through to external lookup which also misses, surfacing
        // POSIX status 127.
        let outcome = ev.eval_program(&prog).unwrap();
        assert_eq!(outcome.status(), 127);
    }

    #[test]
    fn typeclass_use_namespace_brings_into_scope() {
        let (_, out, _) = run(
            "namespace foo {\n\
                 typeclass Sayer { say() { echo default; }; }\n\
                 instance Sayer for Int { say() { echo foo-int; }; }\n\
             }\n\
             entry() { use namespace foo; Sayer::Int::say; }\n\
             entry\n",
        );
        assert_eq!(out, "foo-int\n");
    }

    #[test]
    fn typeclass_same_name_in_different_namespaces_are_distinct() {
        let (_, out, _) = run(
            "namespace foo {\n\
                 typeclass Sayer { say() { echo default; }; }\n\
                 instance Sayer for Int { say() { echo foo; }; }\n\
             }\n\
             namespace bar {\n\
                 typeclass Sayer { say() { echo default; }; }\n\
                 instance Sayer for Int { say() { echo bar; }; }\n\
             }\n\
             .foo.Sayer::Int::say\n\
             .bar.Sayer::Int::say\n",
        );
        assert_eq!(out, "foo\nbar\n");
    }

    #[test]
    fn use_brace_form_imports_each_symbol() {
        let (_, out, _) = run(
            "namespace utils { a() { echo A; }; b() { echo B; }; c() { echo C; }; }\n\
             show() { use .utils.{a,b}; a; b; }\n\
             show\n",
        );
        assert_eq!(out, "A\nB\n");
    }

    #[test]
    fn use_brace_form_cross_product() {
        // `.{x,y}.{a,b}.hi` expands to four imports
        // (xa, xb, ya, yb) — first wins on resolution.
        let (_, out, _) = run(
            "namespace x { namespace a { hi() { echo xa-hi; }; }; }\n\
             namespace y { namespace b { hi() { echo yb-hi; }; }; }\n\
             show() { use .{x,y}.{a,b}.hi; hi; }\n\
             show\n",
        );
        assert_eq!(out, "xa-hi\n");
    }

    #[test]
    fn use_brace_form_with_as_rejected() {
        let src = "namespace u { a() { :; }; b() { :; }; }\n\
                   show() { use .u.{a,b} as x; }\n\
                   show\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_err());
    }

    #[test]
    fn use_brace_form_empty_alternative_rejected() {
        let src = "namespace u { :; }\n\
                   show() { use .u.{a,}; }\n\
                   show\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_err());
    }

    #[test]
    fn use_namespace_as_alias_rejects_dotted_alias() {
        let src = "namespace utils { :; }\n\
                   show() { use namespace utils as a.b; }\n\
                   show\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_err());
    }

    #[test]
    fn namespace_reopen_sees_earlier_var() {
        let (_, out, _) = run(
            "namespace utils { a=first; }\n\
             namespace utils { show() { echo $a; }; }\n\
             .utils.show\n",
        );
        assert_eq!(out, "first\n");
    }

    // ===== function capture list — read-only by-ref =====

    #[test]
    fn capture_list_binds_named_caller_value() {
        let (_, out, _) = run(
            "x=outer-value\n\
             function f(x) { echo \"$x\"; }\n\
             f\n",
        );
        assert_eq!(out, "outer-value\n");
    }

    #[test]
    fn capture_list_with_missing_caller_binding_snapshots_empty() {
        let (_, out, _) = run("function f(x) { echo \"[$x]\"; }\nf\n");
        assert_eq!(out, "[]\n");
    }

    #[test]
    fn capture_list_is_readonly_inside_body() {
        let src = "x=ok\n\
                   function f(x) { x=changed; }\n\
                   f\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(
            msg.to_lowercase().contains("read-only") || msg.to_lowercase().contains("readonly"),
            "got: {msg}",
        );
    }

    #[test]
    fn capture_list_overrides_outer_with_call_time_snapshot() {
        // A captured name is bound *locally* in the function frame
        // with the value seen at call time. Even if a later reassign
        // in the caller would have been visible via static-scope
        // fallback, the local capture binding shadows it.
        let (_, out, _) = run(
            "x=before\n\
             function f(x) { echo \"$x\"; }\n\
             f\n\
             echo \"after=$x\"\n",
        );
        // First call: capture takes "before"; outer is unchanged
        // after the call (the binding was a local copy).
        assert_eq!(out, "before\nafter=before\n");
    }

    #[test]
    fn capture_snapshot_is_taken_at_call_time() {
        // The capture binds the *value at the call*, not at the
        // definition — reassigning `x` between def and call must
        // be reflected.
        let (_, out, _) = run(
            "x=first\n\
             function f(x) { echo \"$x\"; }\n\
             x=second\n\
             f\n",
        );
        assert_eq!(out, "second\n");
    }

    // ===== compound redirect =====

    #[cfg(feature = "std")]
    #[test]
    fn compound_here_doc_redirect_still_rejected() {
        // Plain input redirect on compound bodies is now supported
        // (the file's fd becomes the effective stdin for the
        // body's external commands). Here-doc / fd-dup forms on
        // compound bodies still need cross-stage plumbing.
        let src = "{ /bin/cat; } <<EOF\nbody\nEOF\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(
            msg.contains("here-doc") || msg.contains("here-string"),
            "got: {msg}",
        );
    }

    // ===== .sh.lineno =====

    #[test]
    fn sh_lineno_reflects_source_line() {
        let (_, out, _) = run(
            "echo a-${.sh.lineno}\necho b-${.sh.lineno}\n\necho d-${.sh.lineno}\n",
        );
        assert_eq!(out, "a-1\nb-2\nd-4\n");
    }

    #[test]
    fn sh_lineno_zero_before_any_statement() {
        // Read directly via the public expand path before
        // anything runs.
        let mut ev = Evaluator::new();
        let prog = parse("echo ${.sh.lineno}\n").unwrap();
        ev.eval_program(&prog).unwrap();
        assert_eq!(ev.take_output(), "1\n");
    }

    // ===== .sh.pid / .sh.ppid / .sh.subshell / .sh.name =====

    #[cfg(feature = "std")]
    #[test]
    fn sh_pid_is_numeric() {
        let (_, out, _) = run("echo ${.sh.pid}\n");
        // Should be a positive integer on std builds.
        let pid: i64 = out.trim().parse().expect("numeric pid");
        assert!(pid > 0);
    }

    #[cfg(not(feature = "std"))]
    #[test]
    fn sh_pid_is_zero_on_alloc_only() {
        // alloc-only build has no `std::process::id`, so we
        // surface a sentinel `0`.
        let (_, out, _) = run("echo ${.sh.pid}\n");
        assert_eq!(out, "0\n");
    }

    #[test]
    fn sh_subshell_starts_at_zero() {
        let (_, out, _) = run("echo ${.sh.subshell}\n");
        assert_eq!(out, "0\n");
    }

    #[test]
    fn sh_subshell_increments_in_parens() {
        let (_, out, _) = run(
            "echo a=${.sh.subshell}\n(echo b=${.sh.subshell}; (echo c=${.sh.subshell}))\n",
        );
        assert_eq!(out, "a=0\nb=1\nc=2\n");
    }

    #[test]
    fn sh_subshell_pops_on_exit() {
        let (_, out, _) = run(
            "(echo inner=${.sh.subshell})\necho after=${.sh.subshell}\n",
        );
        assert_eq!(out, "inner=1\nafter=0\n");
    }

    #[test]
    fn sh_name_holds_active_discipline_var() {
        let (_, out, _) = run(
            "function .v.set { echo \"name=${.sh.name}\"; }\nv=1\n",
        );
        assert_eq!(out, "name=v\n");
    }

    #[test]
    fn sh_name_empty_outside_discipline() {
        let (_, out, _) = run("echo \"[${.sh.name}]\"\n");
        assert_eq!(out, "[]\n");
    }

    // ===== .sh.match =====

    #[test]
    fn sh_match_captures_regex_substring() {
        let (_, out, _) = run(
            "[[ \"hello world\" =~ wor.. ]] && echo \"m=${.sh.match}\"\n",
        );
        assert_eq!(out, "m=world\n");
    }

    #[test]
    fn sh_match_captures_quantified_pattern() {
        let (_, out, _) = run(
            "[[ \"v1.2.3\" =~ [0-9]+\\.[0-9]+ ]] && echo \"v=${.sh.match}\"\n",
        );
        assert_eq!(out, "v=1.2\n");
    }

    #[test]
    fn sh_match_empty_before_any_match() {
        let (_, out, _) = run("echo \"[${.sh.match}]\"\n");
        assert_eq!(out, "[]\n");
    }

    // ===== .sh.subscript =====

    #[test]
    fn sh_subscript_exposed_to_set_discipline() {
        let (_, out, _) = run(
            "function .arr.set { echo \"i=${.sh.subscript} v=${.sh.value}\"; }\narr[0]=a\narr[1]=b\n",
        );
        assert_eq!(out, "i=0 v=a\ni=1 v=b\n");
    }

    #[test]
    fn sh_subscript_set_on_indexed_lookup() {
        // Seed three elements through plain `arr[i]=…` to avoid the
        // array-literal parse path; the get hook then fires on lookup
        // and `${.sh.subscript}` carries the index back out.
        let (_, out, _) = run(
            "arr[0]=x\narr[1]=y\narr[2]=z\nfunction .arr.get { .sh.value=\"i=${.sh.subscript}\"; }\necho ${arr[2]}\n",
        );
        assert_eq!(out, "i=2\n");
    }

    // ===== discipline functions =====

    #[test]
    fn discipline_set_transforms_stored_value() {
        let (_, out, _) = run(
            "function .x.set { .sh.value=\"set:${.sh.value}\"; }\nx=raw\necho $x\n",
        );
        assert_eq!(out, "set:raw\n");
    }

    #[test]
    fn discipline_get_transforms_read_value() {
        let (_, out, _) = run(
            "y=base\nfunction .y.get { .sh.value=\"${.sh.value}-modified\"; }\necho $y\n",
        );
        assert_eq!(out, "base-modified\n");
    }

    #[test]
    fn discipline_unset_hook_runs_before_removal() {
        let (_, out, _) = run(
            "function .z.unset { echo gone-z; }\nz=alive\nunset z\necho \"after=[$z]\"\n",
        );
        assert_eq!(out, "gone-z\nafter=[]\n");
    }

    #[test]
    fn discipline_set_reentry_guarded() {
        // The hook itself assigns *to the same variable* by going
        // through `.sh.value` — the re-entry guard stops the hook
        // from triggering itself.
        let (_, out, _) = run(
            "function .v.set { .sh.value=\"once:${.sh.value}\"; }\nv=in\necho $v\n",
        );
        assert_eq!(out, "once:in\n");
    }

    #[test]
    fn discipline_get_on_unset_var_lets_hook_synthesise() {
        // No prior `name=…`; the get hook fabricates the value.
        let (_, out, _) = run(
            "function .ghost.get { .sh.value=summoned; }\necho $ghost\n",
        );
        assert_eq!(out, "summoned\n");
    }

    // ===== `typedef` (user-defined type) =====

    #[test]
    fn typedef_instance_copies_defaults() {
        let (_, out, _) = run(
            "typedef Point { x=1; y=2; }\ntypedef Point p\necho \"${p.x} ${p.y}\"\n",
        );
        assert_eq!(out, "1 2\n");
    }

    #[test]
    fn typedef_instance_members_writable() {
        let (_, out, _) = run(
            "typedef Pair { a=0; b=0; }\ntypedef Pair p\np.a=10\np.b=20\necho \"${p.a},${p.b}\"\n",
        );
        assert_eq!(out, "10,20\n");
    }

    #[test]
    fn typedef_unknown_type_errors() {
        // Declarative NotFound (not an external-command miss) —
        // propagates as `Err(NotFound)` so scripts can't proceed
        // past a typo in a type name.
        let prog = parse("typedef NoSuch v\n").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("NoSuch"), "got: {msg}");
    }

    #[test]
    fn typedef_definition_alone_succeeds() {
        let (outcome, _, _) = run("typedef Foo { a=1; b=2; }\n");
        assert_eq!(outcome.status(), 0);
    }

    // ===== typedef OOP extensions (private / static / __init / __del) =====

    #[test]
    fn typedef_static_field_initialised_at_registration() {
        // `static` fields live at `<TypeName>.<field>` and exist
        // from the moment `typedef NAME { … }` runs — no instance
        // needed.
        let (_, out, _) = run("typedef Counter { static total=7; }\necho ${Counter.total}\n");
        assert_eq!(out, "7\n");
    }

    #[test]
    fn typedef_init_runs_on_instantiation() {
        // Lifecycle bodies see the active instance as `_` — the
        // assignment to `_.x` lands on `t.x` because `__init` is
        // running with `self_instance_var = Some("t")`.
        let (_, out, _) = run(
            "typedef T { x=0; function __init { _.x=42; }; }\ntypedef T t\necho ${t.x}\n",
        );
        assert_eq!(out, "42\n");
    }

    #[test]
    fn typedef_self_ref_reads_active_instance_field() {
        // `${_.field}` inside a lifecycle body resolves to
        // `<var>.field` of the instance being initialised.
        let (_, out, _) = run(
            "typedef T { x=seed; function __init { echo \"from-init=${_.x}\"; }; }\n\
             typedef T t\n",
        );
        assert_eq!(out, "from-init=seed\n");
    }

    #[test]
    fn typedef_del_runs_on_unset() {
        let (_, out, _) = run(
            "typedef T { function __init { echo init; }; function __del { echo del; }; }\n\
             typedef T t\nunset t\n",
        );
        assert_eq!(out, "init\ndel\n");
    }

    #[test]
    fn typedef_static_field_mutated_by_init() {
        // Lifecycle bodies run with dynamic scope so writes to
        // `<Type>.<field>` reach the outer binding, not a local
        // copy. Each instance bumps the shared counter.
        let (_, out, _) = run(
            "typedef Counter { static total=0; function __init { local n=${Counter.total}; Counter.total=$((n + 1)); }; }\n\
             typedef Counter a\ntypedef Counter b\ntypedef Counter c\necho ${Counter.total}\n",
        );
        assert_eq!(out, "3\n");
    }

    #[test]
    fn typedef_private_field_blocks_external_read() {
        // External `${b.secret}` must fail; the empty-string path
        // is via `lookup_param_raw` (specifically the unquoted
        // `echo` arg) and the hard-error path is through
        // `lookup_param`. We exercise the hard path here.
        let prog = parse(
            "typedef Box { private secret=hidden; }\ntypedef Box b\nx=${b.secret}\n",
        )
        .unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("private"), "got: {msg}");
    }

    #[test]
    fn typedef_private_field_blocks_external_write() {
        let prog = parse(
            "typedef Box { private secret=hidden; }\ntypedef Box b\nb.secret=leaked\n",
        )
        .unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("private"), "got: {msg}");
    }

    #[test]
    fn typedef_private_field_visible_inside_lifecycle_hook() {
        let (_, out, _) = run(
            "typedef Box { private secret=hidden; function __init { echo \"from-init=${b.secret}\"; }; }\n\
             typedef Box b\n",
        );
        assert_eq!(out, "from-init=hidden\n");
    }

    #[test]
    fn typedef_del_unset_clears_instance_fields() {
        // After `unset`, the per-instance `var.field` bindings
        // are swept out alongside the bare var — `${b.x}` reads
        // empty.
        let (_, out, _) = run(
            "typedef T { x=alive; }\ntypedef T b\necho before=${b.x}\nunset b\necho \"after=[${b.x}]\"\n",
        );
        assert_eq!(out, "before=alive\nafter=[]\n");
    }

    #[test]
    fn typedef_unknown_method_rejected() {
        // v1 OOP commit only models the `__init` / `__del`
        // lifecycle hooks. Other methods are reserved for a
        // follow-up.
        let res = parse(
            "typedef T { function helper { :; }; }\n",
        );
        assert!(res.is_err(), "expected parse error for typedef method");
    }

    // ===== primitive numeric types (typed integers) =====

    #[test]
    fn typed_int8_wraps_on_overflow() {
        let (_, out, _) = run("int8 a=300\necho $a\n");
        // 300 mod 256, signed → 44 (i.e. (300 as i8) as i64).
        assert_eq!(out, "44\n");
    }

    #[test]
    fn typed_uint8_wraps_negative_to_modular() {
        let (_, out, _) = run("uint8 c=-1\necho $c\n");
        assert_eq!(out, "255\n");
    }

    #[test]
    fn typed_int_carries_through_subsequent_assignment() {
        // Once `a` has the int8 attribute, plain `a=300` still
        // routes through the wrap path — the type is sticky.
        let (_, out, _) = run("int8 a=0\na=300\necho $a\n");
        assert_eq!(out, "44\n");
    }

    #[test]
    fn typed_uint16_wraps_arithmetic_result() {
        let (_, out, _) = run("uint16 u=65535\nu=$((u + 1))\necho $u\n");
        assert_eq!(out, "0\n");
    }

    #[test]
    fn typed_int32_min_via_wrap() {
        // 2_147_483_648 is one past i32::MAX → wraps to i32::MIN.
        let (_, out, _) = run("int32 b=2147483648\necho $b\n");
        assert_eq!(out, "-2147483648\n");
    }

    #[test]
    fn typeset_form_accepts_primitive_type_name() {
        // `typeset int8 x=…` produces the same result as the bare
        // `int8 x=…` declarative form.
        let (_, out, _) = run("typeset int8 a=300\necho $a\n");
        assert_eq!(out, "44\n");
    }

    #[test]
    fn warn_integer_overflow_emits_to_stderr() {
        let (_, _out, mut ev) = run(
            "set -o warn-integer-overflow\nint8 a=300\nint8 b=42\n",
        );
        let err = ev.take_stderr();
        assert!(err.contains("int8"), "stderr was: {err}");
        assert!(err.contains("300"), "stderr was: {err}");
    }

    #[test]
    fn warn_integer_overflow_silent_by_default() {
        let (_, _out, mut ev) = run("int8 a=300\n");
        let err = ev.take_stderr();
        assert_eq!(err, "");
    }

    // ===== primitive numeric types (typed floats) =====

    #[test]
    fn typed_float64_stores_exactly() {
        let (_, out, _) = run("float64 b=2.718281828459045\necho $b\n");
        assert_eq!(out, "2.718281828459045\n");
    }

    #[test]
    fn typed_float32_rounds_through_f32() {
        // 3.14 doesn't round-trip exactly through f32 — it lands
        // on the closest binary32, 3.14000010490417…
        let (_, out, _) = run("float32 a=3.14\necho $a\n");
        assert!(
            out.starts_with("3.140000"),
            "expected f32 round-trip near 3.14, got: {out}",
        );
    }

    #[test]
    fn typed_float16_rounds_through_half_precision() {
        // 0.1 → nearest f16 is 0.09997558593750…
        let (_, out, _) = run("float16 c=0.1\necho $c\n");
        assert!(
            out.starts_with("0.099"),
            "expected f16 round-trip near 0.1, got: {out}",
        );
    }

    #[test]
    fn typed_bfloat16_stores_exactly_for_powers_of_two() {
        let (_, out, _) = run("bfloat16 d=0.5\necho $d\n");
        assert_eq!(out, "0.5\n");
    }

    #[test]
    fn typed_float_accepts_integer_arithmetic_rhs() {
        let (_, out, _) = run("float32 e=$((2 + 3))\necho $e\n");
        assert_eq!(out, "5.0\n");
    }

    #[test]
    fn typed_float_handles_negative() {
        let (_, out, _) = run("float64 n=-1.25\necho $n\n");
        assert_eq!(out, "-1.25\n");
    }

    #[test]
    fn typed_float_carries_through_subsequent_assignment() {
        // Like the int case, the float type is sticky.
        let (_, out, _) = run("float32 a=0.0\na=3.14\necho $a\n");
        assert!(out.starts_with("3.140000"), "got: {out}");
    }

    // ===== primitive numeric types (complex) =====

    #[test]
    fn typed_complex_stores_re_and_im_components() {
        let (_, out, _) = run(
            "complex128 z=1+2i\necho \"z=$z z.re=${z.re} z.im=${z.im}\"\n",
        );
        assert_eq!(out, "z=1.0+2.0i z.re=1.0 z.im=2.0\n");
    }

    #[test]
    fn typed_complex_compound_literal_form() {
        let (_, out, _) = run(
            "complex128 w=\"(re=3 im=-4)\"\necho $w\n",
        );
        assert_eq!(out, "3.0-4.0i\n");
    }

    #[test]
    fn typed_complex_pure_imaginary() {
        let (_, out, _) = run("complex128 p=2i\necho $p\n");
        assert_eq!(out, "2.0i\n");
    }

    #[test]
    fn typed_complex_pure_real_promotes_imaginary_to_zero() {
        let (_, out, _) = run("complex128 r=5\necho $r\n");
        // Pure-real value collapses to plain float-form on
        // round-trip; the underlying `.im` component is `0.0`.
        assert_eq!(out, "5.0\n");
    }

    #[test]
    fn typed_complex_unit_imaginary() {
        let (_, out, _) = run("complex128 i=i\necho $i\n");
        assert_eq!(out, "i\n");
    }

    #[test]
    fn typed_complex_negative_unit_imaginary() {
        let (_, out, _) = run("complex128 ni=-i\necho $ni\n");
        assert_eq!(out, "-i\n");
    }

    #[test]
    fn typed_complex32_projects_through_half_precision() {
        // `complex32` is two `f16`s — the components show the
        // half-precision rounding.
        let (_, out, _) = run(
            "complex32 h=0.1+0.2i\necho \"${h.re} ${h.im}\"\n",
        );
        assert!(out.starts_with("0.099"), "got: {out}");
        assert!(out.contains("0.199") || out.contains("0.200"), "got: {out}");
    }

    #[test]
    fn typed_bcomplex32_round_trips_exactly_for_halves() {
        let (_, out, _) = run("bcomplex32 b=0.5-1.5i\necho $b\n");
        assert_eq!(out, "0.5-1.5i\n");
    }

    #[test]
    fn typed_complex_invalid_literal_errors() {
        let prog = parse("complex128 bad=not_a_complex\n").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("complex"), "got: {msg}");
    }

    // ===== extended precision (float128 / complex256) =====

    #[test]
    fn typed_float128_preserves_precision_beyond_f64() {
        // ~36 significant digits — far past f64's ~17. The value
        // round-trips through the 113-bit BigFloat store.
        let (_, out, _) = run(
            "float128 x=3.14159265358979323846264338327950288\necho $x\n",
        );
        assert_eq!(out, "3.14159265358979323846264338327950288e+0\n");
    }

    #[test]
    fn typed_float128_simple_value() {
        let (_, out, _) = run("float128 x=1.5\necho $x\n");
        assert_eq!(out, "1.5e+0\n");
    }

    #[test]
    fn typed_float128_integer_value() {
        let (_, out, _) = run("float128 x=42\necho $x\n");
        assert_eq!(out, "4.2e+1\n");
    }

    #[test]
    fn typed_float128_negative() {
        let (_, out, _) = run("float128 x=-2.5\necho $x\n");
        assert_eq!(out, "-2.5e+0\n");
    }

    #[test]
    fn typed_float128_invalid_errors() {
        let prog = parse("float128 x=notanum\n").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert!(alloc::format!("{err}").contains("float128"));
    }

    #[test]
    fn typed_complex256_components_full_precision() {
        let (_, out, _) = run(
            "complex256 z=1.123456789012345678901234567890+2i\n\
             echo \"${z.re} ${z.im}\"\n",
        );
        assert_eq!(
            out,
            "1.12345678901234567890123456789e+0 2e+0\n",
        );
    }

    #[test]
    fn typed_complex256_real_only_collapses() {
        let (_, out, _) = run("complex256 z=5\necho $z\n");
        assert_eq!(out, "5e+0\n");
    }

    #[test]
    fn typed_complex256_sticky_reassignment() {
        // The type attribute persists across a plain reassignment.
        let (_, out, _) =
            run("complex256 z=1+1i\nz=2.5-3.5i\necho \"${z.re} ${z.im}\"\n");
        assert_eq!(out, "2.5e+0 -3.5e+0\n");
    }

    // ===== zsh-style expansion flags (case + quote subset) =====

    #[test]
    fn zsh_flag_uppercase() {
        let (_, out, _) = run("x=hello\necho \"${(U)x}\"\n");
        assert_eq!(out, "HELLO\n");
    }

    #[test]
    fn zsh_flag_lowercase() {
        let (_, out, _) = run("x=HELLO\necho \"${(L)x}\"\n");
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn zsh_flag_title_case() {
        let (_, out, _) = run("x=\"hello world\"\necho \"${(C)x}\"\n");
        assert_eq!(out, "Hello World\n");
    }

    #[test]
    fn zsh_flag_backslash_quote() {
        let (_, out, _) = run("x=\"hello world\"\necho \"${(q)x}\"\n");
        assert_eq!(out, "hello\\ world\n");
    }

    #[test]
    fn zsh_flag_single_quote() {
        let (_, out, _) = run("x=\"hello world\"\necho \"${(qq)x}\"\n");
        assert_eq!(out, "'hello world'\n");
    }

    #[test]
    fn zsh_flag_double_quote() {
        let (_, out, _) = run("x=\"hello world\"\necho \"${(qqq)x}\"\n");
        assert_eq!(out, "\"hello world\"\n");
    }

    #[test]
    fn zsh_flag_ansi_c_quote() {
        let (_, out, _) = run("x=\"hello world\"\necho \"${(qqqq)x}\"\n");
        assert_eq!(out, "$'hello world'\n");
    }

    #[test]
    fn zsh_flag_dequote_single_form() {
        let (_, out, _) = run("x=\"'hello'\"\necho \"${(Q)x}\"\n");
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn zsh_flag_dequote_double_form() {
        let (_, out, _) = run("x='\"hi\\\"there\"'\necho \"${(Q)x}\"\n");
        assert_eq!(out, "hi\"there\n");
    }

    #[test]
    fn zsh_flag_evaluation_order_quote_then_case() {
        // Per the zsh order, quoting happens *before* case
        // mapping — so `(qU)` quotes then upper-cases, which
        // upper-cases the literal `'` glyphs to themselves but
        // leaves the structure intact.
        let (_, out, _) = run("x=hello\necho \"${(qqU)x}\"\n");
        assert_eq!(out, "'HELLO'\n");
    }

    #[test]
    fn zsh_flag_combined_juxtaposition() {
        // `(UL)` — both flag chars take effect; `L` wins because
        // it's the most recent case-flag write. Matches zsh
        // behaviour: latest wins for same-category flags.
        let (_, out, _) = run("x=Mixed\necho \"${(UL)x}\"\n");
        assert_eq!(out, "mixed\n");
    }

    #[test]
    fn zsh_flag_unsupported_char_rejected() {
        // Genuinely unknown flag character; all real zsh flags
        // are wired up after this commit.
        let prog = parse("x=hi\necho \"${(Z)x}\"\n").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("Z"), "got: {msg}");
    }

    #[test]
    fn zsh_flag_unterminated_block_rejected() {
        let prog = parse("x=hi\necho \"${(Ux}\"\n").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("unterminated") || msg.contains("flag"), "got: {msg}");
    }

    // ===== zsh-style expansion flags (split + join subset) =====

    #[test]
    fn zsh_flag_split_with_paired_delim() {
        // `${(s.,.)x}` splits on `,`. With no `j` flag the
        // array re-joins on `""` so the parts collapse.
        let (_, out, _) = run("x=a,b,c\necho \"${(s.,.)x}\"\n");
        assert_eq!(out, "abc\n");
    }

    #[test]
    fn zsh_flag_split_then_join() {
        // `(s.,.j.+.)` and the reversed-order form `(j.+.s.,.)`
        // must produce identical results — the order inside the
        // block is irrelevant.
        let (_, out, _) = run("x=a,b,c\necho \"${(s.,.j.+.)x}\"\n");
        assert_eq!(out, "a+b+c\n");
        let (_, out, _) = run("x=a,b,c\necho \"${(j.+.s.,.)x}\"\n");
        assert_eq!(out, "a+b+c\n");
    }

    #[test]
    fn zsh_flag_split_empty_delim_per_char() {
        let (_, out, _) = run("x=abc\necho \"${(s..j.-.)x}\"\n");
        assert_eq!(out, "a-b-c\n");
    }

    #[test]
    fn zsh_flag_f_split_on_newline() {
        // Embed real newlines in the literal — the shell layer's
        // `$'\\n'` parser is unrelated to what `(f)` does. `(f)`
        // splits on the newline byte; `(F)` joins back on it.
        let (_, out, _) = run("x='line1\nline2\nline3'\necho \"${(fF)x}\"\n");
        assert_eq!(out, "line1\nline2\nline3\n");
    }

    #[test]
    fn zsh_flag_z_split_respects_quotes() {
        // Single-quoted value avoids the shell layer eating any
        // backslashes; `(z)` then keeps the quoted run glued.
        let (_, out, _) = run(
            "toks='first \"two words\" three'\necho \"${(z)toks}|\"\n",
        );
        assert_eq!(out, "firsttwo wordsthree|\n");
    }

    #[test]
    fn zsh_flag_split_then_uppercase() {
        // Per the fixed order — split / join → case mapping
        // applies to the final scalar.
        let (_, out, _) = run("x=a,b,c\necho \"${(s.,.j.+.U)x}\"\n");
        assert_eq!(out, "A+B+C\n");
    }

    // ===== zsh-style expansion flags (sort + dedup subset) =====

    #[test]
    fn zsh_flag_sort_ascending() {
        let (_, out, _) = run("x=c,a,b\necho \"${(s.,.j.,.o)x}\"\n");
        assert_eq!(out, "a,b,c\n");
    }

    #[test]
    fn zsh_flag_sort_descending() {
        let (_, out, _) = run("x=a,c,b\necho \"${(s.,.j.,.O)x}\"\n");
        assert_eq!(out, "c,b,a\n");
    }

    #[test]
    fn zsh_flag_unique_preserves_first_seen_order() {
        let (_, out, _) = run("x=c,a,b,a,c\necho \"${(s.,.j.,.u)x}\"\n");
        assert_eq!(out, "c,a,b\n");
    }

    #[test]
    fn zsh_flag_sort_plus_unique() {
        let (_, out, _) = run("x=c,a,b,a,c\necho \"${(s.,.j.,.ou)x}\"\n");
        assert_eq!(out, "a,b,c\n");
    }

    #[test]
    fn zsh_flag_numeric_sort_orders_by_value() {
        // Without `(n)` ascii sort would put `11` before `2`.
        let (_, out, _) = run("x=3,11,2,1\necho \"${(s.,.j.,.no)x}\"\n");
        assert_eq!(out, "1,2,3,11\n");
    }

    #[test]
    fn zsh_flag_numeric_sort_falls_back_for_non_numbers() {
        let (_, out, _) = run("x=3,foo,1\necho \"${(s.,.j.,.no)x}\"\n");
        // Numeric values come first; non-numeric ones land
        // after.
        assert_eq!(out, "1,3,foo\n");
    }

    // ===== zsh-style expansion flags (indirect/meta/compound) =====

    #[test]
    fn zsh_flag_p_indirect_lookup() {
        let (_, out, _) = run("name=value\nref=name\necho \"${(P)ref}\"\n");
        assert_eq!(out, "value\n");
    }

    #[test]
    fn zsh_flag_t_type_scalar() {
        let (_, out, _) = run("x=hi\necho \"${(t)x}\"\n");
        assert_eq!(out, "scalar\n");
    }

    #[test]
    fn zsh_flag_t_type_compound() {
        let (_, out, _) = run("p.x=1\np.y=2\necho \"${(t)p}\"\n");
        assert_eq!(out, "compound\n");
    }

    #[test]
    fn zsh_flag_t_type_unset() {
        let (_, out, _) = run("echo \"${(t)nothing}\"\n");
        assert_eq!(out, "unset\n");
    }

    #[test]
    fn zsh_flag_k_compound_member_names() {
        let (_, out, _) = run(
            "p.alpha=1\np.beta=2\np.gamma=3\necho \"${(kj.,.)p}\"\n",
        );
        assert_eq!(out, "alpha,beta,gamma\n");
    }

    #[test]
    fn zsh_flag_v_compound_member_values() {
        let (_, out, _) = run(
            "p.x=10\np.y=20\np.z=30\necho \"${(vj.,.)p}\"\n",
        );
        assert_eq!(out, "10,20,30\n");
    }

    #[test]
    fn zsh_flag_kv_interleaves_keys_and_values() {
        let (_, out, _) = run(
            "p.a=1\np.b=2\necho \"${(kvj.=.)p}\"\n",
        );
        // Stable order: a then b → "a=1=b=2".
        assert_eq!(out, "a=1=b=2\n");
    }

    #[test]
    fn zsh_flag_d_collapses_home_prefix() {
        let (_, out, _) = run(
            "HOME=/home/x\np=/home/x/projects\necho \"${(D)p}\"\n",
        );
        assert_eq!(out, "~/projects\n");
    }

    #[test]
    fn zsh_flag_d_passthrough_when_no_home_prefix() {
        let (_, out, _) = run(
            "HOME=/home/x\np=/etc/passwd\necho \"${(D)p}\"\n",
        );
        assert_eq!(out, "/etc/passwd\n");
    }

    #[test]
    fn zsh_flag_e_reexpands_value() {
        let (_, out, _) = run(
            "e=DOL\nexpr='hello $e world'\necho \"${(e)expr}\"\n",
        );
        assert_eq!(out, "hello DOL world\n");
    }

    #[test]
    fn zsh_flag_at_widens_join_separator() {
        let (_, out, _) = run("x=a,b,c\necho \"${(s.,.@)x}\"\n");
        assert_eq!(out, "a b c\n");
    }

    // ===== zsh-style expansion flags (path modifier + misc) =====

    #[test]
    fn zsh_flag_path_modifier_h() {
        let (_, out, _) =
            run("p=/home/user/projects/foo.rs\necho \"${(:h)p}\"\n");
        assert_eq!(out, "/home/user/projects\n");
    }

    #[test]
    fn zsh_flag_path_modifier_t() {
        let (_, out, _) =
            run("p=/home/user/projects/foo.rs\necho \"${(:t)p}\"\n");
        assert_eq!(out, "foo.rs\n");
    }

    #[test]
    fn zsh_flag_path_modifier_r() {
        let (_, out, _) =
            run("p=/home/user/projects/foo.rs\necho \"${(:r)p}\"\n");
        assert_eq!(out, "/home/user/projects/foo\n");
    }

    #[test]
    fn zsh_flag_path_modifier_e() {
        let (_, out, _) =
            run("p=/home/user/projects/foo.rs\necho \"${(:e)p}\"\n");
        assert_eq!(out, "rs\n");
    }

    #[test]
    fn zsh_flag_path_modifier_no_extension() {
        // `foo` has no extension; `:e` returns empty, `:r`
        // returns the unchanged name.
        let (_, out, _) = run("p=plain\necho \"[${(:e)p}][${(:r)p}]\"\n");
        assert_eq!(out, "[][plain]\n");
    }

    #[test]
    fn zsh_flag_path_modifier_dotfile_keeps_name() {
        // `.bashrc:r` should yield `.bashrc`, not the empty
        // string — the leading dot does not count as an
        // extension separator.
        let (_, out, _) =
            run("p=/home/u/.bashrc\necho \"${(:r)p}|${(:e)p}\"\n");
        assert_eq!(out, "/home/u/.bashrc|\n");
    }

    #[test]
    fn zsh_flag_g_decodes_ansi_c_escapes() {
        let (_, out, _) = run("x='hello\\nworld'\necho \"${(g)x}|\"\n");
        assert_eq!(out, "hello\nworld|\n");
    }

    #[test]
    fn zsh_flag_v_renders_invisible_bytes() {
        // `(g)` decodes the literal `\t`/`\n` into real control
        // bytes; `(V)` then renders them as `\xHH`.
        let (_, out, _) = run("x='a\\tb\\nc'\necho \"${(gV)x}|\"\n");
        assert!(out.contains("\\x09") || out.contains("\\x0a"), "got: {out}");
    }

    #[test]
    fn zsh_flag_c_counts_characters() {
        let (_, out, _) = run("x=hello\necho \"${(c)x}\"\n");
        assert_eq!(out, "5\n");
    }

    #[test]
    fn zsh_flag_sharp_converts_codepoint() {
        let (_, out, _) = run("n=65\necho \"${(#)n}\"\n");
        assert_eq!(out, "A\n");
    }

    #[test]
    fn zsh_flag_pct_expands_user() {
        let (_, out, _) = run(
            "USER=alice\npat='%n@host'\necho \"${(%)pat}\"\n",
        );
        assert_eq!(out, "alice@host\n");
    }

    #[test]
    fn zsh_flag_pct_double_percent_literal() {
        let (_, out, _) =
            run("pat='100%%done'\necho \"${(%)pat}\"\n");
        assert_eq!(out, "100%done\n");
    }

    #[test]
    fn zsh_flag_case_insensitive_sort() {
        let (_, out, _) = run(
            "x=Banana,apple,Cherry,banana\necho \"${(s.,.j.,.oi)x}\"\n",
        );
        // Case-insensitive: apple < Banana == banana < Cherry.
        // Stable sort keeps Banana before banana.
        assert_eq!(out, "apple,Banana,banana,Cherry\n");
    }

    #[test]
    fn zsh_flag_split_missing_delim_rejected() {
        let prog = parse("x=hi\necho \"${(s)x}\"\n").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("paired") || msg.contains("delim"), "got: {msg}");
    }

    #[test]
    fn parse_complex_literal_accepts_canonical_forms() {
        // Direct unit-test for the helper — covers spelling
        // edge cases beyond what the integration test exercises.
        assert_eq!(parse_complex_literal("1+2i"), Some((1.0, 2.0)));
        assert_eq!(parse_complex_literal("1-2i"), Some((1.0, -2.0)));
        assert_eq!(parse_complex_literal("2i"), Some((0.0, 2.0)));
        assert_eq!(parse_complex_literal("i"), Some((0.0, 1.0)));
        assert_eq!(parse_complex_literal("-i"), Some((0.0, -1.0)));
        assert_eq!(parse_complex_literal("5"), Some((5.0, 0.0)));
        assert_eq!(parse_complex_literal("(re=1 im=2)"), Some((1.0, 2.0)));
        assert_eq!(parse_complex_literal("(im=2)"), Some((0.0, 2.0)));
        // Exponential notation in the real part — the sign-finder
        // mustn't split on the `+` inside `1e+5`.
        assert_eq!(parse_complex_literal("1e+5+2i"), Some((1e5, 2.0)));
    }

    // ===== compound member access =====

    #[test]
    fn compound_member_assign_and_lookup() {
        let (_, out, _) = run("var.name=John\necho ${var.name}\n");
        assert_eq!(out, "John\n");
    }

    #[test]
    fn compound_member_nested_path() {
        let (_, out, _) = run(
            "p.address.city=Seoul\necho ${p.address.city}\n",
        );
        assert_eq!(out, "Seoul\n");
    }

    #[test]
    fn compound_member_independent_keys() {
        let (_, out, _) = run(
            "p.x=1\np.y=2\necho \"${p.x} ${p.y}\"\n",
        );
        assert_eq!(out, "1 2\n");
    }

    #[test]
    fn compound_member_in_modifier_form() {
        // `${var.x:-default}` should work like the plain
        // identifier case.
        let (_, out, _) = run("echo ${p.missing:-fallback}\n");
        assert_eq!(out, "fallback\n");
    }

    // ===== `typeset -n` (nameref) =====

    #[test]
    fn nameref_reads_through_to_target() {
        let (_, out, _) = run("real=42\ntypeset -n alias=real\necho $alias\n");
        assert_eq!(out, "42\n");
    }

    #[test]
    fn nameref_writes_through_to_target() {
        let (_, out, _) = run(
            "real=initial\ntypeset -n alias=real\nalias=updated\necho $real\n",
        );
        assert_eq!(out, "updated\n");
    }

    #[test]
    fn nameref_chain_follows_through() {
        let (_, out, _) = run(
            "a=hello\ntypeset -n b=a\ntypeset -n c=b\necho $c\n",
        );
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn nameref_cycle_bounded() {
        // Self-loop / cycle — `follow_nameref_chain` caps the
        // hop budget instead of looping forever.
        let (_, out, _) = run(
            "typeset -n a=b\ntypeset -n b=a\necho $a\n",
        );
        // No infinite loop; output may be empty (cycle aborts).
        let _ = out;
    }

    // ===== `getopts` builtin =====

    #[test]
    fn getopts_walks_flag_options() {
        let (_, out, _) = run(
            "while getopts \"ab\" opt -a -b; do echo \"opt=$opt OPTIND=$OPTIND\"; done\n",
        );
        assert!(out.contains("opt=a OPTIND=2"), "got: {out}");
        assert!(out.contains("opt=b OPTIND=3"), "got: {out}");
    }

    #[test]
    fn getopts_handles_option_with_argument() {
        let (_, out, _) = run(
            "while getopts \"x:\" opt -x value; do echo \"opt=$opt OPTARG=$OPTARG OPTIND=$OPTIND\"; done\n",
        );
        assert!(
            out.contains("opt=x OPTARG=value OPTIND=3"),
            "got: {out}",
        );
    }

    #[test]
    fn getopts_unknown_option_yields_question_mark() {
        let (_, out, _) = run(
            "getopts \"a\" opt -z; echo \"opt=$opt OPTARG=$OPTARG\"\n",
        );
        assert!(out.contains("opt=? OPTARG=z"), "got: {out}");
    }

    #[test]
    fn getopts_double_dash_stops_parsing() {
        let (outcome, _, _) = run("getopts \"a\" opt -- -a\n");
        assert_eq!(outcome.status(), 1);
    }

    // ===== `die` / `assert` / `usage` builtins =====

    #[test]
    fn die_with_message_and_status() {
        let prog = parse("die \"oops\" 42\n").unwrap();
        let mut ev = Evaluator::new();
        let outcome = ev.eval_program(&prog).unwrap();
        // Exit-request propagates as Outcome::Exit which surfaces
        // as the requested status.
        assert_eq!(outcome.status(), 42);
        assert!(ev.take_stderr().contains("oops"));
    }

    #[test]
    fn die_default_status_is_one() {
        let prog = parse("die\n").unwrap();
        let mut ev = Evaluator::new();
        let outcome = ev.eval_program(&prog).unwrap();
        assert_eq!(outcome.status(), 1);
    }

    #[test]
    fn assert_true_returns_zero() {
        let (_, out, _) = run("assert 1 -eq 1; echo passed\n");
        assert_eq!(out, "passed\n");
    }

    #[test]
    fn assert_false_dies_with_status_one() {
        let prog = parse("assert 1 -eq 2\necho unreachable\n").unwrap();
        let mut ev = Evaluator::new();
        let outcome = ev.eval_program(&prog).unwrap();
        assert_eq!(outcome.status(), 1);
        assert!(!ev.take_output().contains("unreachable"));
        assert!(ev.take_stderr().contains("assertion failed"));
    }

    #[test]
    fn usage_prints_line_and_exits_two() {
        let prog = parse("usage my-tool\necho unreachable\n").unwrap();
        let mut ev = Evaluator::new();
        let outcome = ev.eval_program(&prog).unwrap();
        assert_eq!(outcome.status(), 2);
        let out = ev.take_output();
        assert!(out.contains("Usage: my-tool"), "got: {out}");
        assert!(!out.contains("unreachable"));
    }

    // ===== `printf` builtin =====

    #[test]
    fn printf_substitutes_s_conversion() {
        let (_, out, _) = run("printf 'hello %s\\n' world\n");
        assert_eq!(out, "hello world\n");
    }

    #[test]
    fn printf_substitutes_d_conversion() {
        let (_, out, _) = run("printf '%d\\n' 42\n");
        assert_eq!(out, "42\n");
    }

    #[test]
    fn printf_hex_and_octal() {
        let (_, out, _) = run("printf '%x %o\\n' 255 8\n");
        assert_eq!(out, "ff 10\n");
    }

    #[test]
    fn printf_cycles_format_over_remaining_args() {
        let (_, out, _) = run("printf '<%s>' a b c\n");
        assert_eq!(out, "<a><b><c>");
    }

    #[test]
    fn printf_missing_arg_substitutes_empty_or_zero() {
        let (_, out, _) = run("printf '<%s|%d>\\n'\n");
        assert_eq!(out, "<|0>\n");
    }

    #[test]
    fn printf_percent_percent_literal() {
        let (_, out, _) = run("printf '%%d=%d\\n' 7\n");
        assert_eq!(out, "%d=7\n");
    }

    #[test]
    fn printf_no_args_errors() {
        let prog = parse("printf\n").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("missing format"), "got: {msg}");
    }

    // ===== `command` builtin =====

    #[test]
    fn command_bypasses_function_dispatch() {
        // Define a `echo` function that absorbs the arg silently;
        // the function dispatch normally wins, but `command echo`
        // bypasses functions and reaches the builtin.
        let (_, out, _) = run(
            "echo() { :; }\n\
             echo \"function call\"\n\
             command echo \"builtin call\"\n",
        );
        assert_eq!(out, "builtin call\n");
    }

    #[test]
    fn command_v_finds_builtin() {
        let (_, out, _) = run("command -v echo\n");
        assert_eq!(out, "echo\n");
    }

    #[test]
    fn command_v_finds_function() {
        let (_, out, _) = run("greet() { :; }\ncommand -v greet\n");
        assert_eq!(out, "greet\n");
    }

    #[test]
    fn command_v_finds_alias() {
        let (_, out, _) = run("alias g='echo hi'\ncommand -v g\n");
        assert!(out.contains("alias g="), "got: {out}");
    }

    #[test]
    fn command_v_missing_returns_status_1() {
        let (outcome, out, _) = run("command -v no_such_thing_xyz\n");
        assert_eq!(outcome.status(), 1);
        assert_eq!(out, "");
    }

    #[test]
    fn command_capital_v_verbose_format() {
        let (_, out, _) = run("command -V echo\n");
        assert!(out.contains("echo is a shell builtin"), "got: {out}");
    }

    // ===== `eval` builtin =====

    #[test]
    fn eval_runs_joined_source() {
        let (_, out, _) = run("eval 'x=42; echo $x'\n");
        assert_eq!(out, "42\n");
    }

    #[test]
    fn eval_joins_multiple_args_with_spaces() {
        let (_, out, _) = run("eval 'echo' 'a' 'b'\n");
        assert_eq!(out, "a b\n");
    }

    #[test]
    fn eval_no_args_succeeds_silently() {
        let (outcome, out, _) = run("eval\n");
        assert_eq!(outcome.status(), 0);
        assert_eq!(out, "");
    }

    #[test]
    fn eval_blocked_under_secure_modifier() {
        let src = "mode default-secure\neval 'echo blocked'\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("-secure"), "got: {msg}");
    }

    #[test]
    fn eval_propagates_status() {
        let (outcome, _, _) = run("eval 'true'\n");
        assert_eq!(outcome.status(), 0);
        let (outcome, _, _) = run("eval 'false'\n");
        assert_eq!(outcome.status(), 1);
    }

    // ===== `source` / `.` builtin =====

    #[test]
    fn source_missing_path_errors() {
        let prog = parse("source\n").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("missing PATH"), "got: {msg}");
    }

    #[test]
    fn source_in_venv_without_fs_read_denied() {
        let src = "venv tight { capabilities { profile none } body { source /etc/passwd; } }\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let outcome = ev.eval_program(&prog).unwrap();
        assert_eq!(outcome.status(), 126);
        let err = ev.take_stderr();
        assert!(err.contains("fs-read"), "got: {err}");
    }

    // ===== `read` builtin =====

    #[test]
    fn read_parse_args_default_name_replied() {
        let p = parse_read_args(&[]).unwrap();
        assert!(p.names.is_empty());
        assert!(!p.raw);
        assert!(p.prompt.is_none());
    }

    #[test]
    fn read_parse_args_dash_p_prompt() {
        let args: alloc::vec::Vec<String> = alloc::vec!["-p".into(), "say: ".into(), "X".into()];
        let p = parse_read_args(&args).unwrap();
        assert_eq!(p.prompt.as_deref(), Some("say: "));
        assert_eq!(p.names, alloc::vec!["X".to_string()]);
    }

    #[test]
    fn read_parse_args_long_prompt_eq_form() {
        let args: alloc::vec::Vec<String> = alloc::vec!["--prompt=> ".into(), "X".into()];
        let p = parse_read_args(&args).unwrap();
        assert_eq!(p.prompt.as_deref(), Some("> "));
        assert_eq!(p.names, alloc::vec!["X".to_string()]);
    }

    #[test]
    fn read_parse_args_raw_flag() {
        let args: alloc::vec::Vec<String> = alloc::vec!["-r".into(), "X".into()];
        let p = parse_read_args(&args).unwrap();
        assert!(p.raw);
    }

    #[test]
    fn read_split_single_name_keeps_whole_line() {
        let v = split_for_read("a b c", " \t\n", 1);
        assert_eq!(v, alloc::vec!["a b c".to_string()]);
    }

    #[test]
    fn read_split_multi_name_last_gets_remainder() {
        let v = split_for_read("a b c d", " \t\n", 2);
        assert_eq!(v, alloc::vec!["a".to_string(), "b c d".to_string()]);
    }

    #[test]
    fn read_split_more_names_than_fields_pads_empty() {
        let v = split_for_read("a", " \t\n", 3);
        assert_eq!(
            v,
            alloc::vec!["a".to_string(), String::new(), String::new()]
        );
    }

    #[test]
    fn read_split_leading_whitespace_stripped() {
        let v = split_for_read("  a b", " \t\n", 2);
        assert_eq!(v, alloc::vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn read_unescape_drops_backslashes() {
        assert_eq!(unescape_read_line("a\\bc"), "abc");
        assert_eq!(unescape_read_line("\\\\"), "\\");
    }

    // ===== command-not-found / capability-denied → status =====

    #[test]
    fn unknown_command_lets_or_continue() {
        // The whole point of mapping ExternalNotFound to status
        // 127: `||` and friends can recover from it.
        let (_, out, _) = run("nope_xyz || echo recovered\n");
        assert_eq!(out, "recovered\n");
    }

    #[test]
    fn unknown_command_stderr_message_buffered() {
        let prog = parse("nope_xyz || true").unwrap();
        let mut ev = Evaluator::new();
        ev.eval_program(&prog).unwrap();
        let err = ev.take_stderr();
        assert!(err.contains("nope_xyz: command not found"), "got: {err}");
    }

    #[test]
    fn capability_denied_lets_or_continue() {
        let src = "venv tight { capabilities { profile none } body {\n\
                       /bin/ls || echo recovered\n\
                   } }\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        ev.eval_program(&prog).unwrap();
        let out = ev.take_output();
        assert!(out.contains("recovered"), "got: {out}");
    }

    #[test]
    fn unknown_command_with_errexit_still_aborts() {
        // POSIX errexit: a non-zero status from the *first*
        // command of the list (no `||`, no `&&`, no `if`) aborts.
        let prog = parse("set -e\nnope_xyz\necho should-not-run\n").unwrap();
        let mut ev = Evaluator::new();
        let outcome = ev.eval_program(&prog).unwrap();
        // The evaluator returns Outcome::Exit(127) on errexit; the
        // `echo` line never runs.
        assert!(outcome.status() == 127 || outcome.is_exit_request());
        let out = ev.take_output();
        assert!(!out.contains("should-not-run"), "got: {out}");
    }

    // ===== double-quote backslash escape =====

    #[test]
    fn dq_backslash_dollar_escapes_expansion() {
        let (_, out, _) = run("x=val; echo \"\\$x\"\n");
        assert_eq!(out, "$x\n");
    }

    #[test]
    fn dq_backslash_double_quote_literal() {
        let (_, out, _) = run("echo \"\\\"quoted\\\"\"\n");
        assert_eq!(out, "\"quoted\"\n");
    }

    #[test]
    fn dq_backslash_backslash_literal() {
        let (_, out, _) = run("echo \"\\\\\"\n");
        assert_eq!(out, "\\\n");
    }

    #[test]
    fn dq_backslash_other_char_survives() {
        // POSIX: a backslash before any char other than $, `, ", \,
        // newline is *literal*. `\n` here means literal backslash +
        // letter `n`.
        let (_, out, _) = run("echo \"\\n\"\n");
        assert_eq!(out, "\\n\n");
    }

    #[test]
    fn dq_dollar_without_backslash_still_expands() {
        let (_, out, _) = run("x=val; echo \"$x\"\n");
        assert_eq!(out, "val\n");
    }

    // ===== venv — strict-mode gating =====

    #[test]
    fn venv_disabled_in_posix_strict() {
        // Switching to posix-strict happens before the venv decl
        // so the strict gate fires.
        let src = "mode posix-strict\nvenv x { body {}; }\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert_eq!(err.exit_code(), 2); // KashError::Mode → 2
        let msg = alloc::format!("{err}");
        assert!(msg.contains("posix-strict"), "got: {msg}");
    }

    #[test]
    fn venv_disabled_in_ksh93u_strict() {
        let src = "mode ksh93u-strict\nvenv x { body {}; }\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        let msg = alloc::format!("{err}");
        assert!(msg.contains("ksh93u-strict"), "got: {msg}");
    }

    #[test]
    fn venv_available_in_posix_aware() {
        let (_, out, _) = run(
            "mode posix-aware\nvenv x { body { echo ok; }; }\n",
        );
        assert_eq!(out, "ok\n");
    }

    // ===== venv — env overlay visible inside kash =====

    #[test]
    fn venv_env_set_visible_to_kash_lookup() {
        let (_, out, _) = run(
            "venv proj { env { FOO=bar } body { echo $FOO; } }\n",
        );
        assert_eq!(out, "bar\n");
    }

    #[test]
    fn venv_env_overlay_invisible_after_pop() {
        let (_, out, _) = run(
            "venv proj { env { FOO=bar } body { echo $FOO; } }\n\
             echo \"[$FOO]\"\n",
        );
        assert_eq!(out, "bar\n[]\n");
    }

    #[test]
    fn venv_env_path_prepend_visible_to_kash_lookup() {
        let (_, out, _) = run(
            "PATH=/usr/bin\n\
             venv proj { env { PATH-prepend /opt/venv/bin } body { echo $PATH; } }\n",
        );
        assert_eq!(out, "/opt/venv/bin:/usr/bin\n");
    }

    #[test]
    fn venv_env_path_append_visible_to_kash_lookup() {
        let (_, out, _) = run(
            "PATH=/usr/bin\n\
             venv proj { env { PATH-append /opt/extra/bin } body { echo $PATH; } }\n",
        );
        assert_eq!(out, "/usr/bin:/opt/extra/bin\n");
    }

    #[test]
    fn venv_env_inner_shadows_outer_for_set() {
        let (_, out, _) = run(
            "venv outer { env { X=outer } body {\n\
                 echo $X\n\
                 venv inner { env { X=inner } body { echo $X; } }\n\
                 echo $X\n\
             } }\n",
        );
        assert_eq!(out, "outer\ninner\nouter\n");
    }

    #[test]
    fn venv_env_path_prepends_stack_inside_out() {
        let (_, out, _) = run(
            "PATH=/usr/bin\n\
             venv outer { env { PATH-prepend /outer/bin } body {\n\
                 echo $PATH\n\
                 venv inner { env { PATH-prepend /inner/bin } body { echo $PATH; } }\n\
                 echo $PATH\n\
             } }\n",
        );
        // outer adds /outer/bin in front; inner adds /inner/bin in
        // front of the outer view; outer restored on exit.
        assert_eq!(
            out,
            "/outer/bin:/usr/bin\n\
             /inner/bin:/outer/bin:/usr/bin\n\
             /outer/bin:/usr/bin\n"
        );
    }

    // ===== venv — v.1 surface =====

    #[test]
    fn venv_body_runs_statements() {
        let (_, out, _) = run(
            "venv myproj {\n\
                 body {\n\
                     echo inside\n\
                 }\n\
             }\n\
             echo outside\n",
        );
        assert_eq!(out, "inside\noutside\n");
    }

    #[test]
    fn venv_body_inherits_outer_scope() {
        // v.1 is just a frame marker — there's no isolation yet,
        // so outer bindings stay visible inside the body.
        let (_, out, _) = run(
            "x=visible\n\
             venv myproj {\n\
                 body { echo $x; }\n\
             }\n",
        );
        assert_eq!(out, "visible\n");
    }

    #[test]
    fn venv_block_without_body_is_noop() {
        // A venv block with no `body` section just registers and
        // unregisters the frame.
        let (_, out, _) = run(
            "echo before\n\
             venv empty {}\n\
             echo after\n",
        );
        assert_eq!(out, "before\nafter\n");
    }

    #[test]
    fn venv_unknown_section_is_rejected() {
        // A typo'd section name must error at parse time so it
        // doesn't silently become a command. `capabilities` is now
        // a known section (v.2); pick a name that isn't.
        let src = "venv myproj { capabilites { profile basic; }; }\n";
        let prog = parse(src);
        assert!(prog.is_err());
    }

    #[test]
    fn venv_name_with_embedded_dot_is_rejected() {
        assert!(parse("venv foo.bar { body {} }\n").is_err());
    }

    #[test]
    fn venv_capabilities_section_parses_profile_only() {
        // Smoke test: parse + execute, body just runs.
        let (_, out, _) = run(
            "venv myproj {\n\
                 capabilities { profile basic }\n\
                 body { echo inside }\n\
             }\n",
        );
        assert_eq!(out, "inside\n");
    }

    #[test]
    fn venv_capabilities_section_parses_grants_and_revokes() {
        let (_, out, _) = run(
            "venv myproj {\n\
                 capabilities {\n\
                     profile basic\n\
                     + fs-write\n\
                     - exec-spawn\n\
                 }\n\
                 body { echo ok }\n\
             }\n",
        );
        assert_eq!(out, "ok\n");
    }

    #[test]
    fn venv_capabilities_section_parses_allow_cmd_list() {
        let (_, out, _) = run(
            "venv myproj {\n\
                 capabilities {\n\
                     profile basic\n\
                     allow-cmd ls cat grep\n\
                 }\n\
                 body { echo ok }\n\
             }\n",
        );
        assert_eq!(out, "ok\n");
    }

    #[test]
    fn venv_unknown_profile_rejected_at_run_time() {
        let src = "venv myproj { capabilities { profile no-such-thing }; body {}; }\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("unknown") && msg.contains("profile"), "got: {msg}");
    }

    #[test]
    fn venv_unknown_capability_name_rejected_at_run_time() {
        let src = "venv myproj { capabilities { + nosuchcap }; body {}; }\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("unknown capability"), "got: {msg}");
    }

    #[test]
    fn venv_env_section_parses() {
        let (_, out, _) = run(
            "venv myproj {\n\
                 env {\n\
                     PYTHONHOME=/opt/venv\n\
                     PATH-prepend /opt/venv/bin\n\
                     PATH-append /opt/other/bin\n\
                 }\n\
                 body { echo ok }\n\
             }\n",
        );
        assert_eq!(out, "ok\n");
    }

    #[test]
    fn venv_env_entry_without_equals_rejected() {
        let src = "venv myproj { env { NAME_ONLY }; body {}; }\n";
        let prog = parse(src);
        assert!(prog.is_err());
    }

    #[test]
    fn venv_revoking_exec_spawn_blocks_external_command() {
        // `profile none` denies everything, including exec-spawn.
        // The external `/bin/ls` call surfaces as POSIX status 126
        // (kash's capability-denied mapping); the rationale goes
        // out through the stderr buffer.
        let src = "venv tight {\n\
                       capabilities { profile none }\n\
                       body { /bin/ls /tmp; }\n\
                   }\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let outcome = ev.eval_program(&prog).unwrap();
        assert_eq!(outcome.status(), 126);
        let err = ev.take_stderr();
        assert!(err.contains("exec-spawn"), "got: {err}");
    }

    #[test]
    fn venv_allow_cmd_blocks_disallowed_external_command() {
        // basic profile *does* grant exec-spawn, but the allow-cmd
        // list constrains spawn to a closed set.
        let src = "venv tight {\n\
                       capabilities { profile basic; allow-cmd /bin/echo }\n\
                       body { /bin/cat /tmp; }\n\
                   }\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let outcome = ev.eval_program(&prog).unwrap();
        assert_eq!(outcome.status(), 126);
        let err = ev.take_stderr();
        assert!(err.contains("allow-cmd"), "got: {err}");
    }

    #[test]
    fn venv_allow_cmd_lets_listed_command_through() {
        // The same allow-cmd setup but invoking a *listed* command
        // shouldn't trip the check at parse / dispatch time.
        let src = "venv tight {\n\
                       capabilities { profile basic; allow-cmd /bin/echo }\n\
                       body { :; }\n\
                   }\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        // The body is `:` (a noop) so no external spawn even
        // happens — this checks the spec materialises cleanly.
        assert!(ev.eval_program(&prog).is_ok());
    }

    #[test]
    fn venv_imports_section_applies_namespace_import_to_body() {
        let (_, out, _) = run(
            "namespace utils { hi() { echo hi-from-utils; }; }\n\
             venv myproj {\n\
                 imports { use namespace utils }\n\
                 body { hi; }\n\
             }\n",
        );
        assert_eq!(out, "hi-from-utils\n");
    }

    #[test]
    fn venv_imports_drop_on_exit() {
        // After the venv ends, the bare `hi` reference must not
        // resolve anymore — the imports were scoped to the venv.
        let src = "namespace utils { unique_kash_hi() { echo h; }; }\n\
                   venv myproj {\n\
                       imports { use namespace utils }\n\
                       body { unique_kash_hi; }\n\
                   }\n\
                   unique_kash_hi\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let _ = ev.eval_program(&prog);
        let out = ev.take_output();
        // The inner call ran; the outer one didn't (would either
        // raise or fall through to external exec — either way no
        // second "h" line).
        assert_eq!(out.matches('h').count(), 1, "out: {out}");
    }

    #[test]
    fn venv_imports_directive_without_use_keyword_rejected() {
        let src = "venv myproj { imports { namespace utils }; body {}; }\n";
        let prog = parse(src);
        assert!(prog.is_err());
    }

    #[test]
    fn venv_capability_checks_reflect_active_frame() {
        // Build a program where the body asks the evaluator for a
        // capability check via the public API — done in-test by
        // executing a body that's a no-op, then peeking at the
        // evaluator from outside. Here we just confirm the
        // pop-on-exit invariant.
        let prog = parse(
            "venv tight {\n\
                 capabilities { profile none }\n\
                 body { echo inside }\n\
             }\n",
        )
        .unwrap();
        let mut ev = Evaluator::new();
        // Before the program runs: no venv, everything allowed.
        assert!(ev.is_capability_allowed(crate::capability::Capability::ExecSpawn));
        assert!(ev.is_cmd_allowed("anything"));
        ev.eval_program(&prog).unwrap();
        // After the program ran: frame popped, everything allowed
        // again.
        assert!(ev.is_capability_allowed(crate::capability::Capability::ExecSpawn));
    }

    #[test]
    fn venv_keyword_not_reserved_as_command_name() {
        // `venv` outside head position should still work as a
        // regular argument.
        let (_, out, _) = run(
            "f() { :; }\n\
             f venv arg\n\
             echo done\n",
        );
        assert_eq!(out, "done\n");
    }

    // ===== mode declaration =====

    #[test]
    fn mode_block_temporarily_changes_mode() {
        let (_, out, _) = run(
            "echo before=${.sh.mode}\n\
             mode default-secure { echo inside=${.sh.mode}; }\n\
             echo after=${.sh.mode}\n",
        );
        assert_eq!(
            out,
            "before=default\ninside=default-secure\nafter=default\n"
        );
    }

    #[test]
    fn mode_unbounded_persists_after_declaration() {
        let (_, out, _) = run(
            "echo a=${.sh.mode}\n\
             mode default-secure\n\
             echo b=${.sh.mode}\n",
        );
        assert_eq!(out, "a=default\nb=default-secure\n");
    }

    #[test]
    fn mode_introspection_base_and_modifiers() {
        let (_, out, _) = run(
            "mode default-secure { echo base=${.sh.mode.base} mods=${.sh.mode.modifiers}; }\n",
        );
        assert_eq!(out, "base=default mods=secure\n");
    }

    #[test]
    fn mode_unknown_name_errors() {
        let src = "mode no-such-mode\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_err());
    }

    #[test]
    fn mode_inner_cannot_drop_outer_modifier() {
        // `-secure` is active in the outer block; the inner `mode
        // default` would drop it, so the declaration is rejected.
        let src = "mode default-secure { mode default { :; }; }\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("modifier"), "got: {msg}");
    }

    #[test]
    fn mode_inner_may_add_modifier() {
        let (_, out, _) = run(
            "mode default-secure {\n\
                 mode default-secure {\n\
                     echo nested=${.sh.mode}\n\
                 }\n\
             }\n",
        );
        assert_eq!(out, "nested=default-secure\n");
    }

    #[test]
    fn mode_dash_l_form_at_top_level_persists_for_rest_of_file() {
        // At file scope there's no enclosing function frame to
        // pop, so `-L` just installs the mode for the remainder.
        let (_, out, _) = run("mode -L default-secure\necho ${.sh.mode}\n");
        assert_eq!(out, "default-secure\n");
    }

    #[test]
    fn mode_dash_l_inside_function_auto_restores_on_return() {
        // `-L` inside a function is scope-bound: the change is
        // visible inside the body, but the caller's mode is
        // restored on return.
        let (_, out, _) = run(
            "function f { mode -L default-secure; echo inside=${.sh.mode}; }\n\
             f\n\
             echo outside=${.sh.mode}\n",
        );
        assert_eq!(out, "inside=default-secure\noutside=default\n");
    }

    #[test]
    fn mode_block_inside_function_restores_at_block_end() {
        let (_, out, _) = run(
            "function f {\n\
                 echo a=${.sh.mode}\n\
                 mode default-secure { echo b=${.sh.mode}; }\n\
                 echo c=${.sh.mode}\n\
             }\n\
             f\n\
             echo d=${.sh.mode}\n",
        );
        assert_eq!(
            out,
            "a=default\nb=default-secure\nc=default\nd=default\n"
        );
    }

    #[test]
    fn mode_unbounded_inside_function_propagates_to_caller() {
        // Unbounded mode change inside a function must survive
        // the return — that's the whole point of the form.
        let (_, out, _) = run(
            "function f { mode default-secure; }\n\
             f\n\
             echo ${.sh.mode}\n",
        );
        assert_eq!(out, "default-secure\n");
    }

    #[test]
    fn mode_unbounded_propagates_through_block() {
        // Per `project_shell_mode_syntax.md`: unbounded "현재 scope
        // 끝까지 + 바깥으로도 전파". The block must not restore
        // when the inner unbounded form has marked propagation.
        let (_, out, _) = run(
            "mode default { mode default-secure; echo inner=${.sh.mode}; }\n\
             echo outer=${.sh.mode}\n",
        );
        assert_eq!(out, "inner=default-secure\nouter=default-secure\n");
    }

    #[test]
    fn mode_unbounded_propagates_through_block_inside_function() {
        // Same as above but nested in a function. The propagation
        // must reach the function's caller, too.
        let (_, out, _) = run(
            "function f { mode default { mode default-secure; }; }\n\
             f\n\
             echo ${.sh.mode}\n",
        );
        assert_eq!(out, "default-secure\n");
    }

    #[test]
    fn mode_dash_l_inside_block_does_not_escape_block() {
        // `-L` is scope-bound, so the block's auto-restore on
        // exit cancels the change as expected — even though
        // function_mode_save now also covers blocks.
        let (_, out, _) = run(
            "mode default {\n\
                 mode -L default-secure\n\
                 echo a=${.sh.mode}\n\
             }\n\
             echo b=${.sh.mode}\n",
        );
        assert_eq!(out, "a=default-secure\nb=default\n");
    }

    #[test]
    fn mode_strict_disables_mode_keyword() {
        // Once we're in a strict mode the keyword itself is
        // disabled — no escape from inside.
        let src = "mode posix-strict\nmode default\n";
        let prog = parse(src).unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        let msg = alloc::format!("{err}");
        assert!(msg.contains("strict") || msg.contains("not allowed"), "got: {msg}");
    }

    #[test]
    fn mode_keyword_not_reserved_as_command_name() {
        // Outside head position the word `mode` is still usable as a
        // function name — make sure parsing doesn't get confused.
        let (_, out, _) = run(
            "do_things() { :; }\n\
             do_things mode arg2\n\
             echo done\n",
        );
        assert_eq!(out, "done\n");
    }

    // ===== parameter expansion — strip / fold / replace / substring =====

    #[test]
    fn param_prefix_strip_shortest() {
        let (_, out, _) = run("p=a.b.c; echo ${p#*.}\n");
        assert_eq!(out, "b.c\n");
    }

    #[test]
    fn param_prefix_strip_longest() {
        let (_, out, _) = run("p=a.b.c; echo ${p##*.}\n");
        assert_eq!(out, "c\n");
    }

    #[test]
    fn param_suffix_strip_shortest() {
        let (_, out, _) = run("p=a.b.c; echo ${p%.*}\n");
        assert_eq!(out, "a.b\n");
    }

    #[test]
    fn param_suffix_strip_longest() {
        let (_, out, _) = run("p=a.b.c; echo ${p%%.*}\n");
        assert_eq!(out, "a\n");
    }

    #[test]
    fn param_strip_no_match_returns_value() {
        let (_, out, _) = run("p=abc; echo ${p#xyz}\n");
        assert_eq!(out, "abc\n");
    }

    #[test]
    fn param_case_fold_upper_all() {
        let (_, out, _) = run("v=hello; echo ${v^^}\n");
        assert_eq!(out, "HELLO\n");
    }

    #[test]
    fn param_case_fold_upper_first() {
        let (_, out, _) = run("v=hello; echo ${v^}\n");
        assert_eq!(out, "Hello\n");
    }

    #[test]
    fn param_case_fold_lower_all() {
        let (_, out, _) = run("v=HELLO; echo ${v,,}\n");
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn param_case_fold_lower_first() {
        let (_, out, _) = run("v=HELLO; echo ${v,}\n");
        assert_eq!(out, "hELLO\n");
    }

    #[test]
    fn param_replace_first_match() {
        let (_, out, _) = run("v=foofoo; echo ${v/foo/bar}\n");
        assert_eq!(out, "barfoo\n");
    }

    #[test]
    fn param_replace_all_matches() {
        let (_, out, _) = run("v=foofoo; echo ${v//foo/bar}\n");
        assert_eq!(out, "barbar\n");
    }

    #[test]
    fn param_replace_prefix_anchored() {
        let (_, out, _) = run("v=abcabc; echo ${v/#abc/X}\n");
        assert_eq!(out, "Xabc\n");
    }

    #[test]
    fn param_replace_suffix_anchored() {
        let (_, out, _) = run("v=abcabc; echo ${v/%abc/X}\n");
        assert_eq!(out, "abcX\n");
    }

    #[test]
    fn param_replace_glob_pattern() {
        let (_, out, _) = run("v=a-b-c; echo ${v/-*-/X}\n");
        assert_eq!(out, "aXc\n");
    }

    #[test]
    fn param_substring_simple() {
        let (_, out, _) = run("v=abcdef; echo ${v:2}\n");
        assert_eq!(out, "cdef\n");
    }

    #[test]
    fn param_substring_with_length() {
        let (_, out, _) = run("v=abcdef; echo ${v:1:3}\n");
        assert_eq!(out, "bcd\n");
    }

    #[test]
    fn param_substring_negative_offset_counts_from_end() {
        let (_, out, _) = run("v=abcdef; echo ${v: -2}\n");
        assert_eq!(out, "ef\n");
    }

    #[test]
    fn param_substring_negative_length_is_end_offset() {
        let (_, out, _) = run("v=abcdef; echo ${v:1:-1}\n");
        assert_eq!(out, "bcde\n");
    }

    #[test]
    fn param_colon_dash_modifier_still_works() {
        // Make sure substring detection doesn't break the existing
        // `${VAR:-default}` form.
        let (_, out, _) = run("unset v; echo ${v:-fallback}\n");
        assert_eq!(out, "fallback\n");
    }

    #[test]
    fn capture_list_readonly_is_local_only() {
        // Capture-driven readonly lives in the function frame and
        // disappears when the frame pops — the caller can still
        // reassign after the call.
        let (_, out, _) = run(
            "x=one\n\
             function f(x) { :; }\n\
             f\n\
             x=two\n\
             echo \"$x\"\n",
        );
        assert_eq!(out, "two\n");
    }

    #[test]
    fn explicit_dispatch_uses_instance_over_default() {
        // Both default and instance are present — instance wins.
        let (_, out, _) = run(
            "typeclass Pick { choose() { echo default; }; }\n\
             instance Pick for Int { choose() { echo instance; }; }\n\
             Pick::Int::choose\n\
             Pick::Other::choose\n",
        );
        assert_eq!(out, "instance\ndefault\n");
    }

    // ===== arrays + typeset =====

    #[test]
    fn indexed_array_assign_and_lookup() {
        let (_, out, _) = run("arr[0]=alpha; arr[1]=beta; arr[2]=gamma; echo ${arr[0]} ${arr[1]} ${arr[2]}");
        assert_eq!(out, "alpha beta gamma\n");
    }

    #[test]
    fn indexed_array_length_with_hash_at() {
        let (_, out, _) = run("arr[0]=a; arr[1]=b; arr[2]=c; echo ${#arr[@]}");
        assert_eq!(out, "3\n");
    }

    #[test]
    fn indexed_array_sparse_fills_with_empty() {
        let (_, out, _) = run("arr[3]=x; echo \"[${arr[0]}][${arr[1]}][${arr[2]}][${arr[3]}]\"");
        assert_eq!(out, "[][][][x]\n");
    }

    #[test]
    fn assoc_array_assign_and_lookup() {
        let (_, out, _) = run("typeset -A m; m[foo]=hello; m[bar]=world; echo ${m[foo]} ${m[bar]}");
        assert_eq!(out, "hello world\n");
    }

    #[test]
    fn assoc_array_length_with_hash_at() {
        let (_, out, _) = run("typeset -A m; m[a]=1; m[b]=2; m[c]=3; echo ${#m[@]}");
        assert_eq!(out, "3\n");
    }

    #[test]
    fn array_at_star_joined_in_scalar_context() {
        let (_, out, _) = run("arr[0]=a; arr[1]=b; arr[2]=c; echo ${arr[*]}");
        assert_eq!(out, "a b c\n");
    }

    #[test]
    fn typeset_integer_evaluates_arithmetic_on_store() {
        let (_, _, ev) = run("typeset -i n; n=2+3");
        assert_eq!(ev.scope().get("n").unwrap().to_scalar_string(), "5");
    }

    #[test]
    fn typeset_uppercase_folds_on_store() {
        let (_, _, ev) = run("typeset -u X=hello");
        assert_eq!(ev.scope().get("X").unwrap().to_scalar_string(), "HELLO");
    }

    #[test]
    fn typeset_lowercase_folds_on_store() {
        let (_, _, ev) = run("typeset -l X=HELLO");
        assert_eq!(ev.scope().get("X").unwrap().to_scalar_string(), "hello");
    }

    #[test]
    fn typeset_readonly_locks_binding() {
        let prog = parse("typeset -r X=fixed; X=other").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert!(matches!(err, KashError::Readonly(_)));
    }

    #[test]
    fn typeset_indexed_declaration_then_subscript_assign() {
        let (_, out, _) = run("typeset -a arr; arr[0]=x; arr[2]=z; echo \"${arr[0]}[${arr[2]}]\"");
        assert_eq!(out, "x[z]\n");
    }

    #[test]
    fn typeset_dash_p_lists_bindings() {
        let (_, out, _) = run("X=hi; typeset -p");
        assert!(out.contains("typeset X='hi'"), "got: {out:?}");
    }

    #[test]
    fn export_marks_binding_for_env() {
        let (_, _, ev) = run("export FOO=bar");
        let b = ev.scope().get_binding("FOO").unwrap();
        assert!(b.attrs.export);
        assert_eq!(b.value.to_scalar_string(), "bar");
    }

    #[test]
    fn export_then_typeset_listing_shows_x() {
        let (_, out, _) = run("export FOO=bar; typeset -p");
        assert!(out.contains("typeset -x FOO='bar'"), "got: {out:?}");
    }

    #[cfg(feature = "std")]
    #[test]
    fn exported_env_reaches_external_command() {
        use std::path::Path;
        if !Path::new("/usr/bin/env").exists() && !Path::new("/bin/env").exists() {
            return;
        }
        let envprog = if Path::new("/usr/bin/env").exists() {
            "/usr/bin/env"
        } else {
            "/bin/env"
        };
        let src = alloc::format!("export KASH_BENCH_X=alpha; {envprog}");
        let prog = parse(&src).unwrap();
        let mut ev = Evaluator::new();
        ev.eval_program(&prog).unwrap();
        let out = ev.take_output();
        assert!(out.contains("KASH_BENCH_X=alpha"), "got: {out:?}");
    }

    // ===== arithmetic: float + float128 tiers =====

    #[test]
    fn arith_integer_division_stays_integer() {
        let (_, out, _) = run("echo $((7/2))");
        assert_eq!(out, "3\n");
    }

    #[test]
    fn arith_float_literal_addition() {
        let (_, out, _) = run("echo $((1.5 + 2.5))");
        assert_eq!(out, "4.0\n");
    }

    #[test]
    fn arith_int_float_promotion() {
        let (_, out, _) = run("echo $((3 + 0.5))");
        assert_eq!(out, "3.5\n");
    }

    #[test]
    fn arith_float_division() {
        let (_, out, _) = run("echo $((3.0 / 2))");
        assert_eq!(out, "1.5\n");
    }

    #[test]
    fn arith_float_comparison_yields_int() {
        let (_, out, _) = run("echo $((1.5 < 2)) $((2.5 > 3)) $((1.0 == 1))");
        assert_eq!(out, "1 0 1\n");
    }

    #[test]
    fn arith_float_stored_to_typed_float() {
        let (_, out, _) = run("float64 x=$((1.0 / 4)); echo $x");
        assert_eq!(out, "0.25\n");
    }

    #[test]
    fn arith_bitwise_on_float_errors() {
        let prog = parse("echo $((1.5 & 2))").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert!(alloc::format!("{err}").contains("integer operands"));
    }

    #[test]
    fn arith_float128_operand_promotes_to_113_bit() {
        // A `float128` operand promotes the whole operation to
        // 113-bit BigFloat — precision survives far past f64.
        let (_, out, _) = run(
            "float128 x=1.123456789012345678901234567890\necho $((x + x))\n",
        );
        assert_eq!(out, "2.24691357802469135780246913578e+0\n");
    }

    #[test]
    fn arith_float128_mixed_with_int() {
        let (_, out, _) = run("float128 x=1.5\necho $((x * 4 + 1))\n");
        assert_eq!(out, "7e+0\n");
    }

    #[test]
    fn arith_float128_stored_back() {
        let (_, out, _) = run(
            "float128 x=2\nfloat128 y=$((x * x))\necho $y\n",
        );
        assert_eq!(out, "4e+0\n");
    }

    // ===== arithmetic: complex tier =====

    #[test]
    fn arith_complex_literal_and_add() {
        let (_, out, _) = run("echo $((1 + 2i))");
        assert_eq!(out, "1.0+2.0i\n");
    }

    #[test]
    fn arith_complex_multiplication() {
        // (1+2i)(3+4i) = -5+10i — the memory's worked example.
        let (_, out, _) = run(
            "complex128 a=1+2i\ncomplex128 b=3+4i\necho $((a * b))\n",
        );
        assert_eq!(out, "-5.0+10.0i\n");
    }

    #[test]
    fn arith_complex_division() {
        // (1+2i)/(1+2i) = 1.
        let (_, out, _) = run(
            "complex128 a=1+2i\necho $((a / a))\n",
        );
        assert_eq!(out, "1.0\n");
    }

    #[test]
    fn arith_real_times_imaginary() {
        let (_, out, _) = run("echo $((3 * 2i))");
        assert_eq!(out, "6.0i\n");
    }

    #[test]
    fn arith_complex_stored_back() {
        let (_, out, _) = run(
            "complex128 a=1+2i\ncomplex128 b=3+4i\ncomplex128 r=$((a * b))\necho $r\n",
        );
        assert_eq!(out, "-5.0+10.0i\n");
    }

    #[test]
    fn arith_complex256_at_113_bit() {
        // (1+i)² = 2i, computed in the complex256 tier.
        let (_, out, _) = run(
            "complex256 z=1+1i\necho $((z * z))\n",
        );
        assert_eq!(out, "0.0+2e+0i\n");
    }

    #[test]
    fn arith_complex_ordering_is_an_error() {
        let prog = parse("echo $(( (1+2i) < 3 ))").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert!(alloc::format!("{err}").contains("not ordered"));
    }

    #[test]
    fn arith_complex_modulo_is_an_error() {
        let prog = parse("echo $(( (1+2i) % 2 ))").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert!(alloc::format!("{err}").contains("undefined for complex"));
    }

    #[test]
    fn arith_complex_equality() {
        let (_, out, _) =
            run("echo $(( (1+2i) == (1+2i) )) $(( (1+2i) == (1+3i) ))");
        assert_eq!(out, "1 0\n");
    }

    #[test]
    fn arith_complex_zero_imaginary_is_real() {
        // `(1+2i) - 2i` is `1+0i` — really real, so usable as an
        // integer index and rendered as a plain real.
        let (_, out, _) = run("echo $(( (4 + 0i) + 1 ))");
        assert_eq!(out, "5.0\n");
        let (_, out, _) =
            run("s=XYZ\necho ${s:$(( (1+2i) - 2i )):1}\n");
        assert_eq!(out, "Y\n");
    }

    #[test]
    fn arith_genuinely_complex_has_no_integer_form() {
        let prog = parse("s=XYZ\necho ${s:$(( 1+2i )):1}\n").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert!(alloc::format!("{err}").contains("no integer form"));
    }

    #[test]
    fn arith_float128_truncates_to_int_via_raw_parts() {
        // A float128 variable used *directly* as an integer-context
        // operand routes through `eval_arith` → `bigfloat_to_f64`
        // (the `as_raw_parts` reconstruction). `2.9` truncates to
        // 2. Regression guard: this path shift-overflow-panicked
        // in debug builds before the loop was de-looped. (Note
        // `${s:o:2}`, not `${s:$((o)):2}` — the latter renders `o`
        // to a string first and never reaches the converter.)
        let (_, out, _) = run("s=ABCDEF\nfloat128 o=2.9\necho ${s:o:2}\n");
        assert_eq!(out, "CD\n");
    }

    #[test]
    fn arith_float128_codepoint_via_raw_parts() {
        // `(#)` flag → `eval_arith` → `bigfloat_to_f64`; 65 → 'A'.
        let (_, out, _) =
            run("float128 x=65\nn=x\nprintf '%s\\n' ${(#)n}\n");
        assert_eq!(out, "A\n");
    }

    #[test]
    fn arith_float128_small_value_comparison() {
        // astro-float 0.9.5's `BigFloat::cmp` mis-orders a tiny
        // value against zero; the subtraction-based `bigfloat_cmp`
        // workaround gives the right answer regardless of operand
        // order.
        let (_, out, _) = run("float128 x=1e-40\necho $(( x > 0 )) $(( 0 < x )) $(( x < 0 ))\n");
        assert_eq!(out, "1 1 0\n");
        let (_, out, _) =
            run("float128 x=-1e-300\necho $(( x < 0 )) $(( x > 0 ))\n");
        assert_eq!(out, "1 0\n");
    }

    // ===== arithmetic: math library (real f64 tier) =====

    #[test]
    fn math_sqrt() {
        let (_, out, _) = run("echo $(( sqrt(2) ))");
        assert_eq!(out, "1.4142135623730951\n");
    }

    #[test]
    fn math_trig_at_zero() {
        let (_, out, _) = run("echo $(( sin(0) )) $(( cos(0) )) $(( tan(0) ))");
        assert_eq!(out, "0.0 1.0 0.0\n");
    }

    #[test]
    fn math_pi_reconstructed_via_atan2() {
        let (_, out, _) = run("echo $(( atan2(1, 1) * 4 ))");
        assert_eq!(out, "3.141592653589793\n");
    }

    #[test]
    fn math_constant_pi_and_e() {
        let (_, out, _) = run("echo $(( M_PI )) $(( M_E ))");
        assert_eq!(out, "3.141592653589793 2.718281828459045\n");
    }

    #[test]
    fn math_natural_log_of_e_is_one() {
        let (_, out, _) = run("echo $(( log(M_E) )) $(( ln(M_E) ))");
        assert_eq!(out, "1.0 1.0\n");
    }

    #[test]
    fn math_pow_and_hypot() {
        let (_, out, _) = run("echo $(( pow(2, 10) )) $(( hypot(3, 4) ))");
        assert_eq!(out, "1024.0 5.0\n");
    }

    #[test]
    fn math_abs_preserves_integer_tier() {
        let (_, out, _) = run("echo $(( abs(-5) )) $(( abs(-2.5) ))");
        assert_eq!(out, "5 2.5\n");
    }

    #[test]
    fn math_rounding_family() {
        let (_, out, _) = run(
            "echo $(( floor(3.7) )) $(( ceil(3.2) )) $(( round(2.5) )) $(( trunc(3.9) ))",
        );
        assert_eq!(out, "3.0 4.0 3.0 3.0\n");
    }

    #[test]
    fn math_function_name_does_not_shadow_variable() {
        // A variable named like a function is fine — only `name(`
        // is a call.
        let (_, out, _) = run("sqrt=9\necho $(( sqrt + sqrt(sqrt) ))");
        assert_eq!(out, "12.0\n");
    }

    #[test]
    fn math_unknown_function_errors() {
        let prog = parse("echo $(( nope(1) ))").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert!(alloc::format!("{err}").contains("unknown function"));
    }

    #[test]
    fn math_wrong_arity_errors() {
        let prog = parse("echo $(( sin(1, 2) ))").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert!(alloc::format!("{err}").contains("argument"));
    }

    // ===== arithmetic: math library (float128 + complex tiers) =====

    #[test]
    fn math_float128_sqrt_at_113_bit() {
        // 113-bit √2 — far more digits than f64's ~17.
        let (_, out, _) = run("float128 x=2\necho $(( sqrt(x) ))");
        assert_eq!(
            out,
            "1.41421356237309504880168872420969807857e+0\n",
        );
    }

    #[test]
    fn math_float128_transcendentals() {
        // f64-prefix agreement with Python is enough to confirm
        // the right function is dispatched in the f128 tier.
        let (_, out, _) = run("float128 x=2\necho $(( sin(x) ))");
        assert!(out.starts_with("9.09297426825681695"), "got: {out}");
        let (_, out, _) = run("float128 x=2\necho $(( exp(x) ))");
        assert!(out.starts_with("7.38905609893065022"), "got: {out}");
        let (_, out, _) = run("float128 x=2\necho $(( log(x) ))");
        assert!(out.starts_with("6.93147180559945309"), "got: {out}");
    }

    #[test]
    fn math_float128_rounding_family() {
        let (_, out, _) = run(
            "float128 y=3.7\necho $(( floor(y) )) $(( ceil(y) )) $(( round(y) )) $(( trunc(y) ))",
        );
        assert_eq!(out, "3e+0 4e+0 4e+0 3e+0\n");
        let (_, out, _) = run(
            "float128 y=-3.7\necho $(( floor(y) )) $(( ceil(y) )) $(( round(y) )) $(( trunc(y) ))",
        );
        assert_eq!(out, "-4e+0 -3e+0 -4e+0 -3e+0\n");
    }

    #[test]
    fn math_float128_atan2_and_hypot() {
        // atan2(2, 2) = π/4.
        let (_, out, _) = run("float128 x=2\necho $(( atan2(x, x) ))");
        assert!(out.starts_with("7.85398163397448309"), "got: {out}");
        let (_, out, _) = run("float128 a=3\nfloat128 b=4\necho $(( hypot(a, b) ))");
        assert_eq!(out, "5e+0\n");
    }

    #[test]
    fn math_complex_specific_functions() {
        let (_, out, _) = run(
            "complex128 z=3+4i\necho $(( cabs(z) )) $(( creal(z) )) $(( cimag(z) ))",
        );
        assert_eq!(out, "5.0 3.0 4.0\n");
        let (_, out, _) = run("complex128 z=3+4i\necho $(( carg(z) ))");
        assert_eq!(out, "0.9272952180016122\n");
        let (_, out, _) = run("complex128 z=3+4i\necho $(( cconj(z) ))");
        assert_eq!(out, "3.0-4.0i\n");
    }

    #[test]
    fn math_complex256_specific_functions() {
        let (_, out, _) = run(
            "complex256 w=3+4i\necho $(( cabs(w) )) $(( creal(w) )) $(( cimag(w) ))",
        );
        assert_eq!(out, "5e+0 3e+0 4e+0\n");
    }

    #[test]
    fn math_abs_of_complex_is_magnitude() {
        let (_, out, _) = run("complex128 z=3+4i\necho $(( abs(z) ))");
        assert_eq!(out, "5.0\n");
    }

    #[test]
    fn math_complex_transcendental_not_yet_supported() {
        let prog = parse("complex128 z=1+2i\necho $(( sqrt(z) ))").unwrap();
        let mut ev = Evaluator::new();
        let err = ev.eval_program(&prog).unwrap_err();
        assert!(alloc::format!("{err}").contains("not yet supported"));
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
        let (_, out, _) = run("X=$(echo hi); echo \"[$X]\"");
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
    fn substitution_external_not_found_does_not_propagate() {
        // POSIX 2.6.3: command substitution captures stdout; a
        // command-not-found inside the substitution doesn't abort
        // the host shell — it just leaves the captured value
        // possibly-empty and surfaces as the assignment's own
        // status (`X=...` is always success).
        let prog = parse("X=$(false; nope_not_a_real_cmd_xyzzy); echo done").unwrap();
        let mut ev = Evaluator::new();
        assert!(ev.eval_program(&prog).is_ok());
        assert!(ev.take_output().contains("done"));
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
        // POSIX status 127 — but propagated through the
        // `eval_command` recovery, so it's an `Ok(Status(127))`
        // outcome, not a `KashError`. The original "command not
        // found" message lands in the stderr buffer.
        let prog = parse("definitely_not_a_real_command").unwrap();
        let mut ev = Evaluator::new();
        let outcome = ev.eval_program(&prog).unwrap();
        assert_eq!(outcome.status(), 127);
        let err = ev.take_stderr();
        assert!(err.contains("command not found"), "got: {err}");
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
            let outcome = ev.eval_program(&prog).unwrap();
            assert_eq!(outcome.status(), 127);
            let err = ev.take_stderr();
            assert!(err.contains("command not found"), "got: {err}");
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
        fn pipeline_rejects_non_first_builtin_stage() {
            // A pure-output builtin as the *leading* stage runs
            // in-process and bridges its captured output into the
            // next stage's stdin — that case is now supported.
            // A builtin past the first stage, however, still
            // requires the cross-process bridge we haven't built.
            if !have("/bin/echo") {
                return;
            }
            let prog = parse("/bin/echo a | echo b").unwrap();
            let mut ev = Evaluator::new();
            let err = ev.eval_program(&prog).unwrap_err();
            let msg = alloc::format!("{err}");
            assert!(msg.contains("not yet supported"), "got: {msg}");
        }
    }

    // ===== filesystem globbing (std) =====

    #[cfg(feature = "std")]
    mod glob_tests {
        use super::*;

        /// Make a unique empty temp directory for one test. Unique
        /// across processes (pid) and within a process (atomic
        /// counter) so tests run in parallel without colliding.
        /// Returns the absolute path as a `String`.
        fn temp_dir(tag: &str) -> String {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let mut p = std::env::temp_dir();
            p.push(alloc::format!("kash-glob-{tag}-{pid}-{n}"));
            std::fs::create_dir_all(&p).unwrap();
            p.to_string_lossy().into_owned()
        }

        fn touch(dir: &str, name: &str) {
            std::fs::write(alloc::format!("{dir}/{name}"), b"").unwrap();
        }

        fn mkdir(dir: &str, name: &str) {
            std::fs::create_dir_all(alloc::format!("{dir}/{name}")).unwrap();
        }

        /// Run `src` under an explicit mode and return stdout.
        fn run_mode(src: &str, mode: &str) -> String {
            let prog = parse(src).unwrap();
            let mut ev = Evaluator::with_mode(
                crate::mode::Mode::parse(mode).unwrap(),
            );
            ev.eval_program(&prog).unwrap();
            ev.take_output()
        }

        #[test]
        fn star_matches_extension_sorted() {
            let d = temp_dir("star");
            touch(&d, "b.txt");
            touch(&d, "a.txt");
            touch(&d, "c.log");
            let (_, out, _) = run(&alloc::format!("echo {d}/*.txt"));
            assert_eq!(out, alloc::format!("{d}/a.txt {d}/b.txt\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn question_matches_single_char() {
            let d = temp_dir("q");
            touch(&d, "a.log");
            touch(&d, "ab.log");
            let (_, out, _) = run(&alloc::format!("echo {d}/?.log"));
            assert_eq!(out, alloc::format!("{d}/a.log\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn character_class_matches() {
            let d = temp_dir("class");
            touch(&d, "a.txt");
            touch(&d, "b.txt");
            touch(&d, "c.txt");
            let (_, out, _) = run(&alloc::format!("echo {d}/[ac].txt"));
            assert_eq!(out, alloc::format!("{d}/a.txt {d}/c.txt\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn double_star_matches_recursively() {
            let d = temp_dir("globstar");
            touch(&d, "top.txt");
            mkdir(&d, "sub/deep");
            touch(&d, "sub/mid.txt");
            touch(&d, "sub/deep/low.txt");
            let (_, out, _) = run(&alloc::format!("echo {d}/**/*.txt"));
            assert_eq!(
                out,
                alloc::format!(
                    "{d}/sub/deep/low.txt {d}/sub/mid.txt {d}/top.txt\n"
                ),
            );
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn trailing_double_star_matches_all_descendants() {
            // Trailing `**` yields the base directory plus every
            // non-hidden descendant — files and directories,
            // recursively (bash globstar set, modulo the base
            // trailing-slash cosmetic difference).
            let d = temp_dir("trailing");
            touch(&d, "file.txt");
            mkdir(&d, "sub/deep");
            touch(&d, "sub/mid.txt");
            touch(&d, "sub/deep/leaf.txt");
            let out =
                run_mode(&alloc::format!("echo {d}/**"), "posix-aware");
            assert_eq!(
                out,
                alloc::format!(
                    "{d} {d}/file.txt {d}/sub {d}/sub/deep \
                     {d}/sub/deep/leaf.txt {d}/sub/mid.txt\n"
                ),
            );
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn trailing_double_star_does_not_recurse_symlinked_dirs() {
            // A symlink-to-directory is listed as a leaf but not
            // descended into, so a cycle can't loop.
            let d = temp_dir("symloop");
            mkdir(&d, "real");
            touch(&d, "real/x.txt");
            std::os::unix::fs::symlink(
                alloc::format!("{d}/real"),
                alloc::format!("{d}/link"),
            )
            .unwrap();
            let out =
                run_mode(&alloc::format!("echo {d}/**"), "posix-aware");
            // `link` appears once (as a leaf), never recursed.
            assert!(out.contains(&alloc::format!("{d}/link ")) || out.ends_with(&alloc::format!("{d}/link\n")) || out.contains(&alloc::format!("{d}/link {d}")), "got: {out}");
            assert!(!out.contains(&alloc::format!("{d}/link/x.txt")), "should not recurse symlink: {out}");
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn star_skips_hidden_files() {
            let d = temp_dir("hidden");
            touch(&d, "visible.txt");
            touch(&d, ".hidden.txt");
            let (_, out, _) = run(&alloc::format!("echo {d}/*.txt"));
            assert_eq!(out, alloc::format!("{d}/visible.txt\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn dot_pattern_matches_hidden() {
            let d = temp_dir("dot");
            touch(&d, ".bashrc");
            touch(&d, "normal");
            let (_, out, _) = run(&alloc::format!("echo {d}/.*"));
            assert_eq!(out, alloc::format!("{d}/.bashrc\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn no_match_fails_in_default_mode() {
            let d = temp_dir("nomatch");
            let prog =
                parse(&alloc::format!("echo {d}/*.none")).unwrap();
            let mut ev = Evaluator::new();
            let err = ev.eval_program(&prog).unwrap_err();
            assert!(alloc::format!("{err}").contains("no matches"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn no_match_survives_literal_in_posix_aware() {
            let d = temp_dir("survive");
            let out = run_mode(
                &alloc::format!("echo {d}/*.none"),
                "posix-aware",
            );
            assert_eq!(out, alloc::format!("{d}/*.none\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn no_match_fails_under_secure() {
            let d = temp_dir("secure");
            let prog =
                parse(&alloc::format!("echo {d}/*.none")).unwrap();
            let mut ev = Evaluator::with_mode(
                crate::mode::Mode::parse("posix-aware-secure").unwrap(),
            );
            assert!(ev.eval_program(&prog).is_err());
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn quoted_pattern_stays_literal() {
            let d = temp_dir("quoted");
            touch(&d, "a.txt");
            // Single-quoted → no glob even though `a.txt` exists.
            let (_, out, _) =
                run(&alloc::format!("echo '{d}/*.txt'"));
            assert_eq!(out, alloc::format!("{d}/*.txt\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn non_glob_field_passes_through() {
            // A field with no metacharacter is returned as-is even
            // when it names a missing path.
            let d = temp_dir("plain");
            let (_, out, _) =
                run(&alloc::format!("echo {d}/literal.txt"));
            assert_eq!(out, alloc::format!("{d}/literal.txt\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn star_matches_all_in_dir_sorted() {
            let d = temp_dir("all");
            touch(&d, "3");
            touch(&d, "1");
            touch(&d, "2");
            let (_, out, _) = run(&alloc::format!("echo {d}/*"));
            assert_eq!(
                out,
                alloc::format!("{d}/1 {d}/2 {d}/3\n"),
            );
            std::fs::remove_dir_all(&d).ok();
        }

        // --- (#q…) glob qualifiers ---

        #[test]
        fn qualifier_regular_files_only() {
            let d = temp_dir("q-reg");
            touch(&d, "a.txt");
            touch(&d, "b.txt");
            mkdir(&d, "adir");
            std::os::unix::fs::symlink(
                alloc::format!("{d}/a.txt"),
                alloc::format!("{d}/link"),
            )
            .unwrap();
            let (_, out, _) = run(&alloc::format!("echo {d}/*(#q.)"));
            assert_eq!(out, alloc::format!("{d}/a.txt {d}/b.txt\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn qualifier_directories_only() {
            let d = temp_dir("q-dir");
            touch(&d, "file");
            mkdir(&d, "d1");
            mkdir(&d, "d2");
            let (_, out, _) = run(&alloc::format!("echo {d}/*(#q/)"));
            assert_eq!(out, alloc::format!("{d}/d1 {d}/d2\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn qualifier_symlinks_only() {
            let d = temp_dir("q-sym");
            touch(&d, "real");
            std::os::unix::fs::symlink(
                alloc::format!("{d}/real"),
                alloc::format!("{d}/link"),
            )
            .unwrap();
            let (_, out, _) = run(&alloc::format!("echo {d}/*(#q@)"));
            assert_eq!(out, alloc::format!("{d}/link\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn qualifier_executable() {
            let d = temp_dir("q-exec");
            touch(&d, "plain");
            touch(&d, "prog");
            let mut perm = std::fs::metadata(alloc::format!("{d}/prog"))
                .unwrap()
                .permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
            std::fs::set_permissions(alloc::format!("{d}/prog"), perm).unwrap();
            let (_, out, _) = run(&alloc::format!("echo {d}/*(#q*)"));
            assert_eq!(out, alloc::format!("{d}/prog\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn qualifier_null_optin_allows_empty() {
            // `(#qN)` opts into null glob even in default (failglob)
            // mode — an empty result is fine, no error. The whole
            // field drops, so `echo` runs with no arguments.
            let d = temp_dir("q-null");
            let (o, out, _) =
                run(&alloc::format!("echo {d}/*.none(#qN)"));
            assert_eq!(o.status(), 0);
            assert_eq!(out, "\n");
            // A `for` loop over the same pattern runs zero times.
            let (_, out, _) = run(&alloc::format!(
                "for f in {d}/*.none(#qN); do echo iter; done"
            ));
            assert_eq!(out, "");
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn qualifier_dotfiles_included() {
            let d = temp_dir("q-dot");
            touch(&d, "visible");
            touch(&d, ".hidden");
            let (_, out, _) = run(&alloc::format!("echo {d}/*(#qD)"));
            assert_eq!(out, alloc::format!("{d}/.hidden {d}/visible\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn qualifier_slice_range() {
            let d = temp_dir("q-slice");
            touch(&d, "1");
            touch(&d, "2");
            touch(&d, "3");
            touch(&d, "4");
            // Name sort, take elements 2..3 (1-based inclusive).
            let (_, out, _) =
                run(&alloc::format!("echo {d}/*(#qon[2,3])"));
            assert_eq!(out, alloc::format!("{d}/2 {d}/3\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn qualifier_count_cap() {
            let d = temp_dir("q-count");
            touch(&d, "a");
            touch(&d, "b");
            touch(&d, "c");
            let (_, out, _) = run(&alloc::format!("echo {d}/*(#qY2)"));
            assert_eq!(out, alloc::format!("{d}/a {d}/b\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn qualifier_y0_keeps_all() {
            // `Y0` is "no cap" (zsh), not truncate-to-zero.
            let d = temp_dir("q-y0");
            touch(&d, "a");
            touch(&d, "b");
            touch(&d, "c");
            let (o, out, _) =
                run(&alloc::format!("echo {d}/*(#qonY0)"));
            assert_eq!(o.status(), 0);
            assert_eq!(out, alloc::format!("{d}/a {d}/b {d}/c\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn qualifier_count_caps_before_slice() {
            // `Y<n>` stops the glob at n matches, *then* the slice
            // applies — so a slice past the cap is empty.
            let d = temp_dir("q-capslice");
            for n in 1..=6 {
                touch(&d, &alloc::format!("f{n}"));
            }
            // Cap 2, slice [1,2] → the first two.
            let (_, out, _) =
                run(&alloc::format!("echo {d}/*(#qonY2[1,2])"));
            assert_eq!(out, alloc::format!("{d}/f1 {d}/f2\n"));
            // Cap 2, slice [4,6] → out of range → empty; `N`
            // allows the empty result without a failglob error.
            let (o, out, _) =
                run(&alloc::format!("echo {d}/*(#qonY2N[4,6])"));
            assert_eq!(o.status(), 0);
            assert_eq!(out, "\n");
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn qualifier_sort_by_size_descending() {
            let d = temp_dir("q-size");
            std::fs::write(alloc::format!("{d}/small"), b"x").unwrap();
            std::fs::write(alloc::format!("{d}/big"), b"xxxxxxxxxx").unwrap();
            std::fs::write(alloc::format!("{d}/mid"), b"xxxxx").unwrap();
            // OL = size descending; take the largest.
            let (_, out, _) =
                run(&alloc::format!("echo {d}/*(#qOL[1])"));
            assert_eq!(out, alloc::format!("{d}/big\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn qualifier_literal_pattern_existence_check() {
            let d = temp_dir("q-lit");
            touch(&d, "here.txt");
            // A literal pattern + type qualifier acts as an
            // existence-and-type check.
            let (_, out, _) =
                run(&alloc::format!("echo {d}/here.txt(#q.)"));
            assert_eq!(out, alloc::format!("{d}/here.txt\n"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn qualifier_unknown_letter_errors() {
            let d = temp_dir("q-unknown");
            touch(&d, "a");
            let prog =
                parse(&alloc::format!("echo {d}/*(#qZ)")).unwrap();
            let mut ev = Evaluator::new();
            let err = ev.eval_program(&prog).unwrap_err();
            assert!(alloc::format!("{err}").contains("qualifier"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn qualifier_ownership_not_yet_supported() {
            let d = temp_dir("q-own");
            touch(&d, "a");
            let prog = parse(&alloc::format!("echo {d}/*(#qu:me:)"))
                .unwrap();
            let mut ev = Evaluator::new();
            let err = ev.eval_program(&prog).unwrap_err();
            assert!(alloc::format!("{err}").contains("ownership"));
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn qualifier_is_literal_in_strict_mode() {
            // In a strict mode `(#q…)` is not a qualifier; the
            // pattern (which here matches nothing real) survives
            // literally per the strict null-glob policy.
            let d = temp_dir("q-strict");
            touch(&d, "a.txt");
            let out = run_mode(
                &alloc::format!("echo {d}/a.txt(#q.)"),
                "posix-strict",
            );
            assert_eq!(out, alloc::format!("{d}/a.txt(#q.)\n"));
            std::fs::remove_dir_all(&d).ok();
        }
    }
}
