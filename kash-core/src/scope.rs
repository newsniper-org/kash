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

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::collections::{BTreeBackend, MapBackend, MapStorage};
use crate::error::{KashError, Result};
use crate::value::Value;

/// One variable binding. The value travels with its full `typeset`
/// attribute set so assignment guards and read-side transformations
/// can short-circuit on a single map lookup.
#[derive(Clone, Debug, Default)]
pub struct Binding {
    /// The bound value.
    pub value: Value,
    /// `typeset`-style attribute set.
    pub attrs: AttrSet,
}

impl Binding {
    /// True if this binding is `readonly`-attributed. Short-hand for
    /// `b.attrs.readonly` so existing call sites stay terse.
    #[inline]
    #[must_use]
    pub fn readonly(&self) -> bool {
        self.attrs.readonly
    }
}

/// Built-in primitive integer types — the half of kash's numeric
/// type set that lands in this commit. Float and complex variants
/// come in a follow-up. Stored on [`AttrSet::numeric_type`]; the
/// arithmetic engine wraps store-time values to the type's range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumericType {
    /// 8-bit signed.
    Int8,
    /// 16-bit signed.
    Int16,
    /// 32-bit signed.
    Int32,
    /// 64-bit signed (the default for `typeset -i`).
    Int64,
    /// 128-bit signed.
    Int128,
    /// 8-bit unsigned.
    UInt8,
    /// 16-bit unsigned.
    UInt16,
    /// 32-bit unsigned.
    UInt32,
    /// 64-bit unsigned.
    UInt64,
    /// 128-bit unsigned (stored as `i128`; values above
    /// `i128::MAX` round-trip into negative, matching the
    /// shell's wrap-on-overflow policy).
    UInt128,
    /// IEEE 754 binary16 (`f16` via the `half` crate). Half
    /// precision — round-tripped through `half::f16` on store.
    Float16,
    /// IEEE 754 binary32 (`f32`).
    Float32,
    /// IEEE 754 binary64 (`f64`) — kash's default float.
    Float64,
    /// Google Brain bfloat16 (`bf16` via the `half` crate) —
    /// same exponent as `f32`, narrower mantissa, ML-friendly.
    BFloat16,
}

impl NumericType {
    /// Parse the kash spelling of a primitive integer type.
    /// Returns `None` for anything we don't (yet) recognise.
    #[must_use]
    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "int8" => Self::Int8,
            "int16" => Self::Int16,
            "int32" => Self::Int32,
            "int64" => Self::Int64,
            "int128" => Self::Int128,
            "uint8" => Self::UInt8,
            "uint16" => Self::UInt16,
            "uint32" => Self::UInt32,
            "uint64" => Self::UInt64,
            "uint128" => Self::UInt128,
            "float16" => Self::Float16,
            "float32" => Self::Float32,
            "float64" => Self::Float64,
            "bfloat16" => Self::BFloat16,
            _ => return None,
        })
    }

    /// True iff this type is an integer (not a float). Drives the
    /// "wrap on store" branch in [`crate::eval::Evaluator`].
    #[must_use]
    pub fn is_integer(self) -> bool {
        matches!(
            self,
            Self::Int8
                | Self::Int16
                | Self::Int32
                | Self::Int64
                | Self::Int128
                | Self::UInt8
                | Self::UInt16
                | Self::UInt32
                | Self::UInt64
                | Self::UInt128
        )
    }

    /// True iff this type is a float.
    #[must_use]
    pub fn is_float(self) -> bool {
        !self.is_integer()
    }

    /// Wrap `v` into this type's representable range and return
    /// the canonical `i128` round-trip. Lossy on overflow — that
    /// matches ksh93's `typeset -i` wrap semantics and is what
    /// the `warn-integer-overflow` option flags. Float variants
    /// panic: callers must consult [`Self::is_integer`] first.
    #[must_use]
    pub fn wrap(self, v: i128) -> i128 {
        match self {
            Self::Int8 => (v as i8) as i128,
            Self::Int16 => (v as i16) as i128,
            Self::Int32 => (v as i32) as i128,
            Self::Int64 => (v as i64) as i128,
            Self::Int128 => v,
            Self::UInt8 => i128::from(v as u8),
            Self::UInt16 => i128::from(v as u16),
            Self::UInt32 => i128::from(v as u32),
            Self::UInt64 => i128::from(v as u64),
            Self::UInt128 => v,
            Self::Float16 | Self::Float32 | Self::Float64 | Self::BFloat16 => {
                panic!("NumericType::wrap called on a float type — use project_float")
            }
        }
    }

    /// Project a `f64` value into this float type's precision
    /// and return the round-trip. For `float64` this is a no-op;
    /// `float32` round-trips through `f32`; `float16` and
    /// `bfloat16` round-trip through `half::f16` and `half::bf16`
    /// respectively. Integer variants panic.
    #[must_use]
    pub fn project_float(self, v: f64) -> f64 {
        match self {
            Self::Float64 => v,
            Self::Float32 => v as f32 as f64,
            Self::Float16 => f64::from(half::f16::from_f64(v)),
            Self::BFloat16 => f64::from(half::bf16::from_f64(v)),
            _ => panic!("NumericType::project_float called on an integer type"),
        }
    }

    /// Canonical lowercase name (`"int32"`, `"uint8"`, …).
    /// Symmetric to [`Self::from_name`].
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Int8 => "int8",
            Self::Int16 => "int16",
            Self::Int32 => "int32",
            Self::Int64 => "int64",
            Self::Int128 => "int128",
            Self::UInt8 => "uint8",
            Self::UInt16 => "uint16",
            Self::UInt32 => "uint32",
            Self::UInt64 => "uint64",
            Self::UInt128 => "uint128",
            Self::Float16 => "float16",
            Self::Float32 => "float32",
            Self::Float64 => "float64",
            Self::BFloat16 => "bfloat16",
        }
    }
}

/// `typeset`-style attribute set. Variant fields cover the ksh93
/// surface we ship in this commit; the rest of the ksh93 surface
/// (`-n` nameref, `-T` user-defined types) is named but inactive in
/// [`AttrSet::pending_nameref`] / [`AttrSet::pending_typedef`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttrSet {
    /// `-r` — readonly. Further mutation is refused.
    pub readonly: bool,
    /// `-x` — exported. Surfaces in the environment of an external
    /// command spawned through this scope.
    pub export: bool,
    /// `-i` — integer. Assigned strings are evaluated as arithmetic
    /// before being stored.
    pub integer: bool,
    /// `-l` — lower-case. Stored value is folded to lower case.
    pub lowercase: bool,
    /// `-u` — upper-case. Stored value is folded to upper case.
    pub uppercase: bool,
    /// `-a` — indexed array. The binding's [`Value`] should be
    /// `Value::Array` (assignments coerce if needed).
    pub indexed: bool,
    /// `-A` — associative array. The binding's [`Value`] should be
    /// `Value::AssocArray`.
    pub assoc: bool,
    /// `-n` — nameref. Holds the target variable name; lookups are
    /// transparently routed there. **Not yet wired** in the
    /// evaluator (parser / `typeset` accept it; storage is just a
    /// string).
    pub pending_nameref: Option<String>,
    /// `-T` — user-defined type. Holds the type's name. **Not yet
    /// wired**; reserved for the typeclass / OOP integration.
    pub pending_typedef: Option<String>,
    /// Primitive numeric type the binding has been declared as
    /// (`int32`, `uint8`, …). Drives store-time arithmetic wrap
    /// and (when `warn-integer-overflow` is on) the overflow
    /// warning. `None` for untyped string variables. Implies
    /// `integer = true` for the assignment path.
    pub numeric_type: Option<NumericType>,
}

impl AttrSet {
    /// Merge `other`'s set bits into `self`. Attribute setters are
    /// idempotent and additive; clearing happens via `+letter`-form
    /// builtin args, which take a separate code path.
    #[inline]
    pub fn merge(&mut self, other: &AttrSet) {
        self.readonly |= other.readonly;
        self.export |= other.export;
        self.integer |= other.integer;
        self.lowercase |= other.lowercase;
        self.uppercase |= other.uppercase;
        self.indexed |= other.indexed;
        self.assoc |= other.assoc;
        if other.pending_nameref.is_some() {
            self.pending_nameref = other.pending_nameref.clone();
        }
        if other.pending_typedef.is_some() {
            self.pending_typedef = other.pending_typedef.clone();
        }
        if other.numeric_type.is_some() {
            self.numeric_type = other.numeric_type;
        }
    }

    /// Transform `value` according to whatever read/write
    /// projections this attribute set asks for: `-l` lower-cases,
    /// `-u` upper-cases, `-i` runs the string through arithmetic
    /// evaluation. The order is `integer → case`, matching ksh93's
    /// store-time semantics.
    #[must_use]
    pub fn project_on_store(&self, value: String) -> String {
        if self.uppercase {
            value.to_uppercase()
        } else if self.lowercase {
            value.to_lowercase()
        } else {
            value
        }
    }
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
    /// described in the module docs. Attributes on an existing
    /// binding are preserved.
    pub fn assign(&mut self, name: &str, value: Value) -> Result<()> {
        for frame in &self.frames {
            if let Some(b) = frame.bindings.get(name)
                && b.readonly()
            {
                return Err(KashError::Readonly(name.into()));
            }
        }
        if let Some(top) = self.frames.last()
            && top.is_function_frame
            && top.static_scope
        {
            let top = self.frames.last_mut().expect("just checked");
            Self::write_or_create(top, name, value);
            return Ok(());
        }
        for i in (0..self.frames.len()).rev() {
            if self.frames[i].bindings.contains_key(name) {
                Self::write_or_create(&mut self.frames[i], name, value);
                return Ok(());
            }
        }
        let root = self.frames.first_mut().expect("at least one frame");
        Self::write_or_create(root, name, value);
        Ok(())
    }

    /// Helper for `assign` / `assign_local`: if the binding already
    /// exists, project `value` through the binding's attributes and
    /// overwrite it; otherwise insert a fresh binding with the
    /// default attribute set.
    fn write_or_create(frame: &mut Frame<B>, name: &str, value: Value) {
        if let Some(b) = frame.bindings.get_mut(name) {
            b.value = project_value(&b.attrs, value);
        } else {
            frame.bindings.insert(
                name.to_string(),
                Binding {
                    value,
                    attrs: AttrSet::default(),
                },
            );
        }
    }

    /// `local NAME[=VALUE]` — bind `name` in the topmost (function)
    /// frame, shadowing any outer binding.
    pub fn assign_local(&mut self, name: &str, value: Value) -> Result<()> {
        let top = self.frames.last_mut().expect("at least one frame");
        if let Some(b) = top.bindings.get(name)
            && b.readonly()
        {
            return Err(KashError::Readonly(name.into()));
        }
        Self::write_or_create(top, name, value);
        Ok(())
    }

    /// `readonly NAME[=VALUE]` — mark `name` read-only.
    pub fn mark_readonly(&mut self, name: &str, value: Option<Value>) -> Result<()> {
        for i in (0..self.frames.len()).rev() {
            if let Some(b) = self.frames[i].bindings.get_mut(name) {
                if b.readonly() && value.is_some() {
                    return Err(KashError::Readonly(name.into()));
                }
                if let Some(v) = value {
                    b.value = project_value(&b.attrs, v);
                }
                b.attrs.readonly = true;
                return Ok(());
            }
        }
        let root = self.frames.first_mut().expect("at least one frame");
        root.bindings.insert(
            name.to_string(),
            Binding {
                value: value.unwrap_or_default(),
                attrs: AttrSet {
                    readonly: true,
                    ..AttrSet::default()
                },
            },
        );
        Ok(())
    }

    /// Apply a `typeset`-style attribute set to `name`. Creates the
    /// binding with the default value (`Empty`) if it doesn't exist.
    /// Attributes are *added* to whatever was already there — the
    /// `+letter` clearing form takes a different code path.
    pub fn apply_attrs(&mut self, name: &str, attrs: &AttrSet) -> Result<()> {
        // Pick the topmost frame as the binding's home if it's new.
        let top = self.frames.last_mut().expect("at least one frame");
        if let Some(b) = top.bindings.get_mut(name) {
            b.attrs.merge(attrs);
            return Ok(());
        }
        // Walk down the stack looking for an existing binding.
        for i in (0..self.frames.len() - 1).rev() {
            if let Some(b) = self.frames[i].bindings.get_mut(name) {
                b.attrs.merge(attrs);
                return Ok(());
            }
        }
        // Brand new — install on the top frame with these attrs and an
        // empty / appropriately-shaped value.
        let value = initial_value_for(attrs);
        let top = self.frames.last_mut().expect("at least one frame");
        top.bindings.insert(
            name.to_string(),
            Binding {
                value,
                attrs: attrs.clone(),
            },
        );
        Ok(())
    }

    /// Get the binding (including attrs) for `name`. Walks the stack
    /// top → bottom like `get`.
    #[must_use]
    pub fn get_binding(&self, name: &str) -> Option<&Binding> {
        for frame in self.frames.iter().rev() {
            if let Some(b) = frame.bindings.get(name) {
                return Some(b);
            }
        }
        None
    }

    /// Mutable variant of [`Self::get_binding`].
    pub fn get_binding_mut(&mut self, name: &str) -> Option<&mut Binding> {
        for i in (0..self.frames.len()).rev() {
            if self.frames[i].bindings.contains_key(name) {
                return self.frames[i].bindings.get_mut(name);
            }
        }
        None
    }

    /// Remove the nearest binding of `name`. Returns `true` if a
    /// binding existed.
    pub fn unset(&mut self, name: &str) -> bool {
        for i in (0..self.frames.len()).rev() {
            if let Some(b) = self.frames[i].bindings.get(name) {
                if b.readonly() {
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
                && b.readonly()
            {
                return true;
            }
        }
        false
    }

    /// Iterate every reachable binding as `(name, &Binding)`, walking
    /// the stack bottom-to-top so newer (shadowing) bindings appear
    /// later. Used by `typeset -p` and the export-environment builder.
    pub fn all_bindings(&self) -> impl Iterator<Item = (&String, &Binding)> + '_ {
        self.frames
            .iter()
            .flat_map(|f| f.bindings.iter())
    }
}

/// Decide a starting value for a brand-new binding stamped only with
/// attributes (`typeset -a foo`, `typeset -A foo`, …). Indexed and
/// associative shapes need the right `Value` variant pre-installed so
/// later `arr[i]=...` assignments don't have to re-shape.
fn initial_value_for(attrs: &AttrSet) -> Value {
    if attrs.indexed {
        Value::Array(Vec::new())
    } else if attrs.assoc {
        Value::AssocArray(BTreeMap::new())
    } else {
        Value::Empty
    }
}

/// Apply the attribute-aware projections that should run on every
/// store: `-l` lower-cases, `-u` upper-cases, `-i` runs the value
/// through arithmetic evaluation (TODO: wired up in the evaluator,
/// not here). Array values pass through unchanged in this commit —
/// per-element transformations land in a follow-up.
fn project_value(attrs: &AttrSet, value: Value) -> Value {
    match value {
        Value::Scalar(s) => Value::Scalar(attrs.project_on_store(s)),
        other => other,
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
