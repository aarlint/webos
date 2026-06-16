# webOS connectors — build plan (Phase 0 floor + Phase 1 slice + model-backed compose)

## Phase 0 — security floor (BLOCKING)
- [ ] `egress.rs`: one hardened outbound client — https-only, redirect-none, DNS-resolve + private-IP denylist + IP-pin (anti SSRF / DNS-rebind), size + timeout caps
- [ ] reroute `weather.get` through egress
- [ ] per-session bearer tokens: boot-gen human/ai tokens, `/bootstrap` (localhost) hands them to the console, `?token=` replaces `?as=` on the WS

## Phase 1 — connectors + args-aware gate + informed consent
- [ ] `connectors.rs`: ConnectorDef/OpDef (data, persisted to fs jail), op_hash, load/save
- [ ] caps: `connector.add`/`remove` (PROTECTED), `connector.list`/`describe` (governable), `conn.call` (one governed verb)
- [ ] `conn.call`: path-template expansion + query whitelist + host allow-list + egress
- [ ] P1 args-aware gate: derive policy key `conn.call:<connector>:<op>:<class>:<op_hash>`
- [ ] P3 grants pinned to op_hash; `remove` drops the connector's grants
- [ ] P2 informed consent: approval envelope + modal show connector/op/class/host/untrusted-summary; resolve carries grantKey
- [ ] unsafe_mode carve-out: write-class conn ops still ASK

## Phase 2 (partial) — generative UI
- [ ] widgets: table / list / detail bound to a connector op (pull); XSS-safe (textContent)
- [ ] `ai.compose` → model call (Ollama via egress + CF-Access creds), strict widget validation, deterministic template fallback
- [ ] Settings: Connectors section (list + add GitHub-public preset + remove)

## Verify
- [ ] cargo build clean
- [ ] live: add connector → "show octocat repos as table" → consent (class/host) → table renders real data
- [ ] SSRF: connector pointed at 169.254.169.254 refused
- [ ] AI cannot connector.add (PROTECTED) even in unsafe mode
