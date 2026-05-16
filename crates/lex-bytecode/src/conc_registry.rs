//! Process-global named-actor registry (#444).
//!
//! Keys are user-chosen `String` names; values are `Value::Actor`
//! handles. The registry is flat (one namespace per process) — if two
//! libraries need to avoid name collisions they prefix the string
//! themselves. Access is serialised through a single `Mutex` because
//! register/lookup/unregister all touch the same `HashMap`; contention
//! is expected to be negligible (actor wiring happens once at
//! `main()`, lookups serialise with the actor's own mutex anyway).
//!
//! **Type tag.** v1 stores `Value::Actor` opaquely — `conc.lookup[S, M]`
//! parameterises the static return type but the runtime trusts the
//! registration site. Reified `(S, M)` SigId tagging at register/lookup
//! is the right design (see #444 option B) but requires plumbing
//! compile-time type info into bytecode constants — deferred to a
//! follow-up. The TypeMismatch variant of `ConcError` is reserved for
//! that future check.

use crate::value::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

fn registry() -> &'static Mutex<HashMap<String, Value>> {
    static REG: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register `actor` under `name`. Returns `Err` if the name is already
/// taken — registration is intentionally exclusive so the agent code
/// catches the "two actors fighting over one name" bug at the source
/// level rather than overwriting silently.
pub fn register(name: &str, actor: Value) -> Result<(), RegError> {
    let mut g = registry().lock().expect("conc registry poisoned");
    if g.contains_key(name) {
        return Err(RegError::AlreadyRegistered(name.to_string()));
    }
    g.insert(name.to_string(), actor);
    Ok(())
}

/// Look up an actor by name. Returns `None` if not registered.
pub fn lookup(name: &str) -> Option<Value> {
    let g = registry().lock().expect("conc registry poisoned");
    g.get(name).cloned()
}

/// Unregister by name. Returns `Err` if the name isn't registered.
/// Existing `Value::Actor` handles held by callers continue to work;
/// the actor cell is reclaimed when the last handle drops.
pub fn unregister(name: &str) -> Result<(), RegError> {
    let mut g = registry().lock().expect("conc registry poisoned");
    if g.remove(name).is_none() {
        return Err(RegError::NotRegistered(name.to_string()));
    }
    Ok(())
}

/// Snapshot the current registered names (debug / introspection).
pub fn registered() -> Vec<String> {
    let g = registry().lock().expect("conc registry poisoned");
    let mut names: Vec<String> = g.keys().cloned().collect();
    names.sort();
    names
}

#[derive(Debug, Clone)]
pub enum RegError {
    AlreadyRegistered(String),
    NotRegistered(String),
}

/// Reset the registry. Used by tests that need a clean slate; not
/// exposed to user code.
#[doc(hidden)]
pub fn _reset_for_tests() {
    let mut g = registry().lock().expect("conc registry poisoned");
    g.clear();
}
