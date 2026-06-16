//! webOS kerneld — the microkernel-equivalent.
//!
//! ONE bus. Human and AI are peer principals, bound to per-session bearer tokens
//! (not a spoofable query param). A single policy gate decides allow / deny /
//! ASK in front of every dispatch; the ASK tier routes an informed consent
//! prompt to the human session. Connectors ride this same spine: any external
//! service action is a `conn.call` envelope governed per (connector, op, class).

mod caps;
mod chat;
mod connectors;
mod egress;
mod gate;
mod library;
mod mcp;
mod model;
mod policy;
mod routines;
mod secrets;
mod settings;
mod surface;

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tower_http::services::ServeDir;

use connectors::ConnectorDef;

#[derive(Clone)]
pub(crate) struct AppState {
    pub surfaces: Arc<Mutex<HashMap<String, Value>>>,
    pub grants: Arc<Mutex<HashMap<String, String>>>,
    pub unsafe_mode: Arc<AtomicBool>,
    /// Operator-mounted REAL filesystem roots: canonical absolute directory
    /// paths the OS is permitted to read outside the sandbox jail. Empty by
    /// default — with no mounts the real FS is entirely unreachable. Mutated
    /// only by the PROTECTED mount.add / mount.remove caps; persisted in
    /// sandbox/settings.json (these are directory paths, NOT secrets).
    pub mounts: Arc<Mutex<Vec<String>>>,
    pub creds: Arc<Mutex<HashMap<String, String>>>,
    /// Sealed durable backing for `creds` (OS keychain or encrypted file).
    /// The live values stay in `creds`; this only reads/writes them at rest.
    pub secrets: Arc<secrets::SecretStore>,
    pub connectors: Arc<Mutex<HashMap<String, ConnectorDef>>>,
    /// Live MCP clients (one per connected `kind=="mcp"` connector). Async-mutex
    /// because a client's `Peer` is cloned out under the lock and awaited after
    /// release — the lock is never held across an `.await` to the server.
    pub mcp: mcp::McpRegistry,
    /// Saved "docked apps": metadata index ({id,title,glyph}); their Surfaces
    /// live in `surfaces` keyed by id.
    pub apps: Arc<Mutex<Vec<Value>>>,
    /// Persisted headless routines (id -> definition). The background scheduler
    /// reads this every tick and fires due routines through the same gate.
    pub routines: Arc<Mutex<HashMap<String, routines::Routine>>>,
    pub humans: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Message>>>>,
    pub pending: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
    pub seq: Arc<AtomicU64>,
    pub human_token: Arc<String>,
    pub ai_token: Arc<String>,
    /// The json-render catalog description for the AI, generated from the SAME
    /// catalog the renderer uses (ui/src/catalog.ts → web/catalog-prompt.txt via
    /// `npm run build`) and loaded once at boot. Injected into the chat agent +
    /// ai.compose system prompts so the AI's component manifest never drifts from
    /// the real catalog. `None` when the file is missing → both prompts fall back
    /// to their built-in component list.
    pub catalog_prompt: Arc<Option<String>>,
}

/// 32 bytes of OS entropy, hex-encoded. Used as opaque session bearer tokens.
fn gen_token() -> String {
    let mut f = std::fs::File::open("/dev/urandom").expect("open /dev/urandom");
    let mut b = [0u8; 32];
    f.read_exact(&mut b).expect("read entropy");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();

    let root = caps::root_dir();
    std::fs::create_dir_all(&root).expect("create sandbox root");
    let _ = std::fs::create_dir_all(root.join("Documents"));
    let _ = std::fs::create_dir_all(root.join("Photos"));
    if !root.join("readme.txt").exists() {
        let _ = std::fs::write(root.join("readme.txt"), "Welcome to webOS.\n");
        let _ = std::fs::write(root.join("Documents/notes.md"), "# Notes\n\nFirst note.\n");
    }

    let mut surfaces = HashMap::new();
    surfaces.insert("home".to_string(), surface::default_home());
    surfaces.insert("weather".to_string(), surface::weather_surface());
    surfaces.insert("notes".to_string(), surface::notes_surface());

    // Rehydrate persisted, non-secret settings (grants + unsafe_mode) and the
    // sealed credential store before the bus comes up, so a restart preserves
    // operator tuning instead of silently reverting to defaults.
    let (grants, unsafe_mode, mounts) = settings::load();
    if !mounts.is_empty() {
        tracing::info!("loaded {} real-filesystem mount(s)", mounts.len());
    }
    let secret_store = secrets::SecretStore::open();
    let creds = secret_store.load_all();
    if !creds.is_empty() {
        tracing::info!("loaded {} stored credential(s)", creds.len()); // count only — never names/values
    }

    let state = AppState {
        surfaces: Arc::new(Mutex::new(surfaces)),
        grants: Arc::new(Mutex::new(grants)),
        unsafe_mode: Arc::new(AtomicBool::new(unsafe_mode)),
        mounts: Arc::new(Mutex::new(mounts)),
        creds: Arc::new(Mutex::new(creds)),
        secrets: Arc::new(secret_store),
        connectors: Arc::new(Mutex::new(connectors::load_all())),
        mcp: mcp::new_registry(),
        apps: Arc::new(Mutex::new(Vec::new())),
        routines: Arc::new(Mutex::new(routines::load_all())),
        humans: Arc::new(Mutex::new(HashMap::new())),
        pending: Arc::new(Mutex::new(HashMap::new())),
        seq: Arc::new(AtomicU64::new(1)),
        human_token: Arc::new(gen_token()),
        ai_token: Arc::new(gen_token()),
        catalog_prompt: Arc::new(model::load_catalog_prompt()),
    };
    caps::load_apps(&state); // hydrate saved docked apps from the fs jail

    // Background scheduler: fires persisted routines through the same governed
    // bus as principal "ai". Clones the AppState Arcs into the spawned task.
    routines::spawn_scheduler(state.clone());

    let app = Router::new()
        .route("/bootstrap", get(bootstrap))
        .route("/ws", get(ws_handler))
        .fallback_service(ServeDir::new("web"))
        .with_state(state);

    let addr = std::env::var("WEBOS_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    tracing::info!("kerneld on http://{addr}   (fs sandbox: {})", root.display());
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    axum::serve(listener, app).await.expect("serve");
}

/// Localhost-only handshake: the trusted console fetches its session tokens.
/// (Network/multi-user deployments replace this with OIDC.)
async fn bootstrap(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "human_token": *state.human_token, "ai_token": *state.ai_token }))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(q): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Response {
    let token = q.get("token").cloned().unwrap_or_default();
    let principal = if token == *state.human_token {
        "human"
    } else if token == *state.ai_token {
        "ai"
    } else {
        return (axum::http::StatusCode::UNAUTHORIZED, "invalid session token").into_response();
    };
    let principal = principal.to_string();
    ws.on_upgrade(move |socket| handle_socket(socket, principal, state))
}

async fn handle_socket(socket: WebSocket, principal: String, state: AppState) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });

    let session_id = state.seq.fetch_add(1, Ordering::Relaxed);
    if principal == "human" {
        state.humans.lock().unwrap().insert(session_id, tx.clone());
    }
    tracing::info!("connect principal='{principal}' session={session_id}");

    while let Some(Ok(msg)) = stream.next().await {
        let Message::Text(text) = msg else { continue };
        let inv: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(Message::Text(err_resp("", &format!("bad envelope: {e}"), "error")));
                continue;
            }
        };
        let id = inv.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let cap = inv.get("capability").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let args = inv.get("args").cloned().unwrap_or_else(|| json!({}));

        // Each invocation runs as its own task so the bus is concurrent —
        // critical for the chat agent: while its tool call awaits operator
        // consent, this loop stays free to read the approval.resolve that
        // unblocks it.
        let st = state.clone();
        let txc = tx.clone();
        let pr = principal.clone();
        tokio::spawn(async move {
            let resp = match gate::govern(&pr, &cap, &args, &st).await {
                gate::Outcome::Ok(d) => ok_resp(&id, d),
                gate::Outcome::Deny(r) => err_resp(&id, &r, "deny"),
                gate::Outcome::Err(e) => err_resp(&id, &e, "error"),
            };
            let _ = txc.send(Message::Text(resp));
        });
    }

    if principal == "human" {
        state.humans.lock().unwrap().remove(&session_id);
    }
    drop(tx);
    let _ = writer.await;
    tracing::info!("disconnect principal='{principal}' session={session_id}");
}

fn ok_resp(id: &str, data: Value) -> String {
    json!({ "id": id, "ok": true, "data": data }).to_string()
}
fn err_resp(id: &str, error: &str, decision: &str) -> String {
    json!({ "id": id, "ok": false, "error": error, "decision": decision }).to_string()
}
