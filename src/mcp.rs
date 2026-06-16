//! MCP connector kind — proxy an external Model Context Protocol server so its
//! tools become `conn.call` operations on the SAME governed spine as every other
//! connector. No third-party code runs in-process; an MCP server is either a
//! child process we own (stdio) or a remote HTTP/SSE endpoint we egress to. Its
//! tools are daemon-owned DATA, reached through the one governed verb `conn.call`.
//!
//! # Trust model
//!
//! Everything an MCP server returns — tool names, descriptions, input schemas,
//! and call results — is UNTRUSTED. We:
//!   * never log a tool description or a result (only names + counts);
//!   * derive each op's `class` (read/write) AUTHORITATIVELY from the tool name
//!     and only treat the server's `readOnlyHint` / `destructiveHint` as a soft
//!     downgrade-to-write hint, never an upgrade to read;
//!   * inject per-connector secrets ONLY into the stdio child's environment or
//!     the HTTP `Authorization` header — never into argv, the URL, or any log.
//!
//! # Lifecycle (operator-only verbs)
//!
//!   connector.connect    → spawn/connect the client, initialize, list_tools,
//!                           map tools→ops, persist the connector, keep the
//!                           live client in `AppState.mcp`.
//!   connector.refresh_tools → re-list tools on a live client and re-map ops.
//!   connector.disconnect → drop + cancel the live client (ops persist).
//!
//! conn.call (any principal, governed) → if the connector kind is "mcp",
//!   proxy to the live client's `call_tool` instead of egress HTTP.
//!
//! # Concurrency
//!
//! The live `RunningService` is held in a `tokio::sync::Mutex` keyed by
//! connector id. We NEVER hold the lock across an `.await` to the server: a
//! `Peer` is cheaply `Clone`, so `call_tool` clones the peer out under the lock,
//! releases the lock, then awaits on the clone (clone-then-release).

use crate::connectors::{ConnectorDef, OpDef};
use crate::AppState;
use rmcp::model::{CallToolRequestParams, Tool};
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::child_process::{ConfigureCommandExt, TokioChildProcess};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::ServiceExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

/// How to reach an MCP server. Only meaningful when `ConnectorDef.kind == "mcp"`.
///
/// # Connecting a remote MCP server (e.g. Linear) over http
///
/// Linear exposes a hosted MCP server at the streamable-http endpoint
/// `https://mcp.linear.app/mcp`. Two operator-only steps bring it online; the
/// token is stored SEALED and only ever materializes as a `Bearer` header at
/// the egress boundary (never in the connector file, args, response, or a log).
///
/// 1. Store the Linear token under a NAME (the value is sealed by secrets.rs):
///    ```jsonc
///    creds.set { "name": "LINEAR_TOKEN", "value": "<your-linear-api-token>" }
///    ```
///
/// 2. Add the connector, then connect it (connect discovers the tool list →
///    ops; the author seeds none):
///    ```jsonc
///    connector.add {
///      "id": "linear",
///      "display_name": "Linear",
///      "kind": "mcp",
///      "transport": {
///        "kind": "http",
///        "url": "https://mcp.linear.app/mcp",
///        "auth_cred_ref": "LINEAR_TOKEN"   // NAME in the sealed store, not the value
///      }
///    }
///    connector.connect { "id": "linear" }
///    ```
///
/// After `connector.connect`, Linear's tools are reachable through the one
/// governed verb `conn.call { "connector": "linear", "op": "<tool>", "args": {…} }`,
/// with per-op read/write class derived authoritatively from the tool name.
///
/// Note: Linear's endpoint may use OAuth rather than a static bearer token; in
/// that case store the issued access token as `LINEAR_TOKEN`. The transport
/// always sends it as `Authorization: Bearer <token>` (rmcp's bare-token
/// `auth_header`). Plaintext/local endpoints are refused in production — only
/// the OFF-by-default `WEBOS_ALLOW_LOCAL_MCP=1` dev flag permits a local mock.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct McpTransport {
    /// "stdio" | "http" | "sse"
    pub kind: String,
    // ── stdio ──
    /// Executable to spawn (stdio only). Resolved on PATH by the OS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Map of CHILD_ENV_VAR → credential name. The named secret's VALUE is
    /// injected into the child's environment under that env var at connect
    /// time. The value never touches argv, the connector file, or any log.
    #[serde(default)]
    pub env_cred_refs: HashMap<String, String>,
    // ── http / sse ──
    /// Endpoint URL (http/sse only). Must be https and pass the egress SSRF floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Credential name whose value becomes the Bearer token in the
    /// `Authorization` header (http/sse only). The token is never logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_cred_ref: Option<String>,
}

/// A live, initialized MCP client plus a snapshot of what it exposes.
pub struct McpClient {
    /// Kept alive so the connection (and, for stdio, the child process) lives.
    service: RunningService<RoleClient, ()>,
    pub tool_count: usize,
    pub server_name: String,
}

impl McpClient {
    /// Consume the client and hand back the running service so the caller can
    /// `.cancel().await` it (graceful stdio child / http session shutdown).
    pub fn into_service(self) -> RunningService<RoleClient, ()> {
        self.service
    }
}

/// `AppState.mcp` — the live client registry, keyed by connector id.
pub type McpRegistry = Arc<AsyncMutex<HashMap<String, McpClient>>>;

pub fn new_registry() -> McpRegistry {
    Arc::new(AsyncMutex::new(HashMap::new()))
}

/// Derive an op's class AUTHORITATIVELY from the tool. A tool is "read" only if
/// its name looks read-like AND the server didn't flag it destructive. The
/// server's hints can only push toward "write" (the safer class), never the
/// other way — the name heuristic is the floor.
fn derive_class(tool: &Tool) -> String {
    let name = tool.name.to_ascii_lowercase();
    let read_like = name.starts_with("get_")
        || name.starts_with("get")
        || name.starts_with("list_")
        || name.starts_with("list")
        || name.starts_with("read_")
        || name.starts_with("read")
        || name.starts_with("search_")
        || name.starts_with("search")
        || name.starts_with("find_")
        || name.starts_with("find")
        || name.starts_with("query")
        || name.starts_with("fetch")
        || name.starts_with("describe")
        || name.starts_with("view");
    if !read_like {
        return "write".into();
    }
    // Soft hints from the (untrusted) server can only DOWNGRADE a read to write.
    if let Some(ann) = &tool.annotations {
        if ann.destructive_hint == Some(true) {
            return "write".into();
        }
        if ann.read_only_hint == Some(false) {
            return "write".into();
        }
    }
    "read".into()
}

/// Map an MCP `Tool` (untrusted) to an `OpDef`. The op id is the tool name; the
/// summary is the description (already flagged untrusted to the model elsewhere).
/// `path_template`/`method` are MCP-shaped placeholders so the existing op-hash
/// grant pinning still produces a stable identity per (tool, class).
fn tool_to_op(tool: &Tool) -> OpDef {
    let class = derive_class(tool);
    OpDef {
        id: tool.name.to_string(),
        method: "MCP".into(),
        path_template: format!("mcp://{}", tool.name),
        allowed_query: Vec::new(),
        class,
        summary: tool.description.as_ref().map(|d| d.to_string()).unwrap_or_default(),
        default_args: Map::new(),
        default_columns: Vec::new(),
        graphql: None,
    }
}

/// Resolve a credential name → value from the live (in-memory) creds map.
/// Returns an error naming the MISSING credential (a name, never a value).
fn resolve_cred(state: &AppState, name: &str) -> Result<String, String> {
    state
        .creds
        .lock()
        .unwrap()
        .get(name)
        .cloned()
        .ok_or_else(|| format!("credential '{name}' is not set (add it in Settings → Credentials)"))
}

/// Establish a live MCP client for `def`, returning (client, mapped ops).
/// Secrets are injected here and ONLY here; they are dropped as soon as the
/// transport is built (env handed to the child / token handed to the header).
async fn establish(def: &ConnectorDef, state: &AppState) -> Result<(McpClient, Vec<OpDef>), String> {
    let t = def
        .transport
        .as_ref()
        .ok_or("mcp connector is missing its transport block")?;

    let service: RunningService<RoleClient, ()> = match t.kind.as_str() {
        "stdio" => {
            let command = t.command.as_deref().ok_or("stdio transport requires a 'command'")?;
            // Resolve every referenced secret up front so we fail before spawning.
            let mut env: Vec<(String, String)> = Vec::with_capacity(t.env_cred_refs.len());
            for (var, cred_name) in &t.env_cred_refs {
                env.push((var.clone(), resolve_cred(state, cred_name)?));
            }
            let args = t.args.clone();
            // Build the child command. Secrets go into the ENVIRONMENT only —
            // never argv. process inherits no extra env beyond what we set.
            let cmd = tokio::process::Command::new(command).configure(|c| {
                c.args(&args);
                for (k, v) in &env {
                    c.env(k, v);
                }
            });
            let transport = TokioChildProcess::new(cmd)
                .map_err(|e| format!("failed to spawn mcp server: {e}"))?;
            ().serve(transport)
                .await
                .map_err(|e| format!("mcp stdio initialize failed: {e}"))?
        }
        "http" | "sse" => {
            let url = t.url.as_deref().ok_or("http/sse transport requires a 'url'")?;
            // The remote endpoint must clear the SAME egress floor as every other
            // outbound call: https-only + a public, non-rebindable host. The only
            // relaxation is the OFF-by-default WEBOS_ALLOW_LOCAL_MCP dev flag,
            // which permits a LOCAL plaintext mock (see egress::assert_mcp_endpoint).
            crate::egress::assert_mcp_endpoint(url).await.map_err(|e| e.0)?;
            let mut cfg = StreamableHttpClientTransportConfig::with_uri(url);
            if let Some(cred_name) = &t.auth_cred_ref {
                // bare token; rmcp applies it as `Authorization: Bearer <token>`.
                cfg.auth_header = Some(resolve_cred(state, cred_name)?);
            }
            let transport = StreamableHttpClientTransport::from_config(cfg);
            ().serve(transport)
                .await
                .map_err(|e| format!("mcp http initialize failed: {e}"))?
        }
        other => return Err(format!("unknown mcp transport kind '{other}' (use stdio|http|sse)")),
    };

    let server_name = service
        .peer_info()
        .map(|i| i.server_info.name.to_string())
        .unwrap_or_else(|| def.id.clone());

    let tools = service
        .peer()
        .list_all_tools()
        .await
        .map_err(|e| format!("mcp list_tools failed: {e}"))?;
    let ops: Vec<OpDef> = tools.iter().map(tool_to_op).collect();

    // names + count only — descriptions/schemas are untrusted and never logged.
    tracing::info!(
        "mcp '{}' connected ({} tool(s)): {}",
        def.id,
        ops.len(),
        ops.iter().map(|o| o.id.as_str()).collect::<Vec<_>>().join(", ")
    );

    let client = McpClient {
        service,
        tool_count: ops.len(),
        server_name,
    };
    Ok((client, ops))
}

/// connector.connect (PROTECTED, human-only) — bring an mcp connector online,
/// list its tools, persist the mapped ops, and store the live client.
pub async fn connect(args: &Value, state: &AppState) -> Result<Value, String> {
    let id = args.get("id").and_then(|v| v.as_str()).ok_or("id required")?;
    let def = {
        let c = state.connectors.lock().unwrap();
        c.get(id).cloned().ok_or_else(|| format!("unknown connector '{id}'"))?
    };
    if def.kind != "mcp" {
        return Err(format!("connector '{id}' is kind '{}', not 'mcp'", def.kind));
    }

    let (client, ops) = establish(&def, state).await?;
    let count = client.tool_count;
    let server_name = client.server_name.clone();

    // Persist the mapped ops onto the connector so describe()/conn.call() see
    // them even before a (re)connect, and survive a restart.
    let updated = {
        let mut c = state.connectors.lock().unwrap();
        let d = c.get_mut(id).ok_or_else(|| format!("connector '{id}' vanished"))?;
        d.ops = ops;
        d.clone()
    };
    crate::connectors::persist(&updated)?;

    // Store the live client (replacing any prior one).
    state.mcp.lock().await.insert(id.to_string(), client);

    Ok(json!({ "id": id, "status": "connected", "server": server_name, "tool_count": count }))
}

/// connector.refresh_tools (PROTECTED, human-only) — re-list tools on a live
/// client and re-map the ops. The client must already be connected.
pub async fn refresh_tools(args: &Value, state: &AppState) -> Result<Value, String> {
    let id = args.get("id").and_then(|v| v.as_str()).ok_or("id required")?;
    // Clone the peer out under the async lock, then release before awaiting.
    let peer: Peer<RoleClient> = {
        let reg = state.mcp.lock().await;
        let client = reg.get(id).ok_or_else(|| format!("mcp connector '{id}' is not connected"))?;
        client.service.peer().clone()
    };
    let tools = peer
        .list_all_tools()
        .await
        .map_err(|e| format!("mcp list_tools failed: {e}"))?;
    let ops: Vec<OpDef> = tools.iter().map(tool_to_op).collect();
    let count = ops.len();
    tracing::info!("mcp '{}' refreshed: {} tool(s)", id, count);

    let updated = {
        let mut c = state.connectors.lock().unwrap();
        let d = c.get_mut(id).ok_or_else(|| format!("unknown connector '{id}'"))?;
        d.ops = ops;
        d.clone()
    };
    crate::connectors::persist(&updated)?;
    if let Some(client) = state.mcp.lock().await.get_mut(id) {
        client.tool_count = count;
    }
    Ok(json!({ "id": id, "status": "refreshed", "tool_count": count }))
}

/// connector.disconnect (PROTECTED, human-only) — drop and cancel the live
/// client. The mapped ops persist on the connector; conn.call will then report
/// the connector as offline until reconnected.
pub async fn disconnect(args: &Value, state: &AppState) -> Result<Value, String> {
    let id = args.get("id").and_then(|v| v.as_str()).ok_or("id required")?;
    let client = state.mcp.lock().await.remove(id);
    match client {
        Some(c) => {
            // Best-effort graceful shutdown (closes stdio child / http session).
            let _ = c.service.cancel().await;
            Ok(json!({ "id": id, "status": "disconnected" }))
        }
        None => Err(format!("mcp connector '{id}' is not connected")),
    }
}

/// Proxy a `conn.call` to a live MCP client. Returns the same envelope shape as
/// the REST path: {connector, op, ok, status, class, data, _untrusted}. The
/// result content is UNTRUSTED and flagged as such.
pub async fn call(
    cid: &str,
    opid: &str,
    class: &str,
    call_args: &Value,
    state: &AppState,
) -> Result<Value, String> {
    // Clone the peer out under the async lock, then release before awaiting.
    let peer: Peer<RoleClient> = {
        let reg = state.mcp.lock().await;
        let client = reg
            .get(cid)
            .ok_or_else(|| format!("mcp connector '{cid}' is not connected — run connector.connect"))?;
        client.service.peer().clone()
    };

    // MCP tool arguments must be a JSON object.
    let arguments: Option<Map<String, Value>> = match call_args {
        Value::Object(m) => Some(m.clone()),
        Value::Null => None,
        _ => return Err("mcp tool args must be a JSON object".into()),
    };
    let mut params = CallToolRequestParams::new(opid.to_string());
    params.arguments = arguments;

    let result = peer
        .call_tool(params)
        .await
        .map_err(|e| format!("mcp call_tool '{opid}' failed: {e}"))?;

    let is_error = result.is_error.unwrap_or(false);
    // The full CallToolResult serializes cleanly (content + structuredContent).
    let data = serde_json::to_value(&result).unwrap_or(Value::Null);
    Ok(json!({
        "connector": cid, "op": opid, "ok": !is_error, "status": if is_error { 500 } else { 200 },
        "class": class, "host": "mcp", "data": data, "_untrusted": true,
    }))
}
