//! Lexical scopes, variable storage, and namespace registry.
//!
//! Implements the static / dynamic scope rules from
//! `project_shell_function_scope.md` (POSIX form is dynamic; `function f`
//! is static; `function f(a, b)` is static + read-only by-ref capture),
//! plus the `namespace`/`use namespace` machinery from
//! `project_shell_namespace.md` and `project_kash_module_resolution.md`.
//!
//! Scope of this commit: a single linear stack of frames with name →
//! [`Value`] lookups, no namespace registry yet, no static-vs-dynamic
//! distinction (every frame behaves dynamically). This is enough to
//! run the evaluator skeleton's variable assignments + lookups; the
//! lexical-capture rules light up when functions become callable.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::value::Value;

/// One stack frame's bindings.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    /// Variable name → value.
    pub vars: BTreeMap<String, Value>,
}

/// Stack of [`Frame`]s. Lookups walk the stack top-down; assignments
/// target the topmost frame unless explicitly directed elsewhere
/// (the latter API isn't there yet).
#[derive(Clone, Debug)]
pub struct Scope {
    frames: Vec<Frame>,
}

impl Scope {
    /// New scope with a single root frame.
    #[must_use]
    pub fn new() -> Self {
        Self {
            frames: vec![Frame::default()],
        }
    }

    /// Push a fresh frame on top (e.g. on function entry).
    pub fn push(&mut self) {
        self.frames.push(Frame::default());
    }

    /// Pop the topmost frame. The root frame is never popped.
    pub fn pop(&mut self) {
        if self.frames.len() > 1 {
            self.frames.pop();
        }
    }

    /// Look up a name. Returns the first hit found while walking from
    /// the top frame to the bottom; `None` if no frame has a binding.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        for frame in self.frames.iter().rev() {
            if let Some(v) = frame.vars.get(name) {
                return Some(v);
            }
        }
        None
    }

    /// Assign in the topmost frame.
    pub fn set(&mut self, name: String, value: Value) {
        let top = self.frames.last_mut().expect("at least one frame");
        top.vars.insert(name, value);
    }

    /// Number of frames currently on the stack (always `>= 1`).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.frames.len()
    }
}

impl Default for Scope {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_frame_persists() {
        let mut s = Scope::new();
        assert_eq!(s.depth(), 1);
        s.set("FOO".into(), Value::scalar("bar"));
        assert_eq!(s.get("FOO").unwrap().to_scalar_string(), "bar");
    }

    #[test]
    fn frame_push_pop_isolates_writes() {
        let mut s = Scope::new();
        s.set("FOO".into(), Value::scalar("outer"));
        s.push();
        s.set("FOO".into(), Value::scalar("inner"));
        assert_eq!(s.get("FOO").unwrap().to_scalar_string(), "inner");
        s.pop();
        assert_eq!(s.get("FOO").unwrap().to_scalar_string(), "outer");
    }

    #[test]
    fn root_frame_cannot_be_popped() {
        let mut s = Scope::new();
        s.pop();
        s.pop();
        assert_eq!(s.depth(), 1);
    }

    #[test]
    fn missing_name_returns_none() {
        let s = Scope::new();
        assert!(s.get("nope").is_none());
    }
}
