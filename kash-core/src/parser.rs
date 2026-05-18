//! Parser — token stream → AST.
//!
//! Hand-written recursive descent. Produces the AST defined in `ast.rs`,
//! which downstream consumers (evaluator, transpiler plugins,
//! formatter) walk. Per `project_kash_implementation.md`, no external
//! parser-combinator crate is used — error messages and recovery are
//! tuned for the REPL.
