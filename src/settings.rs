//! Persisted non-secret OS settings: the AI policy grants matrix, the
//! unsafe-mode flag, and the operator's real-filesystem mounts. These survive a
//! kerneld restart so an operator's tuning of what the AI may do — and which
//! real directories webOS may read — isn't silently reset to defaults on boot.
//!
//! Stored as plaintext JSON at `sandbox/settings.json` (it holds NO secrets —
//! only capability names mapped to allow/deny/ask, a boolean, and a list of
//! canonical absolute directory paths). The mount paths are NOT secret: they are
//! operator-chosen directory locations, never credentials. Connectors and apps
//! keep their own per-file persistence; this file is only grants + unsafe_mode +
//! mounts.

use crate::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

fn settings_path() -> PathBuf {
    crate::caps::root_dir().join("settings.json")
}

/// Read persisted settings at boot. Returns `(grants, unsafe_mode, mounts)`.
/// When the file is missing or unreadable we fall back to the compiled defaults
/// (and an empty mount list) so a fresh install behaves exactly as before — with
/// no mounts, the real filesystem is entirely unreachable.
pub fn load() -> (HashMap<String, String>, bool, Vec<String>) {
    let defaults = crate::policy::default_grants();
    let txt = match std::fs::read_to_string(settings_path()) {
        Ok(t) => t,
        Err(_) => return (defaults, false, Vec::new()),
    };
    let v: Value = match serde_json::from_str(&txt) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("settings.json unreadable ({e}); using defaults");
            return (defaults, false, Vec::new());
        }
    };

    // Start from defaults, then overlay any persisted grants. A persisted file
    // that predates a newly-added capability still gets that cap's default.
    let mut grants = defaults;
    if let Some(map) = v.get("grants").and_then(|g| g.as_object()) {
        for (k, val) in map {
            if let Some(s) = val.as_str() {
                if matches!(s, "allow" | "deny" | "ask") {
                    grants.insert(k.clone(), s.to_string());
                }
            }
        }
    }
    let unsafe_mode = v.get("unsafe_mode").and_then(|u| u.as_bool()).unwrap_or(false);

    // Mounts are stored as canonical absolute paths. Re-validate at boot: keep
    // only paths that are still absolute, still exist, and are still directories
    // (a mount whose target was deleted/replaced since the last run is dropped,
    // never silently widening access). Re-canonicalize to collapse any symlink
    // that may have appeared, then de-dupe.
    let mut mounts: Vec<String> = Vec::new();
    if let Some(arr) = v.get("mounts").and_then(|m| m.as_array()) {
        for item in arr {
            if let Some(s) = item.as_str() {
                let p = std::path::Path::new(s);
                if !p.is_absolute() {
                    continue;
                }
                match std::fs::canonicalize(p) {
                    Ok(canon) if canon.is_dir() => {
                        let canon = canon.to_string_lossy().to_string();
                        if !mounts.contains(&canon) {
                            mounts.push(canon);
                        }
                    }
                    _ => {
                        tracing::warn!("dropping mount '{s}' at boot (missing or not a directory)");
                    }
                }
            }
        }
    }
    (grants, unsafe_mode, mounts)
}

/// Write-through: snapshot the live grants + unsafe_mode + mounts to disk.
/// Called after every mutation (policy.set / policy.set_unsafe /
/// connector.remove / mount.add / mount.remove). Best effort — a write failure
/// is logged but never blocks the in-memory update.
pub fn save(state: &AppState) {
    let grants = state.grants.lock().unwrap().clone();
    let unsafe_mode = state.unsafe_mode.load(Ordering::Relaxed);
    let mounts = state.mounts.lock().unwrap().clone();
    let body = json!({ "grants": grants, "unsafe_mode": unsafe_mode, "mounts": mounts });
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(&body) {
        Ok(s) => {
            if let Err(e) = std::fs::write(&path, s) {
                tracing::warn!("could not persist settings.json: {e}");
            }
        }
        Err(e) => tracing::warn!("could not serialize settings: {e}"),
    }
}
