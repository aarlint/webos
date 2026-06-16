//! Connector LIBRARY — connectors as declarative config-as-data the operator
//! browses and installs, instead of hand-authoring a `ConnectorDef` envelope.
//!
//! A library entry is a JSON *manifest*: a connector descriptor (the same fields
//! `connectors::ConnectorDef` carries) PLUS catalog metadata — a human `name`, a
//! `description`, an `icon` name from the inline UI icon set, and an optional
//! `requires_cred` block naming the ONE credential the operator must store for
//! the connector's auth to resolve.
//!
//! Manifests are bundled, read-only DATA shipped in `library/*.json` at the repo
//! root (overridable with `WEBOS_LIBRARY_DIR`). They are loaded on demand, never
//! mutated, and carry NO secrets — `requires_cred.name` and `auth.cred_ref` are
//! only NAMES into the sealed credential store (secrets.rs), never values.
//!
//! Two governed verbs sit on top:
//!   * `library.list` (GOVERNABLE) — the catalog, each entry tagged `installed`.
//!   * `library.install` (PROTECTED, human-only) — derive a `ConnectorDef` from a
//!     manifest and persist it through `connectors::add`, so the installed
//!     connector immediately appears in `connector.list` and is callable through
//!     the one governed verb `conn.call`. Install only ever references a
//!     credential by NAME; it stores no secret.

use crate::connectors::{ConnectorAuth, ConnectorDef, OpDef};
use crate::AppState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

/// The named credential a manifest's auth depends on. Surfaced to the operator
/// so the Settings UI can prompt "set credential <name> in Credentials". Holds
/// only the credential NAME + human labels — never a secret value.
#[derive(Serialize, Deserialize, Clone)]
pub struct RequiresCred {
    /// The name the operator must store under Settings → Credentials. Must match
    /// the connector's `auth.cred_ref` so the auth resolves at egress time.
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub help: String,
}

/// A library manifest = a connector descriptor + catalog metadata. The connector
/// fields mirror `ConnectorDef`; the metadata fields drive the browse UI.
#[derive(Serialize, Deserialize, Clone)]
pub struct Manifest {
    pub id: String,
    /// Human display name (becomes the connector's `display_name` on install).
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Icon name from the inline UI icon set (git/database/cloud/chart/mail/…).
    #[serde(default)]
    pub icon: String,
    #[serde(default = "manual_rest")]
    pub kind: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    #[serde(default)]
    pub ops: Vec<OpDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ConnectorAuth>,
    /// The single credential this connector's auth needs; omitted for no-auth
    /// connectors. NAME + labels only — never a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_cred: Option<RequiresCred>,
    /// Only meaningful when `kind == "mcp"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<crate::mcp::McpTransport>,
}
fn manual_rest() -> String {
    "manual_rest".into()
}

impl Manifest {
    /// Project the catalog-only metadata away and produce the `ConnectorDef` that
    /// `connectors::add` validates and persists. `requires_cred`/`description`/
    /// `icon`/`name` are library-only and do not exist on `ConnectorDef`.
    fn to_connector_def(&self) -> ConnectorDef {
        ConnectorDef {
            id: self.id.clone(),
            display_name: self.name.clone(),
            kind: self.kind.clone(),
            base_url: self.base_url.clone(),
            allowed_hosts: self.allowed_hosts.clone(),
            ops: self.ops.clone(),
            auth: self.auth.clone(),
            transport: self.transport.clone(),
        }
    }
}

/// Directory the bundled manifests live in. Defaults to `library/` relative to
/// the kerneld working directory (the repo root, same as the `web/` ServeDir),
/// overridable with `WEBOS_LIBRARY_DIR` for tests / packaging.
fn dir() -> PathBuf {
    std::env::var("WEBOS_LIBRARY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("library"))
}

/// Load every well-formed manifest from the library directory. Malformed files
/// are skipped with a warning (never fatal); a missing directory yields an empty
/// catalog. Sorted by id for a stable listing.
pub fn load_all() -> Vec<Manifest> {
    let mut out: Vec<Manifest> = Vec::new();
    let d = dir();
    if let Ok(rd) = std::fs::read_dir(&d) {
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(txt) => match serde_json::from_str::<Manifest>(&txt) {
                    Ok(m) => out.push(m),
                    Err(err) => tracing::warn!("skipping library manifest {}: {err}", path.display()),
                },
                Err(err) => tracing::warn!("could not read library manifest {}: {err}", path.display()),
            }
        }
    } else {
        tracing::info!("connector library dir {} not found (empty catalog)", d.display());
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

// ── governable: browse the catalog ──────────────────────────────────────────--

/// `library.list {}` → the catalog. Each entry is tagged `installed` by checking
/// the live connector registry for a connector of the same id, so the UI can
/// show installed state without a second round-trip. No secrets, no op detail —
/// just what the browse grid needs.
pub fn list(state: &AppState) -> Result<Value, String> {
    let installed = state.connectors.lock().unwrap();
    let items: Vec<Value> = load_all()
        .into_iter()
        .map(|m| {
            json!({
                "id": m.id,
                "name": m.name,
                "description": m.description,
                "icon": m.icon,
                "kind": m.kind,
                "requires_cred": m.requires_cred,
                "installed": installed.contains_key(&m.id),
            })
        })
        .collect();
    Ok(json!({ "connectors": items }))
}

// ── operator-only: install a manifest as a real connector ──────────────────────

/// `library.install { id }` → derive a `ConnectorDef` from the named manifest and
/// persist it through `connectors::add` (which re-validates the descriptor and
/// writes it to the connector jail). PROTECTED → human principal only; the AI can
/// never mint a connector even under unsafe_mode (see policy::PROTECTED).
///
/// Returns `{ installed, requires_cred? }`. Install stores NO secret: it only
/// references the credential by NAME, surfacing `requires_cred` so the operator
/// knows which credential to store next for the connector's auth to resolve.
pub fn install(args: &Value, state: &AppState) -> Result<Value, String> {
    let id = args.get("id").and_then(|v| v.as_str()).ok_or("id required")?;
    let manifest = load_all()
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| format!("unknown library connector '{id}'"))?;

    // Build the ConnectorDef and route it through the SAME validating, persisting
    // path the operator's hand-authored connector.add uses — no privileged
    // shortcut. connectors::add enforces the slug/reserved-prefix rules, the
    // https floor, mcp-transport checks, and auth-block consistency.
    let def = manifest.to_connector_def();
    let def_value = serde_json::to_value(&def).map_err(|e| e.to_string())?;
    crate::connectors::add(&def_value, state)?;

    let mut out = json!({ "installed": id });
    if let Some(rc) = &manifest.requires_cred {
        out["requires_cred"] = serde_json::to_value(rc).map_err(|e| e.to_string())?;
    }
    Ok(out)
}
