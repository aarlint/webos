//! Headless routines — autonomous, scheduled work that rides the SAME governed
//! bus as everything else. A routine is a named list of capability steps fired
//! on a fixed interval by a background scheduler. Every step is dispatched
//! through `gate::govern` as principal `"ai"`, so a routine cannot do anything
//! the AI couldn't do interactively: it hits the identical consent gate, emits
//! the same worker-toast `activity` events, and is fail-closed — an `ASK`-tier
//! step with no operator online is DENIED by the gate (no silent escalation).
//!
//! Routines persist as plaintext JSON at `sandbox/routines/*.json` (they hold no
//! secrets — only capability names + literal args). `routine.set`/`routine.delete`
//! are operator-only (PROTECTED); `routine.list` is governable.
//!
//! Shape:
//! ```json
//! {
//!   "id": "morning-weather",
//!   "title": "Morning weather note",
//!   "interval_secs": 3600,
//!   "steps": [
//!     { "capability": "weather.get", "args": { "lat": 40.7, "lon": -74.0 } },
//!     { "capability": "fs.write", "args": { "path": "weather.txt", "content": "..." } }
//!   ],
//!   "on_result": "notify"            // "silent" | "notify" | {"fs_write":"path"} | {"surface":"id"}
//! }
//! ```
//! The spike uses a fixed interval in seconds; cron expressions are a later step.

use crate::{gate, policy, AppState};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone)]
pub struct Step {
    pub capability: String,
    #[serde(default)]
    pub args: Value,
}

/// What to do with a run's collected step results.
///   "silent"               — nothing (results only logged + toasted via the gate)
///   "notify"               — push a {type:"notify"} event to human consoles
///   { "fs_write": "path" } — append a JSON run record to a file in the jail
///   { "surface": "id" }    — render a result surface (stored via the surfaces map)
#[derive(Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum OnResult {
    Named(String),
    Sink(HashMap<String, String>),
}
impl Default for OnResult {
    fn default() -> Self {
        OnResult::Named("silent".into())
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Routine {
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub interval_secs: u64,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub on_result: OnResult,
    /// Monotonic-ish epoch seconds of the last fire; tracked in-memory only so a
    /// freshly-loaded routine fires on the next scheduler tick.
    #[serde(skip)]
    pub last_run: u64,
}

fn dir() -> PathBuf {
    crate::caps::root_dir().join("routines")
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Load every persisted routine at boot. Malformed files are skipped (logged).
pub fn load_all() -> HashMap<String, Routine> {
    let mut m = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(dir()) {
        for e in rd.flatten() {
            match std::fs::read_to_string(e.path()) {
                Ok(txt) => match serde_json::from_str::<Routine>(&txt) {
                    Ok(r) => {
                        m.insert(r.id.clone(), r);
                    }
                    Err(err) => tracing::warn!("skipping unreadable routine: {err}"),
                },
                Err(_) => {}
            }
        }
    }
    m
}

fn persist(r: &Routine) -> Result<(), String> {
    let d = dir();
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    std::fs::write(d.join(format!("{}.json", r.id)), serde_json::to_string_pretty(r).unwrap())
        .map_err(|e| e.to_string())
}

// ── operator-only capabilities (routine.set / routine.delete are PROTECTED) ────

/// Create or replace a routine. Validates the id slug, a positive interval, and
/// that every step names a *known, non-protected* capability — a routine can
/// never schedule an operator-only verb (policy.set, creds.*, connector.add, …)
/// even though only an operator can author the routine in the first place. This
/// keeps the autonomous floor identical to the AI's interactive floor.
pub fn set(args: &Value, state: &AppState) -> Result<Value, String> {
    let mut r: Routine =
        serde_json::from_value(args.clone()).map_err(|e| format!("bad routine definition: {e}"))?;

    if r.id.is_empty() || !r.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("routine id must be a slug (alnum/-/_)".into());
    }
    if r.interval_secs == 0 {
        return Err("interval_secs must be > 0".into());
    }
    if r.steps.is_empty() {
        return Err("routine needs at least one step".into());
    }
    for step in &r.steps {
        if step.capability.is_empty() {
            return Err("each step needs a 'capability'".into());
        }
        if policy::PROTECTED.contains(&step.capability.as_str()) {
            return Err(format!(
                "step '{}' is operator-only and cannot run autonomously in a routine",
                step.capability
            ));
        }
        // A routine that scheduled itself or other routine verbs would be a
        // privilege/loop hazard; routine.* is protected anyway, but be explicit.
        if step.capability.starts_with("routine.") || step.capability == "chat.send" {
            return Err(format!("step '{}' is not allowed in a routine", step.capability));
        }
    }
    if r.title.is_empty() {
        r.title = r.id.clone();
    }
    // Anchor the clock at creation so the FIRST fire lands one full interval
    // later, not the instant the routine is saved. This keeps the schedule
    // predictable and lets an operator set a routine and then go offline before
    // it ever runs (the fail-closed path) rather than racing the next tick.
    r.last_run = now_secs();

    persist(&r)?;
    let summary = format!("routine '{}' set ({} step(s), every {}s)", r.id, r.steps.len(), r.interval_secs);
    state.routines.lock().unwrap().insert(r.id.clone(), r.clone());
    Ok(json!({ "id": r.id, "interval_secs": r.interval_secs, "steps": r.steps.len(), "summary": summary }))
}

/// Delete a routine (operator-only). Removes it from the live registry and the
/// jail so it stops firing and does not resurrect on next boot.
pub fn delete(args: &Value, state: &AppState) -> Result<Value, String> {
    let id = args.get("id").and_then(|v| v.as_str()).ok_or("id required")?;
    state.routines.lock().unwrap().remove(id);
    let _ = std::fs::remove_file(dir().join(format!("{id}.json")));
    Ok(json!({ "id": id, "deleted": true }))
}

// ── governable: names + schedule, never anything sensitive ─────────────────────

pub fn list(state: &AppState) -> Result<Value, String> {
    let routines = state.routines.lock().unwrap();
    let mut rows: Vec<Value> = routines
        .values()
        .map(|r| {
            json!({
                "id": r.id,
                "title": r.title,
                "interval_secs": r.interval_secs,
                "steps": r.steps.iter().map(|s| json!({ "capability": s.capability })).collect::<Vec<_>>(),
                "on_result": on_result_label(&r.on_result),
                "last_run": r.last_run,
            })
        })
        .collect();
    rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Ok(json!({ "routines": rows }))
}

fn on_result_label(o: &OnResult) -> Value {
    match o {
        OnResult::Named(s) => json!(s),
        OnResult::Sink(m) => json!(m),
    }
}

// ── the scheduler ──────────────────────────────────────────────────────────────

/// Spawn the background scheduler. It wakes on a coarse tick, and for each
/// routine whose interval has elapsed it runs every step through `gate::govern`
/// as the `ai` principal. The Arcs inside `AppState` are cheap to clone, so the
/// task owns its own handle to the shared state without blocking the bus.
///
/// Fail-closed by construction: each step goes through the same gate as an
/// interactive AI tool call, so an `ASK`-tier step with no operator online is
/// denied (govern returns Deny "no operator online"). Nothing is auto-approved
/// for a routine that wouldn't be auto-approved interactively.
pub fn spawn_scheduler(state: AppState) {
    tokio::spawn(async move {
        // Coarse 1s tick keeps short demo intervals responsive while staying
        // cheap; per-routine elapsed time is checked against wall-clock seconds.
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tracing::info!("routine scheduler started");
        loop {
            tick.tick().await;
            let now = now_secs();

            // Snapshot the due routines under the lock, then release it before
            // any await — never hold a std Mutex across the gate's async work.
            let due: Vec<Routine> = {
                let routines = state.routines.lock().unwrap();
                routines
                    .values()
                    .filter(|r| now.saturating_sub(r.last_run) >= r.interval_secs)
                    .cloned()
                    .collect()
            };

            for r in due {
                // Mark fired immediately (in the live registry) so a long-running
                // step can't cause a pile-up of overlapping fires on the next tick.
                if let Some(live) = state.routines.lock().unwrap().get_mut(&r.id) {
                    live.last_run = now;
                }
                run_routine(&r, &state).await;
            }
        }
    });
}

/// Run one routine: dispatch each step through the gate, collect results, then
/// apply on_result. Step failures don't abort the routine — they're recorded and
/// the run continues, so one denied step doesn't silently kill the schedule.
async fn run_routine(r: &Routine, state: &AppState) {
    tracing::info!("routine '{}' firing ({} step(s))", r.id, r.steps.len());
    let mut results: Vec<Value> = Vec::with_capacity(r.steps.len());
    for step in &r.steps {
        let outcome = gate::govern("ai", &step.capability, &step.args, state).await;
        let entry = match outcome {
            gate::Outcome::Ok(d) => json!({ "capability": step.capability, "ok": true, "data": d }),
            gate::Outcome::Deny(reason) => {
                tracing::info!("routine '{}' step '{}' denied: {reason}", r.id, step.capability);
                json!({ "capability": step.capability, "ok": false, "decision": "deny", "error": reason })
            }
            gate::Outcome::Err(e) => {
                tracing::warn!("routine '{}' step '{}' error: {e}", r.id, step.capability);
                json!({ "capability": step.capability, "ok": false, "decision": "error", "error": e })
            }
        };
        results.push(entry);
    }
    apply_on_result(r, &results, state);
}

/// Route the collected run results to the configured sink. fs_write goes through
/// the same jail guard as fs.write (no path escape); surface goes into the
/// surfaces map; notify pushes a server event to human consoles.
fn apply_on_result(r: &Routine, results: &[Value], state: &AppState) {
    let run_id = format!("run-{}", state.seq.fetch_add(1, Ordering::Relaxed));
    let record = json!({
        "routine": r.id, "title": r.title, "runId": run_id, "at": now_secs(), "results": results,
    });

    match &r.on_result {
        OnResult::Named(s) if s == "notify" => {
            let ok = results.iter().all(|x| x["ok"].as_bool().unwrap_or(false));
            gate::broadcast_humans(
                state,
                &json!({
                    "type": "notify", "source": "routine", "routine": r.id, "title": r.title,
                    "ok": ok, "summary": format!("routine '{}' ran {} step(s)", r.id, results.len()),
                }),
            );
        }
        OnResult::Named(_) => { /* "silent" (or unknown name) — gate toasts already covered it */ }
        OnResult::Sink(m) => {
            if let Some(path) = m.get("fs_write") {
                // Append one JSON line per run; reuse the jail guard so a routine
                // can never write outside the sandbox even if its path is hostile.
                match crate::caps::resolve_jail_path(path) {
                    Ok(p) => {
                        if let Some(parent) = p.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let line = format!("{}\n", record);
                        use std::io::Write;
                        match std::fs::OpenOptions::new().create(true).append(true).open(&p) {
                            Ok(mut f) => {
                                if let Err(e) = f.write_all(line.as_bytes()) {
                                    tracing::warn!("routine '{}' fs_write failed: {e}", r.id);
                                }
                            }
                            Err(e) => tracing::warn!("routine '{}' fs_write open failed: {e}", r.id),
                        }
                    }
                    Err(e) => tracing::warn!("routine '{}' fs_write rejected: {e}", r.id),
                }
            } else if let Some(id) = m.get("surface") {
                // Store a simple json-render result surface keyed by the given id.
                let surface = result_surface(id, r, results);
                state.surfaces.lock().unwrap().insert(id.clone(), surface);
            }
        }
    }
}

/// A minimal, guaranteed-valid json-render surface summarizing a routine run,
/// matching the flat {root,elements} shape the React island renders.
fn result_surface(id: &str, r: &Routine, results: &[Value]) -> Value {
    let mut children: Vec<String> = vec!["head".into()];
    let mut elements = serde_json::Map::new();
    elements.insert(
        "head".into(),
        json!({ "type": "Heading", "props": { "value": format!("{} — last run", r.title) }, "children": [] }),
    );
    for (i, res) in results.iter().enumerate() {
        let key = format!("r{i}");
        let cap = res["capability"].as_str().unwrap_or("?");
        let ok = res["ok"].as_bool().unwrap_or(false);
        let detail = if ok {
            res["data"]
                .get("summary")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "ok".into())
        } else {
            res["error"].as_str().unwrap_or("error").to_string()
        };
        elements.insert(
            key.clone(),
            json!({ "type": "KeyValue", "props": { "label": cap, "value": detail }, "children": [] }),
        );
        children.push(key);
    }
    elements.insert(
        "stack".into(),
        json!({ "type": "Stack", "props": {}, "children": children }),
    );
    json!({ "id": id, "title": r.title, "root": "stack", "elements": elements })
}
