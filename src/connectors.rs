//! Connectors — connect to ANY external service as daemon-owned DATA (no
//! third-party code). A connector is a descriptor with typed operations; the AI
//! reaches all of them through ONE governed verb, `conn.call`, so policy/consent
//! apply per (connector, op, class) without N dynamic capabilities.

use crate::{egress, AppState};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
pub struct Column {
    pub header: String,
    pub path: String,
}

/// A GraphQL operation block. When present on an `OpDef`, `conn.call` ignores
/// `path_template`/`method` and instead POSTs `{ query, variables }` as JSON to
/// the connector `base_url`, injecting the connector auth header as usual.
#[derive(Serialize, Deserialize, Clone)]
pub struct GraphQlBlock {
    /// The GraphQL document to send (query or mutation).
    pub query: String,
    /// The call_arg names to forward as GraphQL `variables`. Each name is looked
    /// up in the caller's `args` and passed through under the same key; missing
    /// names are simply omitted (the GraphQL server applies its own defaults).
    #[serde(default)]
    pub variables: Vec<String>,
    /// Optional dot-path to the result array (e.g. "data.countries"), surfaced
    /// back to the UI as a hint for table rendering. Purely advisory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items_path: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct OpDef {
    pub id: String,
    /// REST ops set this; graphql ops may omit it (defaults to empty, ignored).
    #[serde(default)]
    pub method: String,
    /// REST ops set this; graphql ops may omit it (defaults to empty, ignored).
    #[serde(default)]
    pub path_template: String,
    #[serde(default)]
    pub allowed_query: Vec<String>,
    #[serde(default)]
    pub class: String, // "read" | "write"; derived from method if empty
    #[serde(default)]
    pub summary: String,
    /// Seeds for the deterministic fallback Surface (no model needed).
    #[serde(default)]
    pub default_args: Map<String, Value>,
    #[serde(default)]
    pub default_columns: Vec<Column>,
    /// When set, this op is a GraphQL POST (path_template/method are ignored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graphql: Option<GraphQlBlock>,
}

/// Per-connector outbound authentication. `cred_ref` is a NAME in the sealed
/// secrets store (secrets.rs) — never the value. The value is resolved
/// server-side at the egress boundary ONLY (see `resolve_auth_header` /
/// `apply_query_auth`); it is never placed in args, the response, or any log,
/// and is never exposed to the AI.
#[derive(Serialize, Deserialize, Clone)]
pub struct ConnectorAuth {
    /// "none" | "bearer" | "header" | "query" | "basic"
    #[serde(default = "auth_none")]
    pub scheme: String,
    /// Name of the credential in the sealed store to inject. Required for every
    /// scheme except "none".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cred_ref: Option<String>,
    /// For scheme "header": the header to set (e.g. "X-Api-Key").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_name: Option<String>,
    /// For scheme "query": the query parameter to append (e.g. "api_key").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param_name: Option<String>,
    /// Optional literal prefix prepended to the secret for "bearer"/"header"
    /// (e.g. "Bearer " override, "Token ", "sk-"). Ignored by "query"/"basic".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}
fn auth_none() -> String {
    "none".into()
}
impl Default for ConnectorAuth {
    fn default() -> Self {
        ConnectorAuth { scheme: auth_none(), cred_ref: None, header_name: None, param_name: None, prefix: None }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ConnectorDef {
    pub id: String,
    pub display_name: String,
    #[serde(default = "manual_rest")]
    pub kind: String,
    /// REST connectors set this; mcp connectors leave it empty.
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    #[serde(default)]
    pub ops: Vec<OpDef>,
    /// Per-connector outbound auth (manual_rest only). The secret is resolved at
    /// the egress boundary; the descriptor only stores the credential NAME.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ConnectorAuth>,
    /// Only meaningful when `kind == "mcp"`: how to reach the MCP server. The
    /// tool list (→ ops) is populated by connector.connect, not by the author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<crate::mcp::McpTransport>,
}
fn manual_rest() -> String {
    "manual_rest".into()
}

const RESERVED: &[&str] = &[
    "sys", "fs", "weather", "ui", "ai", "policy", "creds", "approval", "conn", "connector", "routine",
];

pub fn op_class(op: &OpDef) -> String {
    if !op.class.is_empty() {
        return op.class.clone();
    }
    // GraphQL ops are always a POST under the hood, so the REST method heuristic
    // would mis-classify every query as a write. Default graphql to read unless
    // the op explicitly set class="write" (handled by the early return above).
    if op.graphql.is_some() {
        return "read".into();
    }
    match op.method.to_uppercase().as_str() {
        "GET" | "HEAD" | "OPTIONS" => "read".into(),
        _ => "write".into(),
    }
}

/// Identity a persisted grant is pinned to (P3): if a connector redefines an op
/// behind a granted name, the hash changes and the grant no longer matches.
fn op_hash(conn: &ConnectorDef, op: &OpDef) -> String {
    let mut h = DefaultHasher::new();
    op.id.hash(&mut h);
    op.method.to_uppercase().hash(&mut h);
    op.path_template.hash(&mut h);
    op_class(op).hash(&mut h);
    conn.base_url.hash(&mut h);
    format!("{:x}", h.finish())
}

fn host_of(url: &str) -> String {
    reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default()
}

// ── per-connector outbound auth (secret resolved at the egress boundary only) ──

/// Validate an auth block at connector.add time WITHOUT touching any secret —
/// it only checks that the descriptor is internally consistent (cred_ref is
/// present for non-"none" schemes, the right name field is present for
/// header/query). The credential need not exist yet.
fn validate_auth(a: &ConnectorAuth) -> Result<(), String> {
    match a.scheme.as_str() {
        "none" => Ok(()),
        "bearer" | "basic" | "header" | "query" => {
            let cref = a.cred_ref.as_deref().unwrap_or("");
            if cref.trim().is_empty() {
                return Err(format!("auth scheme '{}' requires a 'cred_ref' (a name in the credential store)", a.scheme));
            }
            if a.scheme == "header" && a.header_name.as_deref().unwrap_or("").trim().is_empty() {
                return Err("auth scheme 'header' requires a 'header_name'".into());
            }
            if a.scheme == "query" && a.param_name.as_deref().unwrap_or("").trim().is_empty() {
                return Err("auth scheme 'query' requires a 'param_name'".into());
            }
            Ok(())
        }
        other => Err(format!("unknown auth scheme '{other}' (use none|bearer|header|query|basic)")),
    }
}

/// Resolve a credential NAME → its sealed value from the live in-memory store.
/// Returns an error that names the MISSING credential only — never a value.
fn resolve_cred(state: &AppState, name: &str) -> Result<String, String> {
    state
        .creds
        .lock()
        .unwrap()
        .get(name)
        .cloned()
        .ok_or_else(|| format!("credential '{name}' is not set (add it in Settings → Credentials)"))
}

/// Build the header-bearing auth header for bearer/header/basic schemes, with
/// the secret resolved server-side. Returns `Ok(None)` for "none" or "query"
/// (query auth is applied to the URL, not a header). The returned tuple is
/// `(header_name, header_value)` — the value carries the secret and must never
/// be logged or echoed.
fn resolve_auth_header(auth: &ConnectorAuth, state: &AppState) -> Result<Option<(String, String)>, String> {
    match auth.scheme.as_str() {
        "none" | "query" => Ok(None),
        "bearer" => {
            let cref = auth.cred_ref.as_deref().ok_or("bearer auth missing cred_ref")?;
            let secret = resolve_cred(state, cref)?;
            // Default RFC 6750 "Bearer " unless the author overrode the prefix.
            let prefix = auth.prefix.as_deref().unwrap_or("Bearer ");
            Ok(Some(("Authorization".to_string(), format!("{prefix}{secret}"))))
        }
        "header" => {
            let cref = auth.cred_ref.as_deref().ok_or("header auth missing cred_ref")?;
            let hname = auth.header_name.as_deref().ok_or("header auth missing header_name")?;
            let secret = resolve_cred(state, cref)?;
            let prefix = auth.prefix.as_deref().unwrap_or("");
            Ok(Some((hname.to_string(), format!("{prefix}{secret}"))))
        }
        "basic" => {
            let cref = auth.cred_ref.as_deref().ok_or("basic auth missing cred_ref")?;
            let secret = resolve_cred(state, cref)?;
            // The credential value is the already-formed "user:pass" userinfo.
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(secret.as_bytes());
            Ok(Some(("Authorization".to_string(), format!("Basic {encoded}"))))
        }
        other => Err(format!("unknown auth scheme '{other}'")),
    }
}

/// A non-secret, human-readable "where" label for an mcp connector's consent
/// prompt and listing: the command basename (stdio) or the endpoint host
/// (http/sse). Never includes args, env, or the auth token.
fn mcp_host_label(d: &ConnectorDef) -> String {
    match &d.transport {
        Some(t) if t.kind == "stdio" => t
            .command
            .as_deref()
            .map(|c| format!("mcp:stdio:{}", c.rsplit('/').next().unwrap_or(c)))
            .unwrap_or_else(|| "mcp:stdio".into()),
        Some(t) => t
            .url
            .as_deref()
            .map(|u| format!("mcp:{}:{}", t.kind, host_of(u)))
            .unwrap_or_else(|| format!("mcp:{}", t.kind)),
        None => "mcp".into(),
    }
}

fn dir() -> PathBuf {
    crate::caps::root_dir().join("connectors")
}

pub fn load_all() -> HashMap<String, ConnectorDef> {
    let mut m = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(dir()) {
        for e in rd.flatten() {
            if let Ok(txt) = std::fs::read_to_string(e.path()) {
                if let Ok(c) = serde_json::from_str::<ConnectorDef>(&txt) {
                    m.insert(c.id.clone(), c);
                }
            }
        }
    }
    m
}
/// Write a connector definition to the jail. Public so the mcp lifecycle can
/// re-persist a connector after it populates `ops` from a live tool list.
pub fn persist(c: &ConnectorDef) -> Result<(), String> {
    let d = dir();
    std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    std::fs::write(d.join(format!("{}.json", c.id)), serde_json::to_string_pretty(c).unwrap())
        .map_err(|e| e.to_string())
}

// ── operator-only capabilities ────────────────────────────────────────────────

pub fn add(args: &Value, state: &AppState) -> Result<Value, String> {
    let mut def: ConnectorDef =
        serde_json::from_value(args.clone()).map_err(|e| format!("bad connector definition: {e}"))?;
    if def.id.is_empty() || !def.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("connector id must be a slug (alnum/-/_)".into());
    }
    let prefix = def.id.split(['-', '_']).next().unwrap_or("");
    if RESERVED.contains(&prefix) {
        return Err(format!("connector id may not start with reserved prefix '{prefix}'"));
    }
    match def.kind.as_str() {
        "mcp" => {
            // mcp connectors carry a transport block, not a REST base_url. Tools
            // (→ ops) are discovered live by connector.connect, so any ops a
            // caller tried to seed are dropped to keep the server authoritative.
            let t = def.transport.as_ref().ok_or("mcp connector requires a 'transport' block")?;
            match t.kind.as_str() {
                "stdio" => {
                    if t.command.as_deref().unwrap_or("").is_empty() {
                        return Err("stdio transport requires a 'command'".into());
                    }
                }
                "http" | "sse" => {
                    let u = t.url.as_deref().unwrap_or("");
                    // https is the production floor. The OFF-by-default
                    // WEBOS_ALLOW_LOCAL_MCP dev flag also permits a local http
                    // mock; the real local/private-host check happens at connect
                    // time in egress::assert_mcp_endpoint (DNS-resolved).
                    let local_ok = std::env::var("WEBOS_ALLOW_LOCAL_MCP").map(|v| v == "1").unwrap_or(false);
                    let scheme_ok = u.starts_with("https://") || (local_ok && u.starts_with("http://"));
                    if !scheme_ok {
                        return Err("mcp http/sse transport 'url' must be https".into());
                    }
                }
                other => return Err(format!("unknown mcp transport kind '{other}' (use stdio|http|sse)")),
            }
            def.base_url = String::new();
            def.allowed_hosts.clear();
            def.ops.clear();
            // mcp transports carry their own auth in the transport block; the
            // REST auth block is meaningless here, so drop it.
            def.auth = None;
        }
        _ => {
            if !def.base_url.starts_with("https://") {
                return Err("base_url must be https".into());
            }
            if def.allowed_hosts.is_empty() {
                let h = host_of(&def.base_url);
                if !h.is_empty() {
                    def.allowed_hosts.push(h);
                }
            }
            if let Some(a) = &def.auth {
                validate_auth(a)?;
            }
        }
    }
    persist(&def)?;
    let op_count = def.ops.len();
    let id = def.id.clone();
    let kind = def.kind.clone();
    state.connectors.lock().unwrap().insert(id.clone(), def);
    // mcp connectors aren't usable until connected; say so explicitly.
    let status = if kind == "mcp" { "added (run connector.connect)" } else { "ready" };
    Ok(json!({ "id": id, "op_count": op_count, "status": status }))
}

pub fn remove(args: &Value, state: &AppState) -> Result<Value, String> {
    let id = args.get("id").and_then(|v| v.as_str()).ok_or("id required")?;
    state.connectors.lock().unwrap().remove(id);
    let _ = std::fs::remove_file(dir().join(format!("{id}.json")));
    // Drop every grant tied to this connector so a re-added id can't inherit it.
    {
        let mut g = state.grants.lock().unwrap();
        let pref = format!("conn.call:{id}:");
        let stale: Vec<String> = g.keys().filter(|k| k.starts_with(&pref)).cloned().collect();
        for k in stale {
            g.remove(&k);
        }
    }
    // Persist the grant drop so the removed connector's grants don't resurrect
    // from settings.json on the next boot.
    crate::settings::save(state);
    Ok(json!({ "id": id, "removed": true }))
}

// ── governable, read-only metadata ─────────────────────────────────────────────

pub fn list(state: &AppState) -> Result<Value, String> {
    let c = state.connectors.lock().unwrap();
    let mut items: Vec<Value> = c
        .values()
        .map(|d| json!({
            "id": d.id, "display_name": d.display_name, "kind": d.kind,
            "host": if d.kind == "mcp" { mcp_host_label(d) } else { host_of(&d.base_url) },
            "op_count": d.ops.len(),
        }))
        .collect();
    items.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Ok(json!({ "connectors": items }))
}

pub fn describe(args: &Value, state: &AppState) -> Result<Value, String> {
    let id = args.get("id").and_then(|v| v.as_str()).ok_or("id required")?;
    let c = state.connectors.lock().unwrap();
    let d = c.get(id).ok_or_else(|| format!("unknown connector '{id}'"))?;
    let ops: Vec<Value> = d
        .ops
        .iter()
        .map(|o| {
            let mut entry = json!({
                "op_id": o.id, "summary": o.summary, "class": op_class(o),
                "method": o.method, "path": o.path_template, "params": o.allowed_query,
                "default_args": o.default_args,
            });
            // GraphQL ops advertise their variable names + items_path hint in
            // place of REST path/params so the model knows how to call them.
            if let Some(gql) = &o.graphql {
                entry["kind"] = json!("graphql");
                entry["graphql_variables"] = json!(gql.variables);
                if let Some(p) = &gql.items_path {
                    entry["items_path"] = json!(p);
                }
            }
            entry
        })
        .collect();
    let host = if d.kind == "mcp" { mcp_host_label(d) } else { host_of(&d.base_url) };
    // descriptions are connector-supplied → flagged untrusted for the model
    Ok(json!({ "id": d.id, "display_name": d.display_name, "kind": d.kind, "host": host, "ops": ops, "_untrusted_descriptions": true }))
}

/// Derive the args-aware policy key + the informed-consent metadata for a
/// `conn.call`. For anything else, the key is just the capability name.
pub fn gate_key_and_meta(cap: &str, args: &Value, state: &AppState) -> (String, Option<Value>) {
    if cap != "conn.call" {
        return (cap.to_string(), None);
    }
    let cid = args.get("connector").and_then(|v| v.as_str()).unwrap_or("");
    let opid = args.get("op").and_then(|v| v.as_str()).unwrap_or("");
    let c = state.connectors.lock().unwrap();
    if let Some(d) = c.get(cid) {
        if let Some(op) = d.ops.iter().find(|o| o.id == opid) {
            let class = op_class(op);
            let key = format!("conn.call:{}:{}:{}:{}", cid, opid, class, op_hash(d, op));
            let host = if d.kind == "mcp" { mcp_host_label(d) } else { host_of(&d.base_url) };
            let meta = json!({
                "connector": cid, "op": opid, "class": class,
                "host": host, "summary": op.summary,
            });
            return (key, Some(meta));
        }
    }
    (cap.to_string(), None)
}

// ── the one governed connector verb ─────────────────────────────────────────--

pub async fn call(args: &Value, state: &AppState) -> Result<Value, String> {
    let cid = args.get("connector").and_then(|v| v.as_str()).ok_or("connector required")?;
    let opid = args.get("op").and_then(|v| v.as_str()).ok_or("op required")?;
    let (def, op) = {
        let c = state.connectors.lock().unwrap();
        let d = c.get(cid).ok_or_else(|| format!("unknown connector '{cid}'"))?;
        let op = d.ops.iter().find(|o| o.id == opid).ok_or_else(|| format!("unknown op '{opid}'"))?;
        (d.clone(), op.clone())
    };
    let class = op_class(&op);
    let call_args = args.get("args").cloned().unwrap_or_else(|| json!({}));

    // MCP connectors proxy to the live rmcp client instead of egress HTTP. The
    // gate/consent already ran in govern() under the SAME op-hash-pinned key.
    if def.kind == "mcp" {
        return crate::mcp::call(cid, opid, &class, &call_args, state).await;
    }

    // GraphQL ops: POST { query, variables } to base_url, ignoring
    // path_template/method. Auth is injected at the egress boundary exactly as
    // for REST (the header/query secret never enters args, the body, or a log).
    if let Some(gql) = &op.graphql {
        return call_graphql(cid, opid, &class, &def, gql, &call_args, state).await;
    }

    // Expand {name} path params from caller args (whitelist: only declared blanks).
    let mut path = op.path_template.clone();
    while let Some(start) = path.find('{') {
        let end = path[start..].find('}').map(|e| start + e).ok_or("malformed path template")?;
        let name = path[start + 1..end].to_string();
        let val = call_args.get(&name).map(val_to_str).ok_or_else(|| format!("missing path arg '{name}'"))?;
        path.replace_range(start..=end, &urlencode(&val));
    }

    let mut u = reqwest::Url::parse(&def.base_url).map_err(|e| format!("bad base_url: {e}"))?;
    {
        let base = u.path().trim_end_matches('/').to_string();
        u.set_path(&format!("{base}{path}"));
    }
    {
        let mut qp = u.query_pairs_mut();
        for k in &op.allowed_query {
            if let Some(v) = call_args.get(k) {
                qp.append_pair(k, &val_to_str(v));
            }
        }
    }

    // Inject per-connector auth at the egress boundary ONLY. The secret is
    // resolved here from the sealed store, applied to the outbound request, and
    // never enters args, the response, or any log. The AI supplies no headers on
    // this path (the headers Vec below originates here), so it cannot override
    // or read the auth header.
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(auth) = &def.auth {
        // query-scheme: append the secret as a query param so egress re-validates
        // the full URL (host allow-list + IP denylist still apply).
        if auth.scheme == "query" {
            let pname = auth.param_name.as_deref().ok_or("query auth missing param_name")?;
            let cref = auth.cred_ref.as_deref().ok_or("query auth missing cred_ref")?;
            let secret = resolve_cred(state, cref)?;
            u.query_pairs_mut().append_pair(pname, &secret);
        } else if let Some((name, value)) = resolve_auth_header(auth, state)? {
            headers.push((name, value));
        }
    }

    let host = u.host_str().unwrap_or("").to_string();
    let (status, data) = egress::fetch(&op.method, u.as_str(), headers, None, &def.allowed_hosts)
        .await
        .map_err(|e| e.0)?;
    Ok(json!({
        "connector": cid, "op": opid, "ok": status < 400, "status": status,
        "class": class, "host": host, "data": data, "_untrusted": true,
    }))
}

/// POST a GraphQL op. Builds `{ query, variables }` from the op's declared
/// variable names (each looked up in the caller's args), injects connector auth
/// at the egress boundary, and surfaces GraphQL-level errors (HTTP 200 + a
/// top-level `errors` array) as `ok=false` WITHOUT leaking the auth header.
async fn call_graphql(
    cid: &str,
    opid: &str,
    class: &str,
    def: &ConnectorDef,
    gql: &GraphQlBlock,
    call_args: &Value,
    state: &AppState,
) -> Result<Value, String> {
    // Assemble GraphQL variables from the whitelisted names only — anything the
    // op didn't declare is ignored, mirroring the REST allowed_query whitelist.
    let mut vars = Map::new();
    for name in &gql.variables {
        if let Some(v) = call_args.get(name) {
            vars.insert(name.clone(), v.clone());
        }
    }
    let body = json!({ "query": gql.query, "variables": Value::Object(vars) });

    // Auth: GraphQL is always a POST to base_url, so a "query"-scheme secret goes
    // on the URL and header schemes (bearer/header/basic) become a request header
    // — resolved server-side from the sealed store, never placed in the body.
    let mut u = reqwest::Url::parse(&def.base_url).map_err(|e| format!("bad base_url: {e}"))?;
    let mut headers: Vec<(String, String)> = vec![("Content-Type".to_string(), "application/json".to_string())];
    if let Some(auth) = &def.auth {
        if auth.scheme == "query" {
            let pname = auth.param_name.as_deref().ok_or("query auth missing param_name")?;
            let cref = auth.cred_ref.as_deref().ok_or("query auth missing cred_ref")?;
            let secret = resolve_cred(state, cref)?;
            u.query_pairs_mut().append_pair(pname, &secret);
        } else if let Some((name, value)) = resolve_auth_header(auth, state)? {
            headers.push((name, value));
        }
    }

    let host = u.host_str().unwrap_or("").to_string();
    let (status, data) = egress::fetch("POST", u.as_str(), headers, Some(body), &def.allowed_hosts)
        .await
        .map_err(|e| e.0)?;

    // GraphQL signals failure two ways: a non-2xx HTTP status, or HTTP 200 with a
    // top-level `errors` array. Fold both into ok=false and surface the first
    // GraphQL error message (it is server-supplied, not auth-derived).
    let mut ok = status < 400;
    let mut gql_error: Option<String> = None;
    if let Some(errors) = data.get("errors").and_then(|e| e.as_array()) {
        if !errors.is_empty() {
            ok = false;
            gql_error = errors
                .first()
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string());
        }
    }

    let mut out = json!({
        "connector": cid, "op": opid, "ok": ok, "status": status,
        "class": class, "host": host, "data": data, "_untrusted": true,
    });
    if let Some(p) = &gql.items_path {
        out["items_path"] = json!(p);
    }
    if let Some(msg) = gql_error {
        out["error"] = json!(msg);
    }
    Ok(out)
}

fn val_to_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
