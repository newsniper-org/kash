//! Lexical scopes, variable storage, and namespace registry.
//!
//! Implements the static / dynamic scope rules from
//! `project_shell_function_scope.md` (POSIX form is dynamic; `function f`
//! is static; `function f(a, b)` is static + read-only by-ref capture),
//! plus the `namespace`/`use namespace` machinery from
//! `project_shell_namespace.md` and `project_kash_module_resolution.md`.
