//! The closed tool surface exposed to the model, and the capability each tool carries.
//!
//! Every tool operates on opaque handles (connection ids, paths) — there is deliberately **no** tool
//! that returns or accepts a secret, makes arbitrary network calls, or escalates scope. A model that
//! hallucinates such a call is rejected by [`capability_for`] returning `None`. Capability
//! containment, not model compliance, is the security boundary (LLD §10.2).

/// The action a tool performs, used for confirm gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// Read-only listing.
    List,
    /// Read-only metadata.
    Stat,
    /// Read file contents.
    Read,
    /// Copy (non-destructive).
    Copy,
    /// Move (recoverable).
    Move,
    /// Delete (irreversible).
    Delete,
    /// Execute a command (irreversible / side-effecting).
    Exec,
    /// Open a connection (the broker resolves credentials internally).
    OpenConnection,
}

/// How reversible an action is — drives confirmation requirements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reversibility {
    /// No state change, or trivially reversible.
    Safe,
    /// Mutates state but is recoverable (e.g. move).
    Recoverable,
    /// Permanent / not recoverable (e.g. delete, exec).
    Irreversible,
}

/// A tool's capability: what it does and how reversible it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    /// The verb.
    pub verb: Verb,
    /// The reversibility.
    pub reversibility: Reversibility,
}

/// The set of tool names the model may use. This list is the entire surface — nothing else is
/// callable.
pub const TOOLS: &[&str] = &[
    "list",
    "stat",
    "read",
    "copy",
    "move",
    "delete",
    "exec",
    "open_connection",
];

/// Map a tool name to its [`Capability`], or `None` if the name is not in the closed set.
#[must_use]
pub fn capability_for(tool: &str) -> Option<Capability> {
    use Reversibility::{Irreversible, Recoverable, Safe};
    use Verb::{Copy, Delete, Exec, List, Move, OpenConnection, Read, Stat};
    let (verb, reversibility) = match tool {
        "list" => (List, Safe),
        "stat" => (Stat, Safe),
        "read" => (Read, Safe),
        "open_connection" => (OpenConnection, Safe),
        "copy" => (Copy, Safe),
        "move" => (Move, Recoverable),
        "delete" => (Delete, Irreversible),
        "exec" => (Exec, Irreversible),
        _ => return None,
    };
    Some(Capability {
        verb,
        reversibility,
    })
}

/// Whether a step with this capability may be approved in bulk (`Safe`/`Recoverable` only).
#[must_use]
pub fn allows_bulk_approve(cap: Capability) -> bool {
    matches!(
        cap.reversibility,
        Reversibility::Safe | Reversibility::Recoverable
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tools_have_capabilities() {
        for t in TOOLS {
            assert!(capability_for(t).is_some(), "{t} should be known");
        }
    }

    #[test]
    fn unknown_tool_is_rejected() {
        assert!(capability_for("read_secret").is_none());
        assert!(capability_for("http_fetch").is_none());
        assert!(capability_for("escalate").is_none());
    }

    #[test]
    fn destructive_tools_are_irreversible() {
        assert_eq!(
            capability_for("delete").unwrap().reversibility,
            Reversibility::Irreversible
        );
        assert_eq!(
            capability_for("exec").unwrap().reversibility,
            Reversibility::Irreversible
        );
        assert!(!allows_bulk_approve(capability_for("delete").unwrap()));
        assert!(allows_bulk_approve(capability_for("copy").unwrap()));
        assert!(allows_bulk_approve(capability_for("move").unwrap()));
    }
}
