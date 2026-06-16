//! Capability providers. Each verb wraps either a local system facility or an
//! external API. No third-party apps, no plugins — just first-party wrappers.
//! In the real OS these become provider modules; the dispatch table becomes a
//! registry that also feeds the AI's tool manifest and the UI's affordances.

use crate::{connectors, egress, model, policy, settings, AppState};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

pub fn root_dir() -> PathBuf {
    std::env::var("WEBOS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("sandbox"))
}

pub async fn dispatch(cap: &str, args: &Value, state: &AppState) -> Result<Value, String> {
    match cap {
        "sys.info" => sys_info(),
        "fs.read" => fs_read(args),
        "fs.write" => fs_write(args),
        "fs.list" => fs_list(args),
        "mount.add" => mount_add(args, state),
        "mount.list" => mount_list(state),
        "mount.remove" => mount_remove(args, state),
        "files.list" => files_list(args, state),
        "files.read" => files_read(args, state),
        "weather.get" => weather_get(args).await,
        "ui.get" => ui_get(args, state),
        "ui.render" => ui_render(args, state),
        "ui.patch" => ui_patch(args, state),
        "ui.table" => ui_table(args, state),
        "ui.chart" => ui_chart(args, state),
        "ui.board" => ui_board(args, state),
        "ui.master_detail" => ui_master_detail(args, state),
        "ui.surface" => ui_surface(args, state),
        "ai.compose" => Ok(model::compose(args, state).await),
        "chat.send" => crate::chat::send(args, state).await,
        "conn.call" => connectors::call(args, state).await,
        "connector.add" => connectors::add(args, state),
        "connector.remove" => connector_remove(args, state).await,
        "connector.list" => connectors::list(state),
        "connector.describe" => connectors::describe(args, state),
        "connector.connect" => crate::mcp::connect(args, state).await,
        "connector.disconnect" => crate::mcp::disconnect(args, state).await,
        "connector.refresh_tools" => crate::mcp::refresh_tools(args, state).await,
        "library.list" => crate::library::list(state),
        "library.install" => crate::library::install(args, state),
        "app.save" => app_save(args, state),
        "app.list" => app_list(state),
        "app.delete" => app_delete(args, state),
        "routine.set" => crate::routines::set(args, state),
        "routine.list" => crate::routines::list(state),
        "routine.delete" => crate::routines::delete(args, state),
        "policy.get" => policy_get(state),
        "policy.set" => policy_set(args, state),
        "policy.set_unsafe" => policy_set_unsafe(args, state),
        "creds.list" => creds_list(state),
        "creds.set" => creds_set(args, state),
        "creds.delete" => creds_delete(args, state),
        "approval.resolve" => approval_resolve(args, state),
        other => Err(format!("unknown capability '{other}'")),
    }
}

/// Remove a connector, first tearing down any live MCP client so a stdio child
/// process / http session isn't orphaned in the registry.
async fn connector_remove(args: &Value, state: &AppState) -> Result<Value, String> {
    if let Some(id) = args.get("id").and_then(|v| v.as_str()) {
        if let Some(client) = state.mcp.lock().await.remove(id) {
            // best-effort graceful shutdown; ignore result (we're removing anyway)
            let _ = client.into_service().cancel().await;
        }
    }
    connectors::remove(args, state)
}

// ── local system verbs ──────────────────────────────────────────────────────

fn sys_info() -> Result<Value, String> {
    Ok(json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "family": std::env::consts::FAMILY,
        "kerneld": "webos-spike 0.1",
    }))
}

/// Reject path traversal and absolute paths — every fs call stays in the jail.
fn safe_path(rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() {
        return Err("path required".into());
    }
    if rel.contains("..") || Path::new(rel).is_absolute() {
        return Err("path escapes sandbox".into());
    }
    Ok(root_dir().join(rel))
}

/// Public jail-guard so other modules (e.g. the routine scheduler's fs_write
/// sink) resolve sandbox paths through the SAME traversal/absolute-path check.
pub fn resolve_jail_path(rel: &str) -> Result<PathBuf, String> {
    safe_path(rel)
}

fn fs_read(args: &Value) -> Result<Value, String> {
    let rel = args.get("path").and_then(|v| v.as_str()).ok_or("path required")?;
    let p = safe_path(rel)?;
    let content = std::fs::read_to_string(&p).map_err(|e| format!("read failed: {e}"))?;
    Ok(json!({ "path": rel, "content": content }))
}

fn fs_write(args: &Value) -> Result<Value, String> {
    let rel = args.get("path").and_then(|v| v.as_str()).ok_or("path required")?;
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let p = safe_path(rel)?;
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&p, content).map_err(|e| format!("write failed: {e}"))?;
    Ok(json!({ "path": rel, "bytes": content.len(), "summary": format!("saved {} bytes", content.len()) }))
}

fn fs_list(args: &Value) -> Result<Value, String> {
    let rel = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let dir = if rel.is_empty() { root_dir() } else { safe_path(rel)? };
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| format!("list failed: {e}"))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        entries.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "dir": is_dir,
            "size": size,
        }));
    }
    // directories first, then alphabetical
    entries.sort_by(|a, b| {
        let (ad, bd) = (a["dir"].as_bool().unwrap_or(false), b["dir"].as_bool().unwrap_or(false));
        bd.cmp(&ad).then_with(|| {
            a["name"].as_str().unwrap_or("").to_lowercase()
                .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });
    Ok(json!({ "path": rel, "entries": entries }))
}

// ── real-filesystem mounts (operator-mounted, AI-governed; READ-ONLY) ─────────
//
// The fs.* caps above stay JAILED to the sandbox and are unchanged. This is a
// SEPARATE, opt-in reader for the operator's real files. It can read ONLY inside
// directories the operator has explicitly mounted (mount.add, which is
// PROTECTED — the AI can never widen its own reach). With zero mounts the real
// filesystem is entirely unreachable, identical to before this feature existed.
//
// Containment is by CANONICAL-PATH PREFIX on path COMPONENTS (not string
// prefix): every requested path is canonicalized (resolving symlinks) and must
// be equal to, or a descendant of, a canonical mount root. This defeats both
// `..` traversal and symlink escapes, and the component check avoids the classic
// `/home/user` vs `/home/user-evil` string-prefix bypass.

/// Largest real file `files.read` will return, in bytes. Reads above this are
/// rejected rather than buffering an unbounded file into memory / over the bus.
const MAX_REAL_READ_BYTES: u64 = 2 * 1024 * 1024; // 2 MiB

/// Canonicalize `path`, then require the result to live inside one of the
/// canonical `mounts`. Returns the canonical, validated absolute path.
///
/// Security properties:
///   * `std::fs::canonicalize` resolves `.`/`..` AND follows symlinks, so the
///     value compared against the mounts is the real on-disk location — a
///     symlink inside a mount that points outside it is rejected here.
///   * Containment is a COMPONENT-wise prefix test (`starts_with` on `Path`
///     compares whole components), so `/home/user` does NOT contain
///     `/home/user-evil`.
///   * The requested path must exist (canonicalize fails otherwise); this is a
///     read-only surface so that is the correct, safe behavior.
fn resolve_in_mounts(path: &str, mounts: &[String]) -> Result<PathBuf, String> {
    if path.trim().is_empty() {
        return Err("path required".into());
    }
    let requested = Path::new(path);
    if !requested.is_absolute() {
        return Err("path must be absolute (real filesystem)".into());
    }
    if mounts.is_empty() {
        return Err("no mounts configured — real filesystem access is disabled".into());
    }
    // Canonicalize the request (resolves .., follows symlinks). Failure here
    // means the path doesn't exist or isn't reachable — reject without leaking
    // why beyond "not found / not permitted".
    let canon = std::fs::canonicalize(requested)
        .map_err(|_| "path not found or not permitted".to_string())?;

    for m in mounts {
        let root = Path::new(m);
        // Component-wise containment: canon == root, or canon is under root.
        if canon == root || canon.starts_with(root) {
            return Ok(canon);
        }
    }
    Err("path is outside all mounted folders".into())
}

/// PROTECTED: add a real directory to the mount set. Validates that the path is
/// absolute, exists, and is a directory; stores the CANONICAL form; rejects a
/// path already covered by (equal to, or inside) an existing mount.
fn mount_add(args: &Value, state: &AppState) -> Result<Value, String> {
    let raw = args.get("path").and_then(|v| v.as_str()).ok_or("path required")?;
    if raw.trim().is_empty() {
        return Err("path required".into());
    }
    let p = Path::new(raw);
    if !p.is_absolute() {
        return Err("path must be absolute".into());
    }
    let canon = std::fs::canonicalize(p).map_err(|e| format!("path not accessible: {e}"))?;
    if !canon.is_dir() {
        return Err("path must be a directory".into());
    }
    let canon_str = canon.to_string_lossy().to_string();

    let mut mounts = state.mounts.lock().unwrap();
    // Reject if already covered: equal to an existing mount, the new mount is a
    // child of an existing one, OR an existing one is a child of the new one
    // (which would make the existing mount redundant / confusing).
    for m in mounts.iter() {
        let existing = Path::new(m);
        if canon == existing || canon.starts_with(existing) {
            return Err(format!("path already covered by mount '{m}'"));
        }
        if existing.starts_with(&canon) {
            return Err(format!("path '{canon_str}' would contain existing mount '{m}'"));
        }
    }
    mounts.push(canon_str.clone());
    mounts.sort();
    drop(mounts);
    settings::save(state); // write-through so the mount survives a restart
    tracing::info!("mount added: {canon_str}");
    Ok(json!({ "path": canon_str, "mounted": true }))
}

/// Governable read of the mount inventory (paths only — no file contents).
fn mount_list(state: &AppState) -> Result<Value, String> {
    let mounts = state.mounts.lock().unwrap();
    let rows: Vec<Value> = mounts.iter().map(|m| json!({ "path": m })).collect();
    Ok(json!({ "mounts": rows }))
}

/// PROTECTED: remove a mount. Accepts either the exact stored canonical path or
/// any path that canonicalizes to a stored mount, so the operator doesn't have
/// to know the post-symlink form.
fn mount_remove(args: &Value, state: &AppState) -> Result<Value, String> {
    let raw = args.get("path").and_then(|v| v.as_str()).ok_or("path required")?;
    // Try to canonicalize for matching; fall back to the raw string so a mount
    // whose target was since deleted can still be removed.
    let target = std::fs::canonicalize(raw)
        .map(|c| c.to_string_lossy().to_string())
        .unwrap_or_else(|_| raw.to_string());

    let mut mounts = state.mounts.lock().unwrap();
    let before = mounts.len();
    mounts.retain(|m| m != &target && m != raw);
    let removed = mounts.len() != before;
    drop(mounts);
    if removed {
        settings::save(state);
        tracing::info!("mount removed: {target}");
        Ok(json!({ "path": target, "removed": true }))
    } else {
        Err(format!("no such mount '{raw}'"))
    }
}

/// Governable: list a directory inside a mount. The requested path is
/// canonicalized and confined to the mounts; entries are returned the same shape
/// as fs.list (name/dir/size) so the Finder UI is identical. The returned
/// `entries[].path` are absolute real paths so the UI can drill down / read.
fn files_list(args: &Value, state: &AppState) -> Result<Value, String> {
    let req = args.get("path").and_then(|v| v.as_str()).ok_or("path required")?;
    let mounts = state.mounts.lock().unwrap().clone();
    let dir = resolve_in_mounts(req, &mounts)?;
    if !dir.is_dir() {
        return Err("not a directory".into());
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| format!("list failed: {e}"))? {
        let entry = entry.map_err(|e| e.to_string())?;
        // file_type() here does NOT follow symlinks (lstat); a symlink shows as
        // its own entry. Reading THROUGH it is still re-validated by
        // resolve_in_mounts on the next files.read/files.list call, so a symlink
        // pointing outside the mount is harmless — it just can't be followed.
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        entries.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "dir": is_dir,
            "size": size,
            "path": entry.path().to_string_lossy(),
        }));
    }
    entries.sort_by(|a, b| {
        let (ad, bd) = (a["dir"].as_bool().unwrap_or(false), b["dir"].as_bool().unwrap_or(false));
        bd.cmp(&ad).then_with(|| {
            a["name"].as_str().unwrap_or("").to_lowercase()
                .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });
    Ok(json!({ "path": dir.to_string_lossy(), "entries": entries }))
}

/// Governable: read a text file inside a mount. Canonicalized + confined to the
/// mounts, size-capped at MAX_REAL_READ_BYTES, returned as lossy UTF-8. No
/// writes to the real filesystem exist in this task.
fn files_read(args: &Value, state: &AppState) -> Result<Value, String> {
    let req = args.get("path").and_then(|v| v.as_str()).ok_or("path required")?;
    let mounts = state.mounts.lock().unwrap().clone();
    let p = resolve_in_mounts(req, &mounts)?;
    let meta = std::fs::metadata(&p).map_err(|e| format!("stat failed: {e}"))?;
    if meta.is_dir() {
        return Err("path is a directory, not a file".into());
    }
    if meta.len() > MAX_REAL_READ_BYTES {
        return Err(format!(
            "file too large: {} bytes (cap {} bytes)",
            meta.len(),
            MAX_REAL_READ_BYTES
        ));
    }
    let bytes = std::fs::read(&p).map_err(|e| format!("read failed: {e}"))?;
    let content = String::from_utf8_lossy(&bytes).to_string();
    Ok(json!({ "path": p.to_string_lossy(), "bytes": bytes.len(), "content": content }))
}

// ── external-API wrapper (this is what "functionality" looks like here) ───────

async fn weather_get(args: &Value) -> Result<Value, String> {
    let lat = args.get("lat").and_then(|v| v.as_f64()).unwrap_or(40.7128);
    let lon = args.get("lon").and_then(|v| v.as_f64()).unwrap_or(-74.0060);
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}&current=temperature_2m,weather_code,wind_speed_10m"
    );
    // Through the egress floor like every other outbound request.
    let (_status, body) = egress::fetch("GET", &url, vec![], None, &["api.open-meteo.com".to_string()])
        .await
        .map_err(|e| e.0)?;
    let cur = body.get("current").cloned().unwrap_or_else(|| json!({}));
    let temp = cur.get("temperature_2m").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let wind = cur.get("wind_speed_10m").and_then(|v| v.as_f64()).unwrap_or(0.0);
    Ok(json!({
        "lat": lat, "lon": lon,
        "temperature_c": temp,
        "weather_code": cur.get("weather_code"),
        "wind_kmh": wind,
        "summary": format!("{temp}°C, wind {wind} km/h"),
    }))
}

// ── UI-as-data verbs: the OS reading/writing its own screens ──────────────────

fn ui_get(args: &Value, state: &AppState) -> Result<Value, String> {
    let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("home");
    let store = state.surfaces.lock().unwrap();
    store.get(id).cloned().ok_or_else(|| format!("no surface '{id}'"))
}

fn ui_render(args: &Value, state: &AppState) -> Result<Value, String> {
    let surface = args.get("surface").cloned().ok_or("surface required")?;
    let id = surface
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("surface.id required")?
        .to_string();
    state.surfaces.lock().unwrap().insert(id.clone(), surface);
    Ok(json!({ "stored": id }))
}

/// Build a guaranteed-valid table Surface from a binding + columns and store it.
/// The chat agent uses this instead of hand-authoring widget JSON (unreliable).
/// Pull an op's curated items_path + default_columns from the connector registry,
/// so ui.table/ui.chart can fill them in when the caller omits/guesses them.
fn op_meta(state: &AppState, connector: &str, op: &str) -> (Option<String>, Value) {
    let c = state.connectors.lock().unwrap();
    if let Some(d) = c.get(connector) {
        if let Some(o) = d.ops.iter().find(|o| o.id == op) {
            let items_path = o.graphql.as_ref().and_then(|g| g.items_path.clone());
            let cols = serde_json::to_value(&o.default_columns).unwrap_or_else(|_| json!([]));
            return (items_path, cols);
        }
    }
    (None, json!([]))
}

fn ui_table(args: &Value, state: &AppState) -> Result<Value, String> {
    let connector = args.get("connector").and_then(|v| v.as_str()).ok_or("connector required")?;
    let op = args.get("op").and_then(|v| v.as_str()).ok_or("op required")?;
    let call_args = args.get("args").cloned().unwrap_or_else(|| json!({}));
    let (op_items, op_cols) = op_meta(state, connector, op);
    // Fill items_path / columns from the op's curated metadata when blank — the
    // AI often guesses wrong dot-paths (e.g. nested state.name) or omits items.
    let mut items = args.get("items").cloned().unwrap_or_else(|| json!(""));
    if items.as_str().map(|s| s.is_empty()).unwrap_or(true) {
        if let Some(ip) = op_items {
            items = json!(ip);
        }
    }
    // The op's curated columns are AUTHORITATIVE when present — the model
    // reliably guesses wrong dot-paths (flat 'status' vs nested 'state.name'),
    // so for library ops we ignore caller columns and use the curated set.
    // Only honor caller-supplied columns for ops that declare none.
    let op_cols_empty = op_cols.as_array().map(|a| a.is_empty()).unwrap_or(true);
    let columns = if !op_cols_empty {
        op_cols
    } else {
        args.get("columns").cloned().unwrap_or_else(|| json!([]))
    };
    let title = args.get("title").and_then(|v| v.as_str()).unwrap_or(op).to_string();
    let id = format!("aiwin-{}", state.seq.fetch_add(1, Ordering::Relaxed));
    let surface = json!({
        "id": id, "title": title, "icon": args.get("icon").and_then(|v| v.as_str()).unwrap_or("grid"), "root": "stack",
        "elements": {
            "stack": { "type": "Stack", "props": {}, "children": ["head", "tbl"] },
            "head": { "type": "Heading", "props": { "value": title }, "children": [] },
            "tbl": { "type": "Table", "props": {
                "source": { "capability": "conn.call", "args": { "connector": connector, "op": op, "args": call_args } },
                "items": items, "columns": columns, "refresh": args.get("refresh").and_then(|v| v.as_u64()).unwrap_or(30)
            }, "children": [] }
        }
    });
    state.surfaces.lock().unwrap().insert(id.clone(), surface);
    Ok(json!({ "stored": id }))
}

/// Build a json-render Chart surface (bar/line/area/donut) bound to a conn.call.
/// `agg:"count"` groups rows by `x` and plots the per-category count — the right
/// mode for categorical data (e.g. tickets by status) that has no numeric field.
fn ui_chart(args: &Value, state: &AppState) -> Result<Value, String> {
    let connector = args.get("connector").and_then(|v| v.as_str()).ok_or("connector required")?;
    let op = args.get("op").and_then(|v| v.as_str()).ok_or("op required")?;
    let call_args = args.get("args").cloned().unwrap_or_else(|| json!({}));
    let (op_items, _) = op_meta(state, connector, op);
    let mut items = args.get("items").cloned().unwrap_or_else(|| json!(""));
    if items.as_str().map(|s| s.is_empty()).unwrap_or(true) {
        if let Some(ip) = op_items {
            items = json!(ip);
        }
    }
    let ctype = args.get("type").and_then(|v| v.as_str()).unwrap_or("bar");
    let x = args.get("x").cloned().unwrap_or(Value::Null);
    let y = args.get("y").cloned().unwrap_or(Value::Null);
    let agg = args.get("agg").cloned().unwrap_or(Value::Null);
    let title = args.get("title").and_then(|v| v.as_str()).unwrap_or(op).to_string();
    let id = format!("aiwin-{}", state.seq.fetch_add(1, Ordering::Relaxed));
    let surface = json!({
        "id": id, "title": title, "icon": args.get("icon").and_then(|v| v.as_str()).unwrap_or("chart"), "root": "stack",
        "elements": {
            "stack": { "type": "Stack", "props": {}, "children": ["head", "chart"] },
            "head": { "type": "Heading", "props": { "value": title }, "children": [] },
            "chart": { "type": "Chart", "props": {
                "source": { "capability": "conn.call", "args": { "connector": connector, "op": op, "args": call_args } },
                "items": items, "type": ctype, "x": x, "y": y, "agg": agg,
                "refresh": args.get("refresh").and_then(|v| v.as_u64()).unwrap_or(30), "height": 200
            }, "children": [] }
        }
    });
    state.surfaces.lock().unwrap().insert(id.clone(), surface);
    Ok(json!({ "stored": id }))
}

/// Build a json-render Board (kanban) surface bound to a conn.call: columns of
/// cards grouped by `groupBy` (a required dot-path, e.g. state.name). Like
/// ui_table, items_path is filled from the op's curated metadata when blank.
/// cardTitle defaults to "title"; cardFields, when omitted, are derived from the
/// op's curated default_columns (each `{header,path}` → `{label,path}`) minus
/// whatever the groupBy/cardTitle paths already show, so the card body doesn't
/// repeat the column header or the title.
fn ui_board(args: &Value, state: &AppState) -> Result<Value, String> {
    let connector = args.get("connector").and_then(|v| v.as_str()).ok_or("connector required")?;
    let op = args.get("op").and_then(|v| v.as_str()).ok_or("op required")?;
    let group_by = args.get("groupBy").and_then(|v| v.as_str()).ok_or("groupBy required")?.to_string();
    let call_args = args.get("args").cloned().unwrap_or_else(|| json!({}));
    let (op_items, op_cols) = op_meta(state, connector, op);

    let mut items = args.get("items").cloned().unwrap_or_else(|| json!(""));
    if items.as_str().map(|s| s.is_empty()).unwrap_or(true) {
        if let Some(ip) = op_items {
            items = json!(ip);
        }
    }

    let card_title = args
        .get("cardTitle")
        .and_then(|v| v.as_str())
        .unwrap_or("title")
        .to_string();

    // cardFields: honor the caller's explicit list; otherwise derive from the
    // op's curated columns, dropping the groupBy + cardTitle paths (no point
    // repeating the column the cards are bucketed by, or the bold title).
    let card_fields = match args.get("cardFields") {
        Some(v) if v.as_array().map(|a| !a.is_empty()).unwrap_or(false) => v.clone(),
        _ => {
            let derived: Vec<Value> = op_cols
                .as_array()
                .map(|cols| {
                    cols.iter()
                        .filter_map(|c| {
                            let path = c.get("path").and_then(|p| p.as_str())?;
                            if path == group_by || path == card_title {
                                return None;
                            }
                            let label = c.get("header").and_then(|h| h.as_str()).unwrap_or(path);
                            Some(json!({ "label": label, "path": path }))
                        })
                        .collect()
                })
                .unwrap_or_default();
            json!(derived)
        }
    };

    let title = args.get("title").and_then(|v| v.as_str()).unwrap_or(op).to_string();
    let id = format!("aiwin-{}", state.seq.fetch_add(1, Ordering::Relaxed));
    let surface = json!({
        "id": id, "title": title, "icon": args.get("icon").and_then(|v| v.as_str()).unwrap_or("grid"), "root": "stack",
        "elements": {
            "stack": { "type": "Stack", "props": {}, "children": ["head", "board"] },
            "head": { "type": "Heading", "props": { "value": title }, "children": [] },
            "board": { "type": "Board", "props": {
                "source": { "capability": "conn.call", "args": { "connector": connector, "op": op, "args": call_args } },
                "items": items, "groupBy": group_by, "cardTitle": card_title, "cardFields": card_fields,
                "refresh": args.get("refresh").and_then(|v| v.as_u64()).unwrap_or(30)
            }, "children": [] }
        }
    });
    state.surfaces.lock().unwrap().insert(id.clone(), surface);
    Ok(json!({ "stored": id }))
}

/// Generic surface builder: the agent (or any composer) hands a COMPLETE flat
/// json-render spec `{id?,title?,icon?,root,elements,state?}` and we validate it
/// server-side with the SAME validator ai.compose uses (model::validate_spec —
/// root resolves, every element.type is in the catalog incl. Board, every child
/// id resolves, every bound capability is on the safelist) before storing it.
///
/// This unlocks the full json-render expressiveness for the agent (mixed
/// Grid/Card/Stack/Metric/Chart/Board layouts, repeat, $state/$cond/$template)
/// rather than the fixed ui.table/ui.chart/ui.board templates. On success an
/// Build a CLICKABLE master→detail surface: a list on the left (Table, or a
/// kanban Board when `groupBy` is set) whose row/card click writes the record to
/// state `/selected` (selectInto), and a Detail on the right bound to
/// `{$state:"/selected"}`. The daemon wires the selection path on BOTH sides so
/// the click-through always connects — the model only picks connector/op.
fn ui_master_detail(args: &Value, state: &AppState) -> Result<Value, String> {
    let connector = args.get("connector").and_then(|v| v.as_str()).ok_or("connector required")?;
    let op = args.get("op").and_then(|v| v.as_str()).ok_or("op required")?;
    let call_args = args.get("args").cloned().unwrap_or_else(|| json!({}));
    let (op_items, op_cols) = op_meta(state, connector, op);

    let mut items = args.get("items").cloned().unwrap_or_else(|| json!(""));
    if items.as_str().map(|s| s.is_empty()).unwrap_or(true) {
        if let Some(ip) = op_items {
            items = json!(ip);
        }
    }
    let columns = match args.get("columns") {
        Some(v) if v.as_array().map(|a| !a.is_empty()).unwrap_or(false) => v.clone(),
        _ => op_cols.clone(),
    };
    // detail fields: caller-supplied, else every curated column as {label,path}
    let detail_fields = match args.get("detailFields") {
        Some(v) if v.as_array().map(|a| !a.is_empty()).unwrap_or(false) => v.clone(),
        _ => json!(op_cols
            .as_array()
            .map(|cols| cols
                .iter()
                .filter_map(|c| {
                    let p = c.get("path").and_then(|x| x.as_str())?;
                    let l = c.get("header").and_then(|x| x.as_str()).unwrap_or(p);
                    Some(json!({ "label": l, "path": p }))
                })
                .collect::<Vec<_>>())
            .unwrap_or_default()),
    };

    let refresh = args.get("refresh").and_then(|v| v.as_u64()).unwrap_or(30);
    let source = json!({ "capability": "conn.call", "args": { "connector": connector, "op": op, "args": call_args } });

    // Left pane: a kanban Board if groupBy given, else a Table. Both clickable.
    let left = if let Some(gb) = args.get("groupBy").and_then(|v| v.as_str()) {
        let card_title = args.get("cardTitle").and_then(|v| v.as_str()).unwrap_or("title");
        let card_fields = json!(op_cols
            .as_array()
            .map(|cols| cols
                .iter()
                .filter_map(|c| {
                    let p = c.get("path").and_then(|x| x.as_str())?;
                    if p == gb || p == card_title { return None; }
                    let l = c.get("header").and_then(|x| x.as_str()).unwrap_or(p);
                    Some(json!({ "label": l, "path": p }))
                })
                .collect::<Vec<_>>())
            .unwrap_or_default());
        json!({ "type": "Board", "props": {
            "source": source, "items": items, "groupBy": gb, "cardTitle": card_title,
            "cardFields": card_fields, "selectInto": "/selected", "refresh": refresh
        }, "children": [] })
    } else {
        json!({ "type": "Table", "props": {
            "source": source, "items": items, "columns": columns,
            "selectInto": "/selected", "refresh": refresh
        }, "children": [] })
    };
    let detail = json!({ "type": "Detail", "props": {
        "record": { "$state": "/selected" }, "empty": "Select an item to view details",
        "fields": detail_fields
    }, "children": [] });

    let title = args.get("title").and_then(|v| v.as_str()).unwrap_or(op).to_string();
    let id = format!("aiwin-{}", state.seq.fetch_add(1, Ordering::Relaxed));
    let surface = json!({
        "id": id, "title": title,
        "icon": args.get("icon").and_then(|v| v.as_str()).unwrap_or("grid"),
        "root": "stack", "state": { "selected": null },
        "elements": {
            "stack": { "type": "Stack", "props": {}, "children": ["head", "grid"] },
            "head": { "type": "Heading", "props": { "value": title }, "children": [] },
            "grid": { "type": "Grid", "props": { "cols": 2 }, "children": ["master", "detail"] },
            "master": left,
            "detail": detail
        }
    });
    state.surfaces.lock().unwrap().insert(id.clone(), surface);
    Ok(json!({ "stored": id }))
}

/// `aiwin-N` id is minted when none was supplied and `{stored:id}` is returned;
/// on validation failure a clear error string comes back so the agent can fix +
/// retry. The spec is accepted either as the args themselves or wrapped under a
/// `surface` key (tolerant of either tool-call shape).
fn ui_surface(args: &Value, state: &AppState) -> Result<Value, String> {
    // Accept {surface:{...}} or the spec fields directly on args.
    let mut spec = match args.get("surface") {
        Some(s) if s.is_object() => s.clone(),
        _ => args.clone(),
    };
    if !spec.is_object() {
        return Err("surface spec must be a JSON object".into());
    }

    // Validate the flat spec against the shared catalog + capability safelist.
    // A clear error here lets the agent correct the component/binding and retry.
    model::validate_spec(&spec)
        .map_err(|e| format!("invalid surface spec: {e}"))?;

    // Mint an id when the caller omitted one (matches ui.table/ui.chart ids), so
    // the shell can address the stored window. Preserve a caller-supplied id.
    let id = match spec.get("id").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => {
            let id = format!("aiwin-{}", state.seq.fetch_add(1, Ordering::Relaxed));
            spec["id"] = json!(id);
            id
        }
    };
    // Default a title/icon so the floating window has a label (non-fatal sugar).
    if spec.get("title").and_then(|v| v.as_str()).map(|s| s.is_empty()).unwrap_or(true) {
        spec["title"] = json!(args.get("title").and_then(|v| v.as_str()).unwrap_or("Surface"));
    }
    if spec.get("icon").is_none() {
        spec["icon"] = json!("grid");
    }

    state.surfaces.lock().unwrap().insert(id.clone(), spec);
    Ok(json!({ "stored": id }))
}

/// Patch a stored FLAT json-render surface ({root,elements}) in place. Three
/// shapes are accepted (and may be combined in one call):
///   { id, elements: { "<elId>": {type,props,children} | <props-patch> } }
///       — for each entry, merge into the stored element if present (deep-merge
///         objects, replace scalars/arrays), otherwise insert it whole.
///   { id, set: { "<elId>": { <props-patch> } } }
///       — sugar that targets element.props only (deep-merge); the element must
///         already exist.
///   { id, root: "<elId>" }  /  { id, title: "..." }
///       — update the surface root / title.
/// Backward-tolerant: a legacy { widget } surface (or a { widget } patch) is
/// still accepted — the widget tree is replaced and nothing panics.
fn ui_patch(args: &Value, state: &AppState) -> Result<Value, String> {
    let id = args.get("id").and_then(|v| v.as_str()).ok_or("id required")?;
    let mut store = state.surfaces.lock().unwrap();
    let surface = store.get_mut(id).ok_or_else(|| format!("no surface '{id}'"))?;

    let mut touched = false;

    // Legacy escape hatch: a {widget} patch replaces the legacy widget tree.
    if let Some(widget) = args.get("widget").cloned() {
        surface["widget"] = widget;
        touched = true;
    }

    // Optionally retarget the surface root.
    if let Some(root) = args.get("root").and_then(|v| v.as_str()) {
        surface["root"] = json!(root);
        touched = true;
    }
    // Optionally update the surface title (preserved otherwise).
    if let Some(title) = args.get("title").and_then(|v| v.as_str()) {
        surface["title"] = json!(title);
        touched = true;
    }

    // `set` is props-only sugar: merge each value into elements[elId].props.
    if let Some(set) = args.get("set").and_then(|v| v.as_object()) {
        let elements = surface_elements_mut(surface);
        for (el_id, props_patch) in set {
            let el = elements
                .get_mut(el_id)
                .ok_or_else(|| format!("no element '{el_id}' to set"))?;
            if !el.is_object() {
                return Err(format!("element '{el_id}' is not an object"));
            }
            let props = el
                .as_object_mut()
                .unwrap()
                .entry("props")
                .or_insert_with(|| json!({}));
            merge_into(props, props_patch);
        }
        touched = true;
    }

    // `elements` merges/replaces whole elements (or partial element patches).
    if let Some(patch_els) = args.get("elements").and_then(|v| v.as_object()) {
        let elements = surface_elements_mut(surface);
        for (el_id, patch) in patch_els {
            match elements.get_mut(el_id) {
                Some(existing) => merge_into(existing, patch),
                None => {
                    elements.insert(el_id.clone(), patch.clone());
                }
            }
        }
        touched = true;
    }

    if !touched {
        return Err("patch must include one of: elements, set, root, title, widget".into());
    }
    Ok(surface.clone())
}

/// Borrow the surface's `elements` object as a mutable map, creating an empty
/// one if the surface has none yet (e.g. a freshly-rooted flat spec).
fn surface_elements_mut(surface: &mut Value) -> &mut serde_json::Map<String, Value> {
    if !surface.get("elements").map(|e| e.is_object()).unwrap_or(false) {
        surface["elements"] = json!({});
    }
    surface["elements"].as_object_mut().unwrap()
}

/// Recursively merge `patch` into `target`: object keys are merged depth-first;
/// any non-object (scalar, array, null) replaces the value at that key.
fn merge_into(target: &mut Value, patch: &Value) {
    match (target.as_object_mut(), patch.as_object()) {
        (Some(tobj), Some(pobj)) => {
            for (k, pv) in pobj {
                match tobj.get_mut(k) {
                    Some(tv) => merge_into(tv, pv),
                    None => {
                        tobj.insert(k.clone(), pv.clone());
                    }
                }
            }
        }
        // target isn't an object, or patch is a scalar/array/null → replace.
        _ => *target = patch.clone(),
    }
}

// ── the generative seam ───────────────────────────────────────────────────────

// ── saved "docked apps" (a Surface persisted + launchable from the dock) ──────

fn apps_dir() -> PathBuf {
    root_dir().join("apps")
}

/// Hydrate saved apps at boot: each app's Surface goes into the surfaces map
/// (so ui.get / dock launch works) and its metadata into the apps index.
pub fn load_apps(state: &AppState) {
    if let Ok(rd) = std::fs::read_dir(apps_dir()) {
        for e in rd.flatten() {
            if let Ok(txt) = std::fs::read_to_string(e.path()) {
                if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                    if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                        if let Some(surf) = v.get("surface") {
                            state.surfaces.lock().unwrap().insert(id.to_string(), surf.clone());
                        }
                        state.apps.lock().unwrap().push(json!({
                            "id": id, "title": v.get("title"), "glyph": v.get("glyph"),
                        }));
                    }
                }
            }
        }
    }
}

fn app_save(args: &Value, state: &AppState) -> Result<Value, String> {
    let id = args.get("id").and_then(|v| v.as_str()).ok_or("id required")?;
    let title = args.get("title").and_then(|v| v.as_str()).unwrap_or(id);
    let glyph = args.get("glyph").and_then(|v| v.as_str()).unwrap_or("\u{229e}");
    let surface = args.get("surface").cloned().ok_or("surface required")?;
    let rec = json!({ "id": id, "title": title, "glyph": glyph, "surface": surface });
    let d = apps_dir();
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    std::fs::write(d.join(format!("{id}.json")), serde_json::to_string_pretty(&rec).unwrap())
        .map_err(|e| e.to_string())?;
    state.surfaces.lock().unwrap().insert(id.to_string(), surface);
    {
        let mut apps = state.apps.lock().unwrap();
        apps.retain(|a| a.get("id").and_then(|x| x.as_str()) != Some(id));
        apps.push(json!({ "id": id, "title": title, "glyph": glyph }));
    }
    Ok(json!({ "id": id, "saved": true }))
}

fn app_list(state: &AppState) -> Result<Value, String> {
    Ok(json!({ "apps": state.apps.lock().unwrap().clone() }))
}

fn app_delete(args: &Value, state: &AppState) -> Result<Value, String> {
    let id = args.get("id").and_then(|v| v.as_str()).ok_or("id required")?;
    let _ = std::fs::remove_file(apps_dir().join(format!("{id}.json")));
    state.surfaces.lock().unwrap().remove(id);
    state.apps.lock().unwrap().retain(|a| a.get("id").and_then(|x| x.as_str()) != Some(id));
    Ok(json!({ "id": id, "deleted": true }))
}

// ── settings: AI policy (operator-only) ───────────────────────────────────────

fn policy_get(state: &AppState) -> Result<Value, String> {
    let grants = state.grants.lock().unwrap();
    let rows: Vec<Value> = policy::GOVERNABLE
        .iter()
        .map(|cap| {
            let st = grants.get(*cap).cloned().unwrap_or_else(|| "ask".to_string());
            json!({ "capability": cap, "state": st })
        })
        .collect();
    Ok(json!({
        "unsafe_mode": state.unsafe_mode.load(Ordering::Relaxed),
        "grants": rows,
    }))
}

fn policy_set(args: &Value, state: &AppState) -> Result<Value, String> {
    let cap = args.get("capability").and_then(|v| v.as_str()).ok_or("capability required")?;
    let st = args.get("state").and_then(|v| v.as_str()).ok_or("state required")?;
    if !matches!(st, "allow" | "deny" | "ask") {
        return Err("state must be allow|deny|ask".into());
    }
    if policy::PROTECTED.contains(&cap) {
        return Err(format!("'{cap}' is operator-only and cannot be granted to AI"));
    }
    state.grants.lock().unwrap().insert(cap.to_string(), st.to_string());
    settings::save(state); // write-through so the grant survives a restart
    Ok(json!({ "capability": cap, "state": st }))
}

fn policy_set_unsafe(args: &Value, state: &AppState) -> Result<Value, String> {
    let on = args.get("on").and_then(|v| v.as_bool()).ok_or("on (bool) required")?;
    state.unsafe_mode.store(on, Ordering::Relaxed);
    settings::save(state); // persist the flag across restarts
    tracing::warn!("UNSAFE MODE {}", if on { "ENABLED — AI may take any action" } else { "disabled" });
    Ok(json!({ "unsafe_mode": on }))
}

// ── settings: credentials (operator-only; values never leave the daemon) ──────

fn creds_list(state: &AppState) -> Result<Value, String> {
    let creds = state.creds.lock().unwrap();
    let mut names: Vec<Value> = creds.keys().map(|k| json!({ "name": k })).collect();
    names.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(json!({ "credentials": names }))
}

fn creds_set(args: &Value, state: &AppState) -> Result<Value, String> {
    let name = args.get("name").and_then(|v| v.as_str()).ok_or("name required")?;
    let value = args.get("value").and_then(|v| v.as_str()).ok_or("value required")?;
    if name.trim().is_empty() {
        return Err("name required".into());
    }
    // value is intentionally never echoed back or logged. Update the live map,
    // then seal to durable storage (OS keychain or encrypted file).
    let live = {
        let mut creds = state.creds.lock().unwrap();
        creds.insert(name.to_string(), value.to_string());
        creds.clone()
    };
    state.secrets.set(name, value, &live)?;
    Ok(json!({ "name": name, "stored": true }))
}

fn creds_delete(args: &Value, state: &AppState) -> Result<Value, String> {
    let name = args.get("name").and_then(|v| v.as_str()).ok_or("name required")?;
    let live = {
        let mut creds = state.creds.lock().unwrap();
        creds.remove(name);
        creds.clone()
    };
    state.secrets.delete(name, &live)?;
    Ok(json!({ "name": name, "deleted": true }))
}

// ── consent: resolve a pending AI approval (operator-only) ─────────────────────

fn approval_resolve(args: &Value, state: &AppState) -> Result<Value, String> {
    let id = args.get("approvalId").and_then(|v| v.as_str()).ok_or("approvalId required")?;
    let verdict = args.get("verdict").and_then(|v| v.as_str()).ok_or("verdict required")?;
    if !matches!(verdict, "allow_once" | "allow_always" | "deny") {
        return Err("verdict must be allow_once|allow_always|deny".into());
    }
    // "Always" persists a grant under the gate key (op-hash-pinned for connectors)
    // so this exact action never asks again.
    if verdict == "allow_always" {
        let key = args
            .get("grantKey")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("capability").and_then(|v| v.as_str()));
        if let Some(key) = key {
            if !policy::PROTECTED.contains(&key) {
                state.grants.lock().unwrap().insert(key.to_string(), "allow".to_string());
            }
        }
    }
    let sender = state.pending.lock().unwrap().remove(id);
    match sender {
        Some(tx) => {
            let _ = tx.send(verdict.to_string());
            Ok(json!({ "resolved": id, "verdict": verdict }))
        }
        None => Err("approval expired or already resolved".into()),
    }
}
