//! Abstract syntax tree node definitions.
//!
//! Models commands, pipelines, redirections, quotes, expansion flags,
//! mode declarations, function definitions (POSIX + capture-list form),
//! typeclass / instance / namespace declarations, and the various
//! literal forms (strings, here-docs, numeric primitives, complex
//! literals). Designed to be allocation-friendly (Vec/Box) but
//! `no_std + alloc` compatible.
//!
//! Scope of this commit: POSIX command syntax up through and including
//! compound commands — brace groups, subshells, `if`/`elif`/`else`,
//! `while`/`until`, `for`, and `case` (with the three POSIX/ksh93
//! fall-through variants `;;`, `;&`, `;;&`). Function definitions,
//! assignment prefixes, here-docs, FD-dup redirects, `!` pipeline
//! negation, and kash-specific declarations (`mode`, `namespace`,
//! `typeclass`, `instance`, `use`) are intentionally not modelled yet.

use alloc::boxed::Box;
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

/// Redirection operator kind. FD-dup variants land in a follow-up.
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
    /// `[n]>&m` — duplicate fd `m` onto fd `n` for the command's
    /// duration. `target` carries the right-hand side as a word
    /// (typically a single decimal number, or `-` to close).
    DupOutput,
    /// `[n]<&m` — duplicate fd `m` onto fd `n` (input side).
    DupInput,
    /// `<<<word` — feed `word` (plus a trailing newline) as stdin.
    /// `target` carries the word.
    HereString,
    /// `<<DELIM` / `<<-DELIM` — feed an inline body as stdin. The
    /// `target` word's first segment carries the captured body
    /// verbatim; a `Bare` segment means the body is subject to
    /// parameter / arithmetic / command-substitution expansion, while
    /// a `SingleQuoted` segment means the delimiter was quoted in the
    /// source (`<<'EOF'`, `<<"EOF"`, `<<\\EOF`) and the body must be
    /// passed through unexpanded.
    HereDoc {
        /// True for the `<<-` form, where each body line's leading
        /// tab characters were stripped at parse time.
        strip_tabs: bool,
    },
}

/// A single redirection clause attached to a [`Command`].
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

/// `KEY=VALUE` (or `KEY[SUBSCRIPT]=VALUE`) prefix on a
/// [`SimpleCommand`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    /// Variable name.
    pub name: String,
    /// Optional subscript — `Some(word)` for `name[word]=value`
    /// (indexed or associative array element), `None` for the bare
    /// `name=value` form.
    pub subscript: Option<Word>,
    /// Right-hand side expression as a word.
    pub value: Word,
    /// Source span covering `name[subscript]=value`.
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

/// A pipeline stage — either a simple command or a compound command,
/// in either case wrapped together with the redirect clauses that
/// apply to *this* stage as a unit (e.g. `{ a; b; } > /tmp/log` parks
/// the redirect on the brace group, not on `b`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// A simple command (its redirects live inside [`SimpleCommand`]).
    Simple(SimpleCommand),
    /// A compound command plus its outer redirect list.
    Compound(CompoundCommand),
}

impl Command {
    /// Source span of this command.
    #[inline]
    #[must_use]
    pub fn span(&self) -> Span {
        match self {
            Self::Simple(s) => s.span,
            Self::Compound(c) => c.span,
        }
    }
}

/// A compound command (one of the grouping / control-flow forms),
/// together with any outer redirects that apply to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompoundCommand {
    /// Which compound form this is.
    pub kind: CompoundKind,
    /// Redirects attached to the compound as a whole.
    pub redirects: Vec<Redirect>,
    /// Source span covering the whole compound + its redirects.
    pub span: Span,
}

/// Compound command shape. POSIX shapes only in this commit; kash-
/// specific shapes (`namespace { ... }`, `typeclass ... { ... }`, …)
/// land in later commits.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompoundKind {
    /// `{ ... }` — runs in the current shell environment.
    BraceGroup {
        /// Statements inside the group.
        body: Vec<Statement>,
    },
    /// `( ... )` — runs in a subshell.
    Subshell {
        /// Statements inside the subshell.
        body: Vec<Statement>,
    },
    /// `if … then … (elif … then …)* (else …)? fi`.
    If {
        /// One entry per `if` / `elif` arm, in source order. `branches`
        /// is always non-empty (the first entry is the `if` arm).
        branches: Vec<IfBranch>,
        /// Optional `else` body.
        else_body: Option<Vec<Statement>>,
    },
    /// `while COND do BODY done` — loops as long as `cond` exits zero.
    While {
        /// Loop condition (a list of statements; exit status of the
        /// last one decides whether to enter the body).
        cond: Vec<Statement>,
        /// Loop body.
        body: Vec<Statement>,
    },
    /// `until COND do BODY done` — loops as long as `cond` exits non-zero.
    Until {
        /// Loop condition (sense inverted vs. [`While`](Self::While)).
        cond: Vec<Statement>,
        /// Loop body.
        body: Vec<Statement>,
    },
    /// `for NAME [in WORDS]? do BODY done`.
    For {
        /// Loop variable name.
        name: String,
        /// Iteration set. `None` means the `in` clause was omitted, so
        /// the loop iterates over `"$@"` (positional parameters).
        words: Option<Vec<Word>>,
        /// Loop body.
        body: Vec<Statement>,
    },
    /// `case SUBJECT in PATTERN) BODY ;; … esac`.
    Case {
        /// Subject word (gets pattern-matched against each item).
        subject: Word,
        /// Case arms, in source order.
        items: Vec<CaseItem>,
    },
    /// `[[ … ]]` extended test (ksh93 / bash baseline). The body is
    /// the sequence of expanded words / operator words inside the
    /// brackets; structural operators (`&&`, `||`, `!`, parens) are
    /// modelled by their `Word`-typed literal so the evaluator can
    /// drive a small recursive matcher without needing a dedicated
    /// expression AST. Word splitting and pathname expansion do *not*
    /// fire inside `[[…]]` per the locked semantics.
    DoubleBracket {
        /// The sequence of words inside the brackets, in source order.
        tokens: Vec<Word>,
    },
    /// A typeclass declaration. Per `project_shell_typeclass.md`,
    /// kash typeclasses are Scala 3-inspired: a named bundle of
    /// method signatures (optionally with default implementations
    /// provided as ordinary function bodies). Inheritance is
    /// deliberately not modelled — instance composition replaces it.
    TypeclassDef {
        /// Typeclass name.
        name: String,
        /// Members — currently every member is a function with a
        /// (default) body; signature-only declarations land with the
        /// dispatch commit.
        items: Vec<TypeclassMember>,
    },
    /// A typeclass instance. Provides concrete implementations of a
    /// typeclass's methods for a given type. The `for_type` slot is a
    /// bare type name (`Int`, `String`, user-defined, …); a richer
    /// type-expression grammar lands with the type-inference commit.
    InstanceDef {
        /// Name of the typeclass this instance implements.
        typeclass: String,
        /// Concrete type the instance is for.
        for_type: String,
        /// Method implementations.
        items: Vec<InstanceMember>,
    },
    /// A `venv NAME { … }` declaration. Locked in
    /// `project_kash_venv.md` — a soft virtual-environment block
    /// that bundles capability profile, env overlay, namespace
    /// imports, and a body of statements as a single scoping unit.
    /// The body sees its own frame (capabilities active, env / PATH
    /// / imports applied) and the frame pops on exit, restoring
    /// the caller's view. This commit ships the v.1 surface
    /// (declaration + body section + push/pop frame); the
    /// `capabilities`, `env`, `imports`, and `load-config` sections
    /// land in follow-up stages.
    VenvDecl {
        /// Venv name (bare identifier, no embedded dots).
        name: String,
        /// Sections in source order.
        sections: Vec<VenvSection>,
    },
    /// A `mode` declaration. Three source forms, all carried by the
    /// [`ModeForm`] tag:
    ///
    ///   * `mode <name>` — unbounded; takes effect from the
    ///     declaration's lexical position to the end of the
    ///     enclosing scope, but does *not* auto-restore on scope
    ///     exit. Idiomatic at file top.
    ///   * `mode -L <name>` — lexical; takes effect from the
    ///     declaration to the end of the enclosing scope and
    ///     restores on scope exit. Idiomatic inside functions.
    ///   * `mode <name> { body }` — block; runs `body` under the new
    ///     mode and restores on block exit.
    ///
    /// The `spec` is the literal mode-name string as it appeared in
    /// source (e.g. `"default-secure"`); the evaluator parses it
    /// against `Mode::parse` at run time so unknown mode names
    /// surface their error at the declaration site, not at parse.
    /// Locked in `project_shell_mode_syntax.md`.
    ModeDecl {
        /// Raw mode-name string as written.
        spec: String,
        /// Source form (unbounded / lexical / block).
        form: ModeForm,
    },
    /// A namespace block. Source form: `namespace NAME { body }`.
    /// `NAME` is a single bare identifier (no embedded dots); to
    /// nest, write the namespace declarations nested. The body is a
    /// list of statements that, when evaluated, register declarations
    /// under the namespace path. Reopening is allowed — repeating
    /// the same namespace path just appends declarations. Locked in
    /// `project_shell_namespace.md`.
    NamespaceDef {
        /// Namespace name (single segment, no leading `.`).
        name: String,
        /// Body statements, evaluated with `name` pushed onto the
        /// evaluator's namespace-path stack.
        body: Vec<Statement>,
    },
    /// A function definition. Three source forms produce this node:
    /// the POSIX `name() <body>`, the ksh93 `function name <body>`,
    /// and the kash extension `function name(p, q, …) <body>` whose
    /// `(p, q, …)` part is the capture list. Locked in
    /// `project_shell_function_scope.md`.
    FunctionDef {
        /// Function name.
        name: String,
        /// Scoping policy (lexical / dynamic). Determined by source
        /// form: POSIX → [`FunctionScope::Dynamic`]; ksh93 / kash form
        /// → [`FunctionScope::Static`].
        scope: FunctionScope,
        /// Capture list, if present. `Some(names)` means the function
        /// is the kash `function name(p, q, …) …` form and captures
        /// those bindings *by reference, read-only* at definition site.
        /// `None` covers both other forms (no capture list at all).
        captures: Option<Vec<String>>,
        /// Function body. Always a compound command.
        body: alloc::boxed::Box<CompoundCommand>,
    },
}

/// One section inside a [`CompoundKind::VenvDecl`] body.
/// Each variant maps to a `<keyword> { … }` block at venv-body
/// position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VenvSection {
    /// `body { … }` — statements that run *inside* the venv frame.
    /// All other sections configure the frame; this one is what
    /// actually executes against it.
    Body {
        /// Statements to run under the venv frame.
        statements: Vec<Statement>,
    },
    /// `capabilities { … }` — capability profile + fine-grained
    /// grant / revoke + external-command allow-list. The spec is
    /// captured textually here; the evaluator materialises it into
    /// a runtime capability set when the venv frame is pushed.
    Capabilities {
        /// Textual spec produced by `parse_capabilities_section`.
        spec: crate::capability::CapabilitySpec,
    },
    /// `load-config PATH` — load capability + env (+ v.6 imports)
    /// from an external *data-only* TOML file. The path is taken
    /// verbatim from source (subject to ordinary expansion at
    /// evaluator time) and parsed strictly as TOML — no shell
    /// evaluation, no source-able formats. See
    /// `project_kash_venv.md` for the schema lock.
    LoadConfig {
        /// The single bare-word path argument written after
        /// `load-config`. Expanded at evaluator time.
        path: Word,
    },
    /// `env { … }` — environment overlay applied to *external*
    /// commands spawned from inside the venv. Directives:
    ///
    ///   * `NAME=VALUE` — set / override an env entry
    ///   * `PATH-prepend DIR` — prepend `DIR` to `PATH`
    ///   * `PATH-append  DIR` — append  `DIR` to `PATH`
    ///
    /// Order matters: directives are applied in declaration order
    /// at spawn time, so later `PATH-prepend`s end up *first* on
    /// the resulting `PATH`.
    Env {
        /// Directive list in source order.
        directives: Vec<EnvDirective>,
    },
}

/// One directive inside a venv `env { … }` section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvDirective {
    /// `NAME=VALUE` — overwrite (or define) an env entry.
    Set {
        /// Env var name.
        name: String,
        /// Env var value.
        value: String,
    },
    /// `PATH-prepend DIR`.
    PathPrepend {
        /// Directory to prepend to `PATH`.
        dir: String,
    },
    /// `PATH-append DIR`.
    PathAppend {
        /// Directory to append to `PATH`.
        dir: String,
    },
}

/// The three source forms of [`CompoundKind::ModeDecl`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModeForm {
    /// `mode <name>` — no automatic restore on scope exit.
    Unbounded,
    /// `mode -L <name>` — automatic restore on enclosing-scope exit.
    Lexical,
    /// `mode <name> { body }` — automatic restore at end of block.
    Block {
        /// Statements that run under the temporarily-installed mode.
        body: Vec<Statement>,
    },
}

/// One member inside a [`CompoundKind::TypeclassDef`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeclassMember {
    /// A method with a default implementation body. Same shape as a
    /// top-level function definition. Source form: `name() body`.
    Default {
        /// Method name.
        name: String,
        /// Method body.
        body: alloc::boxed::Box<CompoundCommand>,
    },
    /// A signature-only (abstract) method. Source form: `name()`
    /// with no body. Every instance of the typeclass must provide a
    /// concrete implementation; instance-registration time enforces
    /// that.
    Signature {
        /// Method name.
        name: String,
    },
}

/// One member inside a [`CompoundKind::InstanceDef`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstanceMember {
    /// A concrete method implementation.
    Method {
        /// Method name.
        name: String,
        /// Method body.
        body: alloc::boxed::Box<CompoundCommand>,
    },
}

/// Scoping policy attached to a function definition. The choice is
/// fixed at parse time by the source form (per
/// `project_shell_function_scope.md`); the evaluator reads it back at
/// call time to pick the right lookup discipline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FunctionScope {
    /// POSIX `name()` form — looks up captured names via dynamic
    /// scope (the runtime call stack).
    Dynamic,
    /// ksh93 `function` keyword form (with or without a capture list)
    /// — lexical scope.
    Static,
}

/// One `if` / `elif` arm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IfBranch {
    /// Condition list — exit status of the last statement decides.
    pub cond: Vec<Statement>,
    /// Body to run if the condition succeeded.
    pub body: Vec<Statement>,
    /// Source span covering `if`/`elif` … `then` … body (up to but
    /// not including the next `elif`/`else`/`fi`).
    pub span: Span,
}

/// One case arm: pattern set + body + fall-through behaviour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseItem {
    /// Patterns separated by `|` (e.g. `a|b|c)` → three patterns).
    pub patterns: Vec<Word>,
    /// Body to run when any pattern matches.
    pub body: Vec<Statement>,
    /// Trailing fall-through marker.
    pub fallthrough: CaseFallthrough,
    /// Source span covering the arm.
    pub span: Span,
}

/// What to do after running a case arm's body. The three variants
/// match the POSIX `;;` plus the bash/ksh93 fall-through extensions
/// (locked in `project_shell_glob_pattern.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseFallthrough {
    /// `;;` — stop after this arm (standard POSIX).
    Stop,
    /// `;&` — fall through and run the next arm's body unconditionally.
    Continue,
    /// `;;&` — stop *if* this body's exit was successful; otherwise
    /// continue matching subsequent arms.
    MatchNext,
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

/// A pipeline: one or more [`Command`]s joined by `|` / `|&`.
///
/// `stages` and `ops` are kept in lock-step. `ops[0]` is a placeholder
/// — only `ops[i]` for `i >= 1` describes a real join (the one *into*
/// stage `i`). The placeholder is always [`PipeOp::Pipe`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pipeline {
    /// Pipeline stages, in left-to-right source order. Always
    /// non-empty.
    pub stages: Vec<Command>,
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

/// One top-level statement: an and-or list with a terminator. Same
/// shape inside compound-command bodies as at program top level.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    /// The and-or list itself.
    pub list: AndOrList,
    /// Trailing terminator.
    pub terminator: Terminator,
    /// Source span covering list + terminator.
    pub span: Span,
}

/// A full source unit: zero or more [`Statement`]s in source order.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Program {
    /// Statements in source order. May be empty (empty input / all
    /// comments / only blank lines).
    pub statements: Vec<Statement>,
}

// Re-export the boxed shape used internally by some parser plumbing.
// Kept private to the crate so the public surface stays flat.
#[allow(dead_code)]
pub(crate) type BoxedStatements = Box<Vec<Statement>>;
