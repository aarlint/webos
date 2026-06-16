# webOS — Phase 0 spike

A web-native OS where the **only** interaction surface is a webpage, the human
and an AI are **peer principals on one capability bus**, and screens are
**generated data**, not hard-coded apps.

This spike proves the load-bearing ideas on a dev machine. Pi + embedded web
engine (WPE WebKit) + Cedar policy + a real model come in later phases.

## Architecture (this spike)

```
browser (web shell)  ─┐
                      ├─►  ws://…/ws?as=<principal>  ─►  kerneld
AI agent (later)     ─┘                                   ├─ policy gate (one decision point)
                                                          ├─ capability dispatch
                                                          └─ Surfaces (UI as data)
capabilities: sys.info · fs.read · fs.write · weather.get(real API) · ui.* · ai.compose
```

- **One bus, two principals.** Buttons and (later) AI tool-calls emit the same
  invocation envelope `{ id, capability, args }`. Parity is structural.
- **One gate.** `src/policy.rs` decides allow/deny per principal+capability.
  Human = root; AI = default-deny allowlist. `fs.write` is denied for AI — read
  but not write — enforced at the bus, not in the UI.
- **UI as data.** A *Surface* is a widget-tree document (`src/surface.rs`). The
  shell (`web/shell.js`) renders the vocabulary and knows nothing about features.
- **Generative.** `ai.compose` turns an intent into a Surface. Today it's a
  rule-based stub; it's the exact seam for a model (Ollama / nemotron / Claude).

## Run

```bash
cd ~/Documents/GitHub/webos
cargo run                       # kerneld on http://127.0.0.1:8080
# open http://127.0.0.1:8080
```

## The demo that sells it

1. Home renders (weather + note cards) — generated from a Surface, not coded.
2. **Refresh** weather → real temperature from open-meteo.
3. As **Human**: type a note, **Save** → ok. **Load** → reads it back.
4. Switch principal to **AI** (right panel) → **Save** → `⛔ ai is not permitted
   to write files`. **Load** still works. Same UI, same bus, one policy line.
5. Composer: type *"show me weather"* / *"make a notes page"* → the OS builds a
   new screen on the fly.

Watch `cargo run`'s logs and the in-app audit panel — every invocation is
recorded with principal + decision.

## Layout

| File | Role |
|------|------|
| `src/main.rs` | kerneld: WS bus, envelope parse, the policy gate |
| `src/policy.rs` | allow/deny per principal+capability (→ Cedar later) |
| `src/caps.rs` | capability providers (local verbs + API wrappers) |
| `src/surface.rs` | default + generated Surfaces (UI as data) |
| `web/shell.js` | generic Surface renderer + capability invoker |

## Your contribution

`web/shell.js` `renderWidget()` has a marked `TODO` — extend the widget
vocabulary (e.g. `table`, `chart`, `toggle`) and decide pull-vs-push binding.
That vocabulary is the soul of the generative UI.

## Roadmap

- **P1** Cedar policy + permission-matrix Surface + real AI agent client (parity proof)
- **P2** provider modules + capability registry → AI tool manifest
- **P3** Buildroot image, boot-to-kerneld on Pi, **WPE WebKit** kiosk (the embedded engine, not a browser)
- **P4** OIDC, remote+TLS, audit UI
