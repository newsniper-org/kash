//! Lexical scopes, variable storage, and namespace registry.
//!
//! Implements the static / dynamic scope rules from
//! `project_shell_function_scope.md` (POSIX form is dynamic; `function f`
//! is static; `function f(a, b)` is static + read-only by-ref capture),
//! plus the `namespace`/`use namespace` machinery from
//! `project_shell_namespace.md` and `project_kash_module_resolution.md`.
//!
//! Scope of this commit: a stack of frames with first-class `local` /
//! `readonly` semantics. Dynamic resolution walks the stack to find
//! the nearest existing binding (POSIX); static-scoped function
//! frames pin assignments to themselves (ksh93 `function` form). The
//! `static_scope` flag is per-frame so a `function f`-style frame
//! locals-by-default without affecting calls into it from outside.
//! By-ref capture lists are still not enforced — the parser records
//! them, the evaluator ignores them, and assignments to captured
//! names just fall through dynamic resolution. That tightens up when
//! the typeset attribute machinery lands.
//!
//! Storage layer is abstracted through [`MapBackend`] — `Frame<B>` /
//! `Scope<B>` are generic over the backend, with [`BTreeBackend`] as
//! the default so external callers don't have to spell the parameter.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::collections::{BTreeBackend, MapBackend, MapStorage};
use crate::error::{KashError, Result};
use crate::value::Value;

/// One variable binding. The value travels with its `readonly`
/// attribute so the assignment guards can short-circuit on a single
/// map lookup.
#[derive(Clone, Debug, Default)]
pub struct Binding {
    /// The bound value.
    pub value: Value,
    /// `readonly` attribute (POSIX `readonly`, ksh93 `typeset -r`).
    /// Set bindings refuse further mutation; `local` of the same name
    /// in an inner frame still works (it creates a *new* binding).
    pub readonly: bool,
}

/// One stack frame's bindings + scope-discipline flags. Generic over
/// the [`MapBackend`] used for the binding table.
pub struct Frame<B: MapBackend = BTreeBackend> {
    /// Name → binding.
    pub bindings: B::Map<String, Binding>,
    /// True iff this frame was pushed by a function call. `local`
    /// only works inside a function frame.
    pub is_function_frame: bool,
    /// True iff the function that owns this frame was defined with
    /// the ksh93 `function NAME` keyword form. Static-scoped frames
    /// treat *every* top-level assignment inside them as `local` —
    /// they don't bleed back into outer frames.
    pub static_scope: bool,
}

impl<B: MapBackend> Default for Frame<B> {
    fn default() -> Self {
        Self {
            bindings: <B::Map<String, Binding> as Default>::default(),
            is_function_frame: false,
            static_scope: false,
        }
    }
}

impl<B: MapBackend> Clone for Frame<B>
where
    B::Map<String, Binding>: Clone,
{
    fn clone(&self) -> Self {
        Self {
            bindings: self.bindings.clone(),
            is_function_frame: self.is_function_frame,
            static_scope: self.static_scope,
        }
    }
}

impl<B: MapBackend> core::fmt::Debug for Frame<B>
where
    B::Map<String, Binding>: core::fmt::Debug,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Frame")
            .field("bindings", &self.bindings)
            .field("is_function_frame", &self.is_function_frame)
            .field("static_scope", &self.static_scope)
            .finish()
    }
}

/// Stack of [`Frame`]s. Generic over the [`MapBackend`].
pub struct Scope<B: MapBackend = BTreeBackend> {
    frames: Vec<Frame<B>>,
}

impl<B: MapBackend> Clone for Scope<B>
where
    Frame<B>: Clone,
{
    fn clone(&self) -> Self {
        Self {
            frames: self.frames.clone(),
        }
    }
}

impl<B: MapBackend> core::fmt::Debug for Scope<B>
where
    Frame<B>: core::fmt::Debug,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Scope").field("frames", &self.frames).finish()
    }
}

impl<B: MapBackend> Scope<B> {
    /// New scope with a single (non-function) root frame.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self {
            frames: vec![Frame::<B>::default()],
        }
    }

    /// Push a fresh non-function frame (e.g. for a brace group or
    /// subshell isolation).
    #[inline]
    pub fn push(&mut self) {
        self.frames.push(Frame::<B>::default());
    }

    /// Push a function frame. `static_scope = true` for ksh93
    /// `function`-form functions (kash inherits this rule per
    /// `project_shell_function_scope.md`), `false` for POSIX
    /// `name()`-form functions.
    #[inline]
    pub fn push_function_frame(&mut self, static_scope: bool) {
        self.frames.push(Frame::<B> {
            bindings: <B::Map<String, Binding> as Default>::default(),
            is_function_frame: true,
            static_scope,
        });
    }

    /// Pop the topmost frame. The root frame is never popped.
    #[inline]
    pub fn pop(&mut self) {
        if self.frames.len() > 1 {
            self.frames.pop();
        }
    }

    /// Number of frames currently on the stack (always `>= 1`).
    #[inline]
    #[must_use]
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// True iff the topmost frame is a function frame.
    #[inline]
    #[must_use]
    pub fn in_function(&self) -> bool {
        self.frames
            .last()
            .is_some_and(|f| f.is_function_frame)
    }

    /// Look up a name, walking top → bottom and returning the first
    /// hit.
    #[inline]
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        for frame in self.frames.iter().rev() {
            if let Some(b) = frame.bindings.get(name) {
                return Some(&b.value);
            }
        }
        None
    }

    /// Plain variable assignment, with the scope-resolution policy
    /// described in the module docs.
    pub fn assign(&mut self, name: &str, value: Value) -> Result<()> {
        for frame in &self.frames {
            if let Some(b) = frame.bindings.get(name)
                && b.readonly
            {
                return Err(KashError::Readonly(name.into()));
            }
        }
        if let Some(top) = self.frames.last()
            && top.is_function_frame
            && top.static_scope
        {
            let top = self.frames.last_mut().expect("just checked");
            top.bindings.insert(
                name.to_string(),
                Binding {
                    value,
                    readonly: false,
                },
            );
            return Ok(());
        }
        for i in (0..self.frames.len()).rev() {
            if self.frames[i].bindings.contains_key(name) {
                self.frames[i].bindings.insert(
                    name.to_string(),
                    Binding {
                        value,
                        readonly: false,
                    },
                );
                return Ok(());
            }
        }
        let root = self.frames.first_mut().expect("at least one frame");
        root.bindings.insert(
            name.to_string(),
            Binding {
                value,
                readonly: false,
            },
        );
        Ok(())
    }

    /// `local NAME[=VALUE]` — bind `name` in the topmost (function)
    /// frame, shadowing any outer binding.
    pub fn assign_local(&mut self, name: &str, value: Value) -> Result<()> {
        let top = self.frames.last_mut().expect("at least one frame");
        if let Some(b) = top.bindings.get(name)
            && b.readonly
        {
            return Err(KashError::Readonly(name.into()));
        }
        top.bindings.insert(
            name.to_string(),
            Binding {
                value,
                readonly: false,
            },
        );
        Ok(())
    }

    /// `readonly NAME[=VALUE]` — mark `name` read-only.
    pub fn mark_readonly(&mut self, name: &str, value: Option<Value>) -> Result<()> {
        for i in (0..self.frames.len()).rev() {
            if let Some(b) = self.frames[i].bindings.get_mut(name) {
                if b.readonly && value.is_some() {
                    return Err(KashError::Readonly(name.into()));
                }
                if let Some(v) = value {
                    b.value = v;
                }
                b.readonly = true;
                return Ok(());
            }
        }
        let root = self.frames.first_mut().expect("at least one frame");
        root.bindings.insert(
            name.to_string(),
            Binding {
                value: value.unwrap_or_default(),
                readonly: true,
            },
        );
        Ok(())
    }

    /// Remove the nearest binding of `name`. Returns `true` if a
    /// binding existed.
    pub fn unset(&mut self, name: &str) -> bool {
        for i in (0..self.frames.len()).rev() {
            if let Some(b) = self.frames[i].bindings.get(name) {
                if b.readonly {
                    return false;
                }
                self.frames[i].bindings.remove(name);
                return true;
            }
        }
        false
    }

    /// True iff `name` is currently bound as `readonly` anywhere on
    /// the stack.
    #[inline]
    #[must_use]
    pub fn is_readonly(&self, name: &str) -> bool {
        for frame in &self.frames {
            if let Some(b) = frame.bindings.get(name)
                && b.readonly
            {
                return true;
            }
        }
        false
    }
}

impl<B: MapBackend> Default for Scope<B> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_frame_persists() {
        let mut s = Scope::<BTreeBackend>::new();
        s.assign("FOO", Value::scalar("bar")).unwrap();
        assert_eq!(s.get("FOO").unwrap().to_scalar_string(), "bar");
    }

    #[test]
    fn assign_walks_to_outer_existing_binding() {
        let mut s = Scope::<BTreeBackend>::new();
        s.assign("X", Value::scalar("outer")).unwrap();
        s.push();
        s.assign("X", Value::scalar("from_inner")).unwrap();
        s.pop();
        assert_eq!(s.get("X").unwrap().to_scalar_string(), "from_inner");
    }

    #[test]
    fn assign_local_isolates_to_function_frame() {
        let mut s = Scope::<BTreeBackend>::new();
        s.assign("X", Value::scalar("outer")).unwrap();
        s.push_function_frame(false);
        s.assign_local("X", Value::scalar("inner")).unwrap();
        assert_eq!(s.get("X").unwrap().to_scalar_string(), "inner");
        s.pop();
        assert_eq!(s.get("X").unwrap().to_scalar_string(), "outer");
    }

    #[test]
    fn static_scope_makes_assign_local_by_default() {
        let mut s = Scope::<BTreeBackend>::new();
        s.assign("X", Value::scalar("outer")).unwrap();
        s.push_function_frame(true);
        s.assign("X", Value::scalar("inner")).unwrap();
        assert_eq!(s.get("X").unwrap().to_scalar_string(), "inner");
        s.pop();
        assert_eq!(s.get("X").unwrap().to_scalar_string(), "outer");
    }

    #[test]
    fn dynamic_scope_propagates_to_caller() {
        let mut s = Scope::<BTreeBackend>::new();
        s.assign("X", Value::scalar("outer")).unwrap();
        s.push_function_frame(false);
        s.assign("X", Value::scalar("from_dynamic")).unwrap();
        s.pop();
        assert_eq!(s.get("X").unwrap().to_scalar_string(), "from_dynamic");
    }

    #[test]
    fn readonly_blocks_subsequent_assignment() {
        let mut s = Scope::<BTreeBackend>::new();
        s.mark_readonly("X", Some(Value::scalar("fixed"))).unwrap();
        let err = s.assign("X", Value::scalar("nope")).unwrap_err();
        assert!(matches!(err, KashError::Readonly(_)));
        assert_eq!(s.get("X").unwrap().to_scalar_string(), "fixed");
    }

    #[test]
    fn readonly_creates_empty_when_absent() {
        let mut s = Scope::<BTreeBackend>::new();
        s.mark_readonly("X", None).unwrap();
        assert!(s.is_readonly("X"));
        assert_eq!(s.get("X").unwrap().to_scalar_string(), "");
    }

    #[test]
    fn local_can_shadow_outer_readonly() {
        let mut s = Scope::<BTreeBackend>::new();
        s.mark_readonly("X", Some(Value::scalar("locked"))).unwrap();
        s.push_function_frame(false);
        s.assign_local("X", Value::scalar("shadow")).unwrap();
        assert_eq!(s.get("X").unwrap().to_scalar_string(), "shadow");
        s.pop();
        assert_eq!(s.get("X").unwrap().to_scalar_string(), "locked");
        assert!(s.is_readonly("X"));
    }

    #[test]
    fn unset_removes_binding_and_refuses_readonly() {
        let mut s = Scope::<BTreeBackend>::new();
        s.assign("X", Value::scalar("v")).unwrap();
        assert!(s.unset("X"));
        assert!(s.get("X").is_none());
        s.mark_readonly("Y", Some(Value::scalar("v"))).unwrap();
        assert!(!s.unset("Y"));
        assert!(s.get("Y").is_some());
    }

    #[test]
    fn in_function_reports_frame_kind() {
        let mut s = Scope::<BTreeBackend>::new();
        assert!(!s.in_function());
        s.push();
        assert!(!s.in_function());
        s.pop();
        s.push_function_frame(false);
        assert!(s.in_function());
    }
}
