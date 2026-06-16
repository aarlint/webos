# Lessons

## webOS / kerneld

- Routine scheduler: anchor `last_run = now` (NOT 0) on `routine.set`, so the
  first fire lands one full interval LATER, not on the next tick. Firing
  instantly on save (a) surprises the operator and (b) makes a fail-closed test
  impossible — you can't set a routine and go offline before it runs if it races
  the next tick. The scheduler must snapshot due routines under the std Mutex,
  release, THEN await `gate::govern` per step (never hold the lock across await),
  and mark `last_run` immediately on dispatch to avoid overlapping re-fires.
- Fail-closed test gotcha: to exercise the ASK→no-operator→deny path you need a
  capability that resolves to the ASK *tier*, not the deny-by-grant tier. Every
  GOVERNABLE cap has a default grant (mostly `allow`; `fs.write`=`deny`); none
  default to `ask`. `ui.patch` defaults to `allow` so a routine step using it
  RUNS (wrong test). Flip a cap to `ask` first (`policy.set fs.write ask`), then
  with no operator the gate returns Deny "no operator online to approve" — the
  genuine fail-closed signal (assert on that substring + that no file was
  written), distinct from "ai denied 'X' by policy" (the deny-grant path).
- @json-render/core `createSpecStreamCompiler()` is the client streaming hook:
  `push(chunk) -> {result, newPatches}`, `getResult()`, `reset()`. Chunks are
  SpecStream = JSONL patch lines. Repaint only when `newPatches.length > 0`.
  Skip `harden()` (autoFix+validateSpec) on PARTIAL specs — an incomplete spec
  always fails validateSpec; only run harden() on the final `done()` paint, and
  guard partial renders on `spec.elements[spec.root]` existing.

- macOS Keychain (keyring `apple-native`) can pop an interactive authorization
  dialog for an unsigned dev binary on cred set/get. Rapid back-to-back fresh
  processes hitting the same item may throttle/block → spurious TimeoutError in
  smoke tests. The keyring code path itself is fine. For deterministic CI use
  `WEBOS_SECRETS_BACKEND=file` (forces the encrypted-file backend). The Pi
  target uses the file backend anyway.
- keyring has NO "enumerate entries" API. To rehydrate creds at boot you must
  keep a separate names-only index on disk (creds_index.json); values stay in
  the OS store.
- `sync-secret-service` keyring feature pulls in `dbus` (libdbus C dep) — too
  heavy for a headless Pi. Compile keyring with ONLY `apple-native` and let
  Linux/Pi fall back to the self-sealed encrypted file (no system libs).
- macOS has no `timeout(1)`; rely on in-script timeouts.
- rmcp 1.7 client lifecycle: `().serve(transport).await` returns a
  `RunningService<RoleClient,()>` (`()` impls `ClientHandler`; initialize runs
  automatically). Keep the `RunningService` to hold the connection/child alive;
  `.peer()` is a cheaply-`Clone` `Peer<RoleClient>` exposing `list_all_tools()`
  and `call_tool(CallToolRequestParams)`. To honor "never hold a std/async Mutex
  across .await", clone the `Peer` out under the lock, release, THEN await.
  `CallToolRequestParam` is deprecated → use `CallToolRequestParams::new(name)`
  + `.arguments`. stdio: build a `tokio::process::Command`, set `.env()` (NEVER
  args) for secret injection, pass to `TokioChildProcess::new` (`.configure(..)`
  ext trait helps). http: `StreamableHttpClientTransportConfig::with_uri` +
  `auth_header = Some(bare_token)` (rmcp applies `Authorization: Bearer`).
- MCP tool metadata + results are UNTRUSTED. Derive op class from the tool NAME
  (read-like prefixes) as the floor; let the server's `destructiveHint`/
  `readOnlyHint` only DOWNGRADE a read→write, never upgrade write→read. Verified
  via a `get_`-named tool flagged `destructiveHint:true` correctly landing WRITE.
