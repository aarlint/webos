//! The egress chokepoint — the hard security floor for ALL outbound traffic.
//!
//! Every external request (weather.get, connector calls, the model API) goes
//! through `fetch`. It enforces: https-only, no redirects, DNS resolved + every
//! resolved IP validated against a private/loopback/link-local denylist, the
//! connection pinned to the validated IP (kills DNS-rebind TOCTOU), a host
//! allow-list, and response size + timeout caps. Nothing — not unsafe_mode, not
//! an AI-authored Surface — can route around this.

use futures_util::stream::StreamExt;
use serde_json::Value;
use std::net::{IpAddr, Ipv6Addr};
use std::time::Duration;

const MAX_BYTES: usize = 8 * 1024 * 1024;
// Generous enough to cover a cold model load on the first ai.compose call.
const TIMEOUT: Duration = Duration::from_secs(60);

pub struct EgressError(pub String);

fn v6_is_ula(a: &Ipv6Addr) -> bool {
    (a.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
}
fn v6_is_link_local(a: &Ipv6Addr) -> bool {
    (a.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10
}

/// Reject anything that isn't a routable public address.
fn ip_is_public(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.octets()[0] == 0
                || v4.octets()[0] >= 224) // multicast/reserved 224.0.0.0+
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || v6_is_ula(v6) || v6_is_link_local(v6))
        }
    }
}

/// Validate a URL against the egress floor WITHOUT making a request: https-only,
/// no embedded credentials, a real host, and every resolved IP public (no
/// private/loopback/link-local). Used to vet an MCP http/sse endpoint before
/// handing it to the rmcp transport, which does its own connection. Note: rmcp
/// opens its own socket, so this is a best-effort pre-flight (it does not pin
/// the IP for rmcp's later connection), but it still blocks the obvious
/// `http://`, credentials-in-URL, and internal-host SSRF attempts at connect.
pub async fn assert_public_https(url: &str) -> Result<(), EgressError> {
    let parsed = reqwest::Url::parse(url).map_err(|e| EgressError(format!("bad url: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(EgressError("mcp http/sse endpoint must be https".into()));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(EgressError("credentials in URL are not allowed".into()));
    }
    let host = parsed.host_str().ok_or_else(|| EgressError("url has no host".into()))?.to_string();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| EgressError(format!("dns resolution failed: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(EgressError("dns returned no addresses".into()));
    }
    for a in &addrs {
        if !ip_is_public(&a.ip()) {
            return Err(EgressError(format!("blocked non-public address {} (SSRF guard)", a.ip())));
        }
    }
    Ok(())
}

/// Gate an MCP http/sse endpoint before handing it to the rmcp transport.
///
/// In production this is exactly `assert_public_https`: https-only, no embedded
/// credentials, a real host, every resolved IP public. For local development
/// against a mock MCP server, setting the env flag `WEBOS_ALLOW_LOCAL_MCP=1`
/// relaxes the floor to ALSO permit `http://` (and https) to a LOOPBACK or
/// private/link-local host only — it never permits plaintext `http://` to a
/// public host, and it never widens the door for any other egress path
/// (weather, REST connectors, the model API all stay on the strict floor).
/// The flag is OFF by default, so production behavior is unchanged.
pub async fn assert_mcp_endpoint(url: &str) -> Result<(), EgressError> {
    let local_ok = std::env::var("WEBOS_ALLOW_LOCAL_MCP").map(|v| v == "1").unwrap_or(false);
    if !local_ok {
        return assert_public_https(url).await;
    }
    // Local-dev bypass: allow http|https, but only to a NON-public (loopback /
    // private / link-local) host — a plaintext public endpoint is still refused.
    let parsed = reqwest::Url::parse(url).map_err(|e| EgressError(format!("bad url: {e}")))?;
    let scheme = parsed.scheme();
    if scheme != "https" && scheme != "http" {
        return Err(EgressError("mcp endpoint must be http or https".into()));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(EgressError("credentials in URL are not allowed".into()));
    }
    let host = parsed.host_str().ok_or_else(|| EgressError("url has no host".into()))?.to_string();
    let port = parsed.port_or_known_default().unwrap_or(if scheme == "https" { 443 } else { 80 });
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| EgressError(format!("dns resolution failed: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(EgressError("dns returned no addresses".into()));
    }
    if scheme == "http" {
        // Plaintext is only tolerated for a non-public target under the flag.
        for a in &addrs {
            if ip_is_public(&a.ip()) {
                return Err(EgressError(
                    "WEBOS_ALLOW_LOCAL_MCP permits http only to a local/private host".into(),
                ));
            }
        }
        tracing::warn!("WEBOS_ALLOW_LOCAL_MCP: permitting plaintext mcp endpoint to local host '{host}' (DEV ONLY)");
        Ok(())
    } else {
        // https to any host is fine even under the flag.
        Ok(())
    }
}

/// `allowed_hosts` empty = no host restriction (only the IP denylist applies);
/// non-empty = the URL host must be in the list.
pub async fn fetch(
    method: &str,
    url: &str,
    headers: Vec<(String, String)>,
    body: Option<Value>,
    allowed_hosts: &[String],
) -> Result<(u16, Value), EgressError> {
    let parsed = reqwest::Url::parse(url).map_err(|e| EgressError(format!("bad url: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(EgressError("only https egress is allowed".into()));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(EgressError("credentials in URL are not allowed".into()));
    }
    let host = parsed.host_str().ok_or_else(|| EgressError("url has no host".into()))?.to_string();
    if !allowed_hosts.is_empty() && !allowed_hosts.iter().any(|h| h.eq_ignore_ascii_case(&host)) {
        return Err(EgressError(format!("host '{host}' is not in the connector allow-list")));
    }
    let port = parsed.port_or_known_default().unwrap_or(443);

    // Resolve and validate EVERY answer, then pin to one.
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| EgressError(format!("dns resolution failed: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(EgressError("dns returned no addresses".into()));
    }
    for a in &addrs {
        if !ip_is_public(&a.ip()) {
            return Err(EgressError(format!("blocked non-public address {} (SSRF guard)", a.ip())));
        }
    }
    let pinned = addrs[0];

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .user_agent("webOS/0.1") // many APIs (e.g. GitHub) reject requests with no UA
        .resolve(&host, pinned) // pin to the validated IP — no second, rebindable lookup
        .build()
        .map_err(|e| EgressError(e.to_string()))?;

    let m = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| EgressError(format!("bad method '{method}'")))?;
    let mut req = client.request(m, parsed);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }

    let resp = req.send().await.map_err(|e| EgressError(format!("request failed: {e}")))?;
    let status = resp.status().as_u16();

    // Size-capped streaming read — never buffer an unbounded body.
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| EgressError(format!("read failed: {e}")))?;
        if buf.len() + chunk.len() > MAX_BYTES {
            return Err(EgressError("response exceeds size cap".into()));
        }
        buf.extend_from_slice(&chunk);
    }

    let data = serde_json::from_slice::<Value>(&buf)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&buf).to_string()));
    Ok((status, data))
}
