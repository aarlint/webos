//! The single governance path: decide → (ask the operator) → dispatch. Used by
//! both the WebSocket loop and the in-OS chat agent, so EVERY action — a button
//! click or an AI tool call — is gated identically.

use crate::{caps, connectors, policy, AppState};
use axum::extract::ws::Message;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::oneshot;

pub enum Outcome {
    Ok(Value),
    Deny(String),
    Err(String),
}

pub async fn govern(principal: &str, cap: &str, args: &Value, state: &AppState) -> Outcome {
    let (gate_key, conn_meta) = connectors::gate_key_and_meta(cap, args, state);

    // Worker toast: announce ANY ai-principal work (chat, tool calls, autonomous)
    // to the operator's console. govern() is the single ai chokepoint.
    let activity_token = if principal == "ai" {
        let token = format!("act-{}", state.seq.fetch_add(1, Ordering::Relaxed));
        let label = conn_meta
            .as_ref()
            .and_then(|m| m.get("summary").and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| friendly_label(cap));
        broadcast_humans(state, &json!({ "type": "activity", "state": "start", "actor": "ai", "token": token, "label": label }));
        Some(token)
    } else {
        None
    };

    let decision = {
        let grants = state.grants.lock().unwrap();
        let unsafe_on = state.unsafe_mode.load(Ordering::Relaxed);
        policy::decide(principal, cap, &gate_key, &grants, unsafe_on)
    };
    let outcome = match decision {
        policy::Decision::Deny(reason) => {
            tracing::warn!("DENY [{principal}] {cap}: {reason}");
            Outcome::Deny(reason)
        }
        policy::Decision::Allow => run(cap, args, state).await,
        policy::Decision::Ask => {
            tracing::info!("ASK  [{principal}] {gate_key} → awaiting operator");
            ask_then_run(principal, cap, &gate_key, &conn_meta, args, state).await
        }
    };

    if let Some(token) = activity_token {
        let detail = match &outcome {
            Outcome::Ok(_) => "ok",
            Outcome::Deny(_) => "denied",
            Outcome::Err(_) => "error",
        };
        broadcast_humans(state, &json!({ "type": "activity", "state": "done", "actor": "ai", "token": token, "detail": detail }));
    }
    outcome
}

/// Broadcast a server-pushed message to all connected human consoles.
pub fn broadcast_humans(state: &AppState, msg: &Value) {
    let text = msg.to_string();
    let humans = state.humans.lock().unwrap();
    for sender in humans.values() {
        let _ = sender.send(Message::Text(text.clone()));
    }
}

fn friendly_label(cap: &str) -> String {
    match cap {
        "conn.call" => "Calling a service",
        "connector.list" => "Listing connectors",
        "connector.describe" => "Inspecting a connector",
        "library.list" => "Browsing the connector library",
        "library.install" => "Installing a connector",
        "weather.get" => "Getting the weather",
        "fs.read" => "Reading a file",
        "fs.list" => "Listing files",
        "files.read" => "Reading a real file",
        "files.list" => "Browsing your files",
        "mount.list" => "Listing mounted folders",
        "ui.table" | "ui.chart" | "ui.board" | "ui.surface" | "ui.render" => "Building a view",
        "ai.compose" => "Composing a screen",
        other => other,
    }
    .to_string()
}

async fn run(cap: &str, args: &Value, state: &AppState) -> Outcome {
    match caps::dispatch(cap, args, state).await {
        Ok(d) => Outcome::Ok(d),
        Err(e) => Outcome::Err(e),
    }
}

async fn ask_then_run(
    principal: &str,
    cap: &str,
    gate_key: &str,
    conn_meta: &Option<Value>,
    args: &Value,
    state: &AppState,
) -> Outcome {
    let approval_id = format!("apr-{}", state.seq.fetch_add(1, Ordering::Relaxed));
    let (otx, orx) = oneshot::channel::<String>();
    state.pending.lock().unwrap().insert(approval_id.clone(), otx);

    let mut request = json!({
        "type": "approval", "approvalId": approval_id, "principal": principal,
        "capability": cap, "args": args, "grantKey": gate_key,
    });
    if let Some(meta) = conn_meta {
        request["conn"] = meta.clone();
    }
    let request = request.to_string();

    let mut operators = 0;
    {
        let humans = state.humans.lock().unwrap();
        for sender in humans.values() {
            if sender.send(Message::Text(request.clone())).is_ok() {
                operators += 1;
            }
        }
    }
    if operators == 0 {
        state.pending.lock().unwrap().remove(&approval_id);
        return Outcome::Deny("no operator online to approve this action".into());
    }

    match tokio::time::timeout(Duration::from_secs(120), orx).await {
        Ok(Ok(verdict)) if verdict.starts_with("allow") => {
            tracing::info!("GRANT [{principal}] {gate_key}: {verdict}");
            run(cap, args, state).await
        }
        Ok(Ok(_)) => Outcome::Deny("operator denied this action".into()),
        _ => {
            state.pending.lock().unwrap().remove(&approval_id);
            Outcome::Deny("approval timed out".into())
        }
    }
}
