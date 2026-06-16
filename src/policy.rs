//! The policy gate. Humans are root (local console); the AI is governed by
//! grants (allow / deny / ask) with ungoverned actions defaulting to ASK, plus a
//! hard privilege floor the AI can never cross — even in unsafe mode.
//!
//! Grants are keyed on a "gate key": for ordinary capabilities that is just the
//! capability name; for `conn.call` it is the args-aware, op-hash-pinned key
//! `conn.call:<connector>:<op>:<class>:<op_hash>` (see connectors::gate_key_and_meta).

use std::collections::HashMap;

pub enum Decision {
    Allow,
    Deny(String),
    Ask,
}

/// Operator-only — never reachable by the AI, even under unsafe_mode. These let
/// the AI escalate privileges, read secrets, mint/arm connectors, or approve its
/// own prompts.
pub const PROTECTED: &[&str] = &[
    "policy.set",
    "policy.set_unsafe",
    "creds.set",
    "creds.delete",
    "creds.list",
    "approval.resolve",
    "connector.add",
    "connector.remove",
    "connector.connect",
    "connector.disconnect",
    "connector.refresh_tools",
    // Installing a library connector mints + arms a real connector (it persists a
    // ConnectorDef and may wire it to a credential). That is the same operator
    // privilege as connector.add — the AI can never install one, even unsafe.
    "library.install",
    "app.save",
    "app.delete",
    "routine.set",
    "routine.delete",
    // Mounting/unmounting a REAL filesystem directory is an operator privilege:
    // it is the act of widening webOS's reach beyond the sandbox. The AI can
    // never add or remove a mount, even under unsafe_mode. (Reading WITHIN a
    // mount is the governable files.* tier below.)
    "mount.add",
    "mount.remove",
];

/// Non-protected capabilities shown in the Settings permission matrix.
pub const GOVERNABLE: &[&str] = &[
    "sys.info",
    "fs.read",
    "fs.list",
    "fs.write",
    // Real-filesystem reads inside an operator's mounts. Distinct from the
    // sandbox-jailed fs.* caps: these reach actual user files, so the AI must
    // be prompted (default "ask" below) for each new op rather than blanket
    // allowed. The human (root) reads freely.
    "files.read",
    "files.list",
    // Read-only listing of the configured mounts (names/paths only, no file
    // contents). Governable so an operator can hide even the mount inventory
    // from the AI if they wish; defaults to "ask".
    "mount.list",
    "weather.get",
    "ai.compose",
    "ui.get",
    "ui.render",
    "ui.patch",
    "ui.table",
    "ui.chart",
    "ui.board",
    "ui.surface",
    "connector.list",
    "connector.describe",
    "library.list",
    "app.list",
    "routine.list",
];

pub fn default_grants() -> HashMap<String, String> {
    [
        ("sys.info", "allow"),
        ("fs.read", "allow"),
        ("fs.list", "allow"),
        ("weather.get", "allow"),
        ("ui.get", "allow"),
        ("ui.render", "allow"),
        ("ui.patch", "allow"),
        ("ui.table", "allow"),
        ("ui.chart", "allow"),
        ("ui.board", "allow"),
        ("ui.surface", "allow"),
        ("ai.compose", "allow"),
        ("connector.list", "allow"),
        ("connector.describe", "allow"),
        ("library.list", "allow"),
        ("app.list", "allow"),
        ("routine.list", "allow"),
        ("fs.write", "deny"),
        // Real-file access posture: the AI must ASK the operator on every new
        // real-FS op. Unlike sandbox fs.read/fs.list (which are local, walled
        // data and default-allow), files.read/files.list touch the operator's
        // actual documents — so the default is "ask", routing each op through
        // the consent gate. mount.list is also "ask" so the AI cannot even
        // enumerate the operator's mounted directories without a prompt. The
        // human principal bypasses all of this (human == allow in decide()).
        ("files.read", "ask"),
        ("files.list", "ask"),
        ("mount.list", "ask"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// `cap` is the raw capability (for the protected check); `gate_key` is what
/// grants are looked up / persisted under.
pub fn decide(
    principal: &str,
    cap: &str,
    gate_key: &str,
    grants: &HashMap<String, String>,
    unsafe_mode: bool,
) -> Decision {
    match principal {
        "human" => Decision::Allow,

        "ai" => {
            if PROTECTED.contains(&cap) {
                return Decision::Deny(format!("'{cap}' is operator-only"));
            }
            // Write-class connector ops are NEVER blanket-allowed by unsafe_mode;
            // they still require an explicit grant or a fresh ASK.
            let is_write_conn = gate_key.starts_with("conn.call:") && gate_key.contains(":write:");
            if unsafe_mode && !is_write_conn {
                return Decision::Allow;
            }
            match grants.get(gate_key).map(String::as_str) {
                Some("allow") => Decision::Allow,
                Some("deny") => Decision::Deny(format!("ai denied '{gate_key}' by policy")),
                _ => Decision::Ask,
            }
        }

        other => Decision::Deny(format!("unknown principal '{other}'")),
    }
}
