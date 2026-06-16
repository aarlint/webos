// webOS shell — window manager + system apps over the capability bus.
// The bus protocol and the Surface widget vocabulary are unchanged. System apps
// (Finder, Settings, Console) are first-party built-ins, but they only ever act
// through bus capabilities — so the AI has the same reach at the capability level.

// ── helpers ───────────────────────────────────────────────────────────────────
const $ = (s) => document.querySelector(s);
function div(cls) { const d = document.createElement("div"); if (cls) d.className = cls; return d; }
function clamp(v, lo, hi) { return Math.max(lo, Math.min(hi, v)); }
function esc(s) { const d = document.createElement("div"); d.textContent = s; return d.innerHTML; }
// Inline-SVG icon markup from the surface bundle's name-keyed set (loaded before
// this script). Falls back to empty string if the bundle isn't ready yet.
function icon(name, size) {
  const f = window.WebOSSurface && window.WebOSSurface.icon;
  return f ? f(name, size || 18) : "";
}

// ── state ─────────────────────────────────────────────────────────────────────
let ws;
let principal = "human";
let booted = false;
let zTop = 10;
let cascade = 0;
let genCount = 0;
const pending = new Map();
const windows = new Map();
const logLines = [];
let logEl = null;
const approvalQueue = [];
let humanToken = null, aiToken = null;

// Frontend payload for the "add example connector" button (operator action).
const GITHUB_PRESET = {
  id: "github-public",
  display_name: "GitHub (public)",
  kind: "manual_rest",
  base_url: "https://api.github.com",
  allowed_hosts: ["api.github.com"],
  ops: [{
    id: "list_repos",
    method: "GET",
    path_template: "/users/{user}/repos",
    allowed_query: ["sort", "per_page"],
    class: "read",
    summary: "List a user's public repositories",
    default_args: { user: "octocat", per_page: "30" },
    default_columns: [
      { header: "Name", path: "name" },
      { header: "Stars", path: "stargazers_count" },
      { header: "Language", path: "language" },
    ],
  }],
};

const APPS = [
  { key: "chat",     label: "Chat",     icon: "message-circle", bg: "linear-gradient(135deg,#5b8cff,#7b3aff)" },
  { key: "finder",   label: "Finder",   icon: "folder",         bg: "linear-gradient(135deg,#4f9bff,#2f6bdb)" },
  { key: "home",     label: "Welcome",  icon: "home",           bg: "linear-gradient(135deg,#7b8cff,#4a4ad8)" },
  { key: "weather",  label: "Weather",  icon: "sun",            bg: "linear-gradient(135deg,#4fc9ff,#1f8fd8)" },
  { key: "notes",    label: "Notes",    icon: "edit",           bg: "linear-gradient(135deg,#ffd25b,#f0a500)" },
  { key: "builder",  label: "Interface Builder", icon: "grid",  bg: "linear-gradient(135deg,#b06aff,#6a3ad8)" },
  { key: "settings", label: "Settings", icon: "settings",       bg: "linear-gradient(135deg,#aab2c2,#5f6877)" },
  { key: "console",  label: "Console",  icon: "terminal",       bg: "linear-gradient(135deg,#3ecf8e,#1f9e69)" },
];
const BUILTIN = { chat: openChat, finder: openFinder, settings: openSettings, console: openConsole, builder: openBuilder };

// ── bus ─────────────────────────────────────────────────────────────────────--
function connect() {
  if (ws) { ws.onclose = null; ws.close(); }
  const token = principal === "human" ? humanToken : aiToken;
  ws = new WebSocket(`ws://${location.host}/ws?token=${encodeURIComponent(token || "")}`);
  ws.onopen = () => {
    log(`connected as ${principal}`);
    refreshSavedApps();
    if (!booted) { booted = true; openApp("chat", "Chat"); }
  };
  ws.onmessage = (ev) => handleMessage(JSON.parse(ev.data));
  ws.onclose = () => log("disconnected");
}

function invoke(capability, args = {}, opts = {}) {
  const id = crypto.randomUUID();
  pending.set(id, { cap: capability, ...opts });
  ws.send(JSON.stringify({ id, capability, args }));
}

function handleMessage(msg) {
  // server-pushed consent prompt (no request id)
  if (msg.type === "approval") { enqueueApproval(msg); return; }
  // server-pushed agent activity → top-right worker toast
  if (msg.type === "activity") { onActivity(msg); return; }

  const meta = pending.get(msg.id) || {};
  pending.delete(msg.id);
  log(`[${principal}] ${meta.cap || "?"} → ${msg.ok ? "ok" : (msg.decision || "err")}` +
      (msg.error ? " — " + msg.error : ""));

  if (meta.then) { meta.then(msg); return; }      // built-in apps handle their own results
  if (!msg.ok) { if (meta.target) setBox(meta.target, "⛔ " + msg.error); return; }
  if (meta.open) { openWindow(meta.key, msg.data.title || meta.title, msg.data); return; }
  if (meta.target) {
    const val = meta.field ? msg.data[meta.field] : (msg.data.summary ?? msg.data);
    setBox(meta.target, val);
  }
}

// ── launching ─────────────────────────────────────────────────────────────────
function openApp(key, label) {
  const w = windows.get(key);
  if (w) { unminimize(w); focusWin(w); return; }
  if (BUILTIN[key]) { BUILTIN[key](); return; }
  invoke("ui.get", { id: key }, { open: true, key, title: label });
}
function compose(intent) {
  invoke("ai.compose", { intent }, { open: true, key: "gen" + (++genCount), title: "Generated" });
}

// ── window manager ────────────────────────────────────────────────────────────
function newShell(key, title, opts) {
  const floating = !!(opts && opts.floating);
  const node = div("win" + (floating ? " floating" : ""));
  // Floating widgets (AI-generated surfaces): no titlebar; hover reveals
  // top-right save-to-dock + close. Built-in apps keep the macOS chrome.
  node.innerHTML = floating
    ? '<div class="fw-controls">' +
        '<button class="fw-btn fw-save" title="Save to dock">' + icon("save", 13) + '</button>' +
        '<button class="fw-btn fw-close" title="Close">' + icon("x", 13) + '</button>' +
      '</div>' +
      '<div class="content"></div>' +
      '<div class="resize"></div>'
    : '<div class="titlebar">' +
        '<div class="lights"><span class="l red"></span><span class="l yellow"></span><span class="l green"></span></div>' +
        '<div class="wtitle"></div>' +
      '</div>' +
      '<div class="content"></div>' +
      '<div class="resize"></div>';

  const W = 540, H = 400;
  const x = 90 + cascade * 26, y = 54 + cascade * 26;
  cascade = (cascade + 1) % 6;
  Object.assign(node.style, { left: x + "px", top: y + "px", width: W + "px", height: H + "px", zIndex: ++zTop });
  if (!floating) node.querySelector(".wtitle").textContent = title;

  const w = { key, node, title, floating };
  windows.set(key, w);
  $("#windows").appendChild(node);

  node.addEventListener("pointerdown", () => focusWin(w), true);
  if (floating) {
    node.querySelector(".fw-close").onclick = (e) => { e.stopPropagation(); closeWin(w); };
    node.querySelector(".fw-save").onclick = (e) => { e.stopPropagation(); saveFloating(w); };
    makeDraggable(w, node); // drag the body
  } else {
    node.querySelector(".l.red").onclick = (e) => { e.stopPropagation(); closeWin(w); };
    node.querySelector(".l.yellow").onclick = (e) => { e.stopPropagation(); minimize(w); };
    node.querySelector(".l.green").onclick = (e) => { e.stopPropagation(); zoom(w); };
    makeDraggable(w, node.querySelector(".titlebar"));
  }
  makeResizable(w);
  focusWin(w);
  refreshDock();
  return w;
}

// Persist a floating widget's surface as a docked app (reuses app.save).
function saveFloating(w) {
  if (!w.surface) { log("nothing to save yet"); return; }
  const title = w.title || "Widget";
  const id = (w.surface.id && String(w.surface.id)) || ("app-" + title.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, ""));
  invoke("app.save", { id, title, glyph: (w.surface && w.surface.icon) || "grid", surface: w.surface }, { then: (m) => {
    if (m.ok) { refreshSavedApps(); log('saved "' + title + '" to the dock'); } else log("save failed: " + m.error);
  }});
}

function openWindow(key, title, surface) {
  let w = windows.get(key);
  if (!w) w = newShell(key, title, { floating: true });
  else { unminimize(w); focusWin(w); }
  w.surface = surface; // retain so save-to-dock needs no refetch
  w.title = surface.title || title;
  const wt = w.node.querySelector(".wtitle");
  if (wt) wt.textContent = w.title;
  const c = w.node.querySelector(".content");
  if (window.WebOSSurface) { try { window.WebOSSurface.unmount(c); } catch (e) {} }
  c.className = "content";
  c.innerHTML = "";
  // json-render spec ({root,elements}) → the real @json-render/react renderer;
  // legacy widget tree ({widget}) → the vanilla renderer.
  if (surface.root && surface.elements && window.WebOSSurface) {
    window.WebOSSurface.mount(c, surface);
  } else if (surface.widget) {
    c.appendChild(renderWidget(surface.widget));
  }
  syncMenuTitle();
}

function focusWin(w) {
  document.querySelectorAll(".win.active").forEach((n) => n.classList.remove("active"));
  w.node.classList.add("active");
  w.node.style.zIndex = ++zTop;
  syncMenuTitle();
}
function closeWin(w) {
  if (window.WebOSSurface) { try { window.WebOSSurface.unmount(w.node.querySelector(".content")); } catch (e) {} }
  w.node.remove();
  windows.delete(w.key);
  if (w.key === "console") logEl = null;
  refreshDock();
  syncMenuTitle();
}
function minimize(w) { w.node.style.display = "none"; w.min = true; refreshDock(); syncMenuTitle(); }
function unminimize(w) { if (w.min) { w.node.style.display = ""; w.min = false; refreshDock(); } }
function zoom(w) {
  if (w.prev) { Object.assign(w.node.style, w.prev); w.prev = null; }
  else {
    w.prev = { left: w.node.style.left, top: w.node.style.top, width: w.node.style.width, height: w.node.style.height };
    Object.assign(w.node.style, { left: "8px", top: "34px", width: (window.innerWidth - 16) + "px", height: (window.innerHeight - 110) + "px" });
  }
}
function makeDraggable(w, handle) {
  handle = handle || w.node.querySelector(".titlebar");
  handle.addEventListener("pointerdown", (e) => {
    if (e.target.classList && e.target.classList.contains("l")) return;
    // floating: drag by body, but keep inputs/buttons/resize + text-selection usable
    if (w.floating && e.target.closest && e.target.closest("input,button,select,textarea,a,.fw-btn,.resize,[contenteditable]")) return;
    const sx = e.clientX, sy = e.clientY, ox = w.node.offsetLeft, oy = w.node.offsetTop;
    // Movement threshold: below 4px it's a CLICK (let it bubble to row/card
    // handlers), above it's a DRAG. No setPointerCapture — capturing on the
    // window handle steals the click event from interactive surface content,
    // which broke follow-up row selection in floating widgets.
    let dragging = false;
    const move = (ev) => {
      const dx = ev.clientX - sx, dy = ev.clientY - sy;
      if (!dragging) {
        if (Math.abs(dx) + Math.abs(dy) < 4) return;
        dragging = true;
        document.body.style.userSelect = "none";
      }
      w.node.style.left = clamp(ox + dx, -w.node.offsetWidth + 80, window.innerWidth - 80) + "px";
      w.node.style.top = clamp(oy + dy, 28, window.innerHeight - 40) + "px";
    };
    const up = () => {
      document.removeEventListener("pointermove", move);
      document.removeEventListener("pointerup", up);
      document.body.style.userSelect = "";
    };
    document.addEventListener("pointermove", move); document.addEventListener("pointerup", up);
  });
}
function makeResizable(w) {
  const h = w.node.querySelector(".resize");
  h.addEventListener("pointerdown", (e) => {
    e.stopPropagation();
    const sx = e.clientX, sy = e.clientY, ow = w.node.offsetWidth, oh = w.node.offsetHeight;
    h.setPointerCapture(e.pointerId);
    const move = (ev) => {
      w.node.style.width = Math.max(300, ow + ev.clientX - sx) + "px";
      w.node.style.height = Math.max(180, oh + ev.clientY - sy) + "px";
    };
    const up = () => { h.removeEventListener("pointermove", move); h.removeEventListener("pointerup", up); };
    h.addEventListener("pointermove", move); h.addEventListener("pointerup", up);
  });
}
function syncMenuTitle() {
  const active = document.querySelector(".win.active");
  let title = "";
  for (const w of windows.values()) if (w.node === active && !w.min) title = w.title;
  $("#mb-wintitle").textContent = title;
}

// ── built-in: Console ───────────────────────────────────────────────────────--
function openConsole() {
  let w = windows.get("console");
  if (w) { unminimize(w); focusWin(w); return; }
  w = newShell("console", "Console");
  const c = w.node.querySelector(".content");
  c.classList.add("console");
  logEl = div("logwrap");
  c.appendChild(logEl);
  renderLog();
}
function log(line) {
  logLines.push(line);
  if (logLines.length > 300) logLines.shift();
  if (logEl) { const d = div(); d.textContent = line; logEl.prepend(d); }
}
function renderLog() {
  if (!logEl) return;
  logEl.innerHTML = "";
  [...logLines].reverse().forEach((l) => { const d = div(); d.textContent = l; logEl.appendChild(d); });
}

// ── built-in: Settings ────────────────────────────────────────────────────────
function openSettings() {
  let w = windows.get("settings");
  if (w) { unminimize(w); focusWin(w); return; }
  w = newShell("settings", "Settings");
  Object.assign(w.node.style, { width: "600px", height: "560px" });
  const c = w.node.querySelector(".content");
  c.classList.add("settings");
  c.innerHTML =
    '<div class="set-sec">' +
      '<h2>AI Safety</h2>' +
      '<div class="set-row"><div><div class="set-label">Unsafe mode</div>' +
        '<div class="set-sub">Let the AI take any action without asking. Operator-only capabilities stay blocked.</div></div>' +
        '<label class="switch"><input type="checkbox" class="set-unsafe"><span class="slider"></span></label></div>' +
    '</div>' +
    '<div class="set-sec">' +
      '<h2>AI Permissions</h2>' +
      '<div class="set-sub">What the AI may do without asking. Anything ungoverned prompts you for consent.</div>' +
      '<div class="set-perms"></div>' +
    '</div>' +
    '<div class="set-sec">' +
      '<h2>Connector Library</h2>' +
      '<div class="set-sub">Browse ready-made connectors and install them in one click. Installing references any credential by name only — set the secret below in Credentials.</div>' +
      '<div class="set-library lib-grid"></div>' +
    '</div>' +
    '<div class="set-sec">' +
      '<h2>Connectors</h2>' +
      '<div class="set-sub">Connect any service as data. The AI reaches every op through one governed verb (conn.call) — ungoverned ops prompt for consent.</div>' +
      '<div class="set-conns"></div>' +
      '<div class="set-row addrow"><button class="act conn-add">+ Add example: GitHub (public)</button></div>' +
    '</div>' +
    '<div class="set-sec">' +
      '<h2>Files &amp; Folders</h2>' +
      '<div class="set-sub">Mount real folders so webOS can read them. The AI is asked for consent on every real-file read; you (the operator) read freely. With no mounts, only the sandbox is reachable.</div>' +
      '<div class="set-mounts"></div>' +
      '<div class="set-row addrow">' +
        '<input class="mount-path" placeholder="absolute path e.g. /Users/you/Documents">' +
        '<button class="act mount-add">Mount</button>' +
      '</div>' +
    '</div>' +
    '<div class="set-sec">' +
      '<h2>Connections &amp; Credentials</h2>' +
      '<div class="set-sub">Secrets are write-only — held by the daemon, never shown to the UI or AI.</div>' +
      '<div class="set-creds"></div>' +
      '<div class="set-row addrow">' +
        '<input class="cred-name" placeholder="name e.g. OPENAI_API_KEY">' +
        '<input class="cred-val" type="password" placeholder="secret value">' +
        '<button class="act cred-add">Add</button>' +
      '</div>' +
    '</div>';

  const unsafeBox = c.querySelector(".set-unsafe");
  const permsHost = c.querySelector(".set-perms");
  const credsHost = c.querySelector(".set-creds");
  const connsHost = c.querySelector(".set-conns");
  const libHost = c.querySelector(".set-library");
  const mountsHost = c.querySelector(".set-mounts");

  c.querySelector(".conn-add").onclick = () =>
    invoke("connector.add", GITHUB_PRESET, { then: () => { loadConns(); loadLibrary(); } });
  function loadConns() {
    invoke("connector.list", {}, { then: (m) => {
      if (!m.ok) return;
      connsHost.innerHTML = "";
      if (!m.data.connectors.length) { const d = div("set-sub"); d.textContent = "No connectors yet."; connsHost.appendChild(d); return; }
      m.data.connectors.forEach((cn) => {
        const row = div("set-row cred-row");
        const lab = div();
        lab.innerHTML = `<div class="set-label">${esc(cn.display_name)}</div><div class="set-sub mono">${esc(cn.host)} · ${cn.op_count} op(s)</div>`;
        row.appendChild(lab);
        const del = document.createElement("button");
        del.className = "act danger"; del.textContent = "Remove";
        del.onclick = () => invoke("connector.remove", { id: cn.id }, { then: () => { loadConns(); loadLibrary(); } });
        row.appendChild(del);
        connsHost.appendChild(row);
      });
    }});
  }

  // Connector Library: browse declarative manifests and install in one click.
  // library.list reports installed state; library.install (operator-only)
  // persists the connector so it appears in Connectors above and conn.call works.
  function loadLibrary() {
    invoke("library.list", {}, { then: (m) => {
      if (!m.ok) { libHost.innerHTML = ""; const d = div("set-sub"); d.textContent = "Library unavailable: " + esc(m.error || ""); libHost.appendChild(d); return; }
      libHost.innerHTML = "";
      const items = (m.data && m.data.connectors) || [];
      if (!items.length) { const d = div("set-sub"); d.textContent = "No connectors in the library."; libHost.appendChild(d); return; }
      items.forEach((it) => {
        const card = div("lib-card");
        const head = div("lib-head");
        const ic = div("lib-icon"); ic.innerHTML = icon(it.icon || "box", 22);
        const titleWrap = div("lib-titlewrap");
        const t = div("lib-title"); t.textContent = it.name || it.id;
        titleWrap.appendChild(t);
        if (it.kind === "mcp") { const k = div("lib-kind"); k.textContent = "MCP"; titleWrap.appendChild(k); }
        head.appendChild(ic); head.appendChild(titleWrap);
        card.appendChild(head);

        const desc = div("lib-desc"); desc.textContent = it.description || "";
        card.appendChild(desc);

        if (it.requires_cred && it.requires_cred.name) {
          const hint = div("lib-hint");
          hint.innerHTML = icon("key", 12) + " Needs credential <span class=\"mono\">" + esc(it.requires_cred.name) + "</span> — set it in Credentials below.";
          card.appendChild(hint);
        }

        const foot = div("lib-foot");
        if (it.installed) {
          const badge = div("lib-installed");
          badge.innerHTML = icon("check", 14) + " Installed";
          foot.appendChild(badge);
          // For mcp connectors, installing only registers them — they still need
          // a live Connect (connector.connect) before conn.call works.
          if (it.kind === "mcp") {
            const conn = document.createElement("button");
            conn.className = "act ghost"; conn.textContent = "Connect";
            conn.onclick = () => invoke("connector.connect", { id: it.id }, { then: (r) => {
              if (!r.ok) log("connect failed: " + r.error);
              loadConns();
            }});
            foot.appendChild(conn);
          }
        } else {
          const btn = document.createElement("button");
          btn.className = "act"; btn.textContent = "Install";
          btn.onclick = () => {
            btn.disabled = true;
            invoke("library.install", { id: it.id }, { then: (r) => {
              btn.disabled = false;
              if (!r.ok) { log("install failed: " + r.error); return; }
              loadConns();
              loadLibrary();
            }});
          };
          foot.appendChild(btn);
        }
        card.appendChild(foot);
        libHost.appendChild(card);
      });
    }});
  }

  c.querySelector(".mount-add").onclick = () => {
    const pathEl = c.querySelector(".mount-path");
    const path = pathEl.value.trim();
    if (!path) return;
    invoke("mount.add", { path }, { then: (m) => {
      if (m.ok) { pathEl.value = ""; loadMounts(); }
      else log("mount failed: " + m.error);
    }});
  };

  unsafeBox.onchange = () => invoke("policy.set_unsafe", { on: unsafeBox.checked }, { then: () => {} });
  c.querySelector(".cred-add").onclick = () => {
    const name = c.querySelector(".cred-name").value.trim();
    const value = c.querySelector(".cred-val").value;
    if (!name || !value) return;
    invoke("creds.set", { name, value }, { then: () => {
      c.querySelector(".cred-name").value = ""; c.querySelector(".cred-val").value = "";
      loadCreds();
    }});
  };

  function loadPerms() {
    invoke("policy.get", {}, { then: (m) => {
      if (!m.ok) return;
      unsafeBox.checked = !!m.data.unsafe_mode;
      permsHost.innerHTML = "";
      m.data.grants.forEach((g) => {
        const row = div("set-row");
        const lab = div(); lab.innerHTML = `<div class="set-label mono">${esc(g.capability)}</div>`;
        row.appendChild(lab);
        const seg = div("seg perm-seg");
        ["allow", "ask", "deny"].forEach((s) => {
          const b = document.createElement("button");
          b.textContent = s;
          if (g.state === s) b.classList.add("active");
          b.onclick = () => invoke("policy.set", { capability: g.capability, state: s }, { then: (r) => {
            if (r.ok) { seg.querySelectorAll("button").forEach((x) => x.classList.remove("active")); b.classList.add("active"); }
          }});
          seg.appendChild(b);
        });
        row.appendChild(seg);
        permsHost.appendChild(row);
      });
    }});
  }
  function loadCreds() {
    invoke("creds.list", {}, { then: (m) => {
      if (!m.ok) return;
      credsHost.innerHTML = "";
      if (!m.data.credentials.length) { const d = div("set-sub"); d.textContent = "No credentials yet."; credsHost.appendChild(d); return; }
      m.data.credentials.forEach((cr) => {
        const row = div("set-row cred-row");
        const lab = div(); lab.innerHTML = `<div class="set-label mono cred-key">${icon("key", 14)} ${esc(cr.name)}</div>`;
        row.appendChild(lab);
        const del = document.createElement("button");
        del.className = "act danger"; del.textContent = "Delete";
        del.onclick = () => invoke("creds.delete", { name: cr.name }, { then: () => loadCreds() });
        row.appendChild(del);
        credsHost.appendChild(row);
      });
    }});
  }
  function loadMounts() {
    invoke("mount.list", {}, { then: (m) => {
      if (!m.ok) { mountsHost.innerHTML = ""; const d = div("set-sub"); d.textContent = "Mounts unavailable: " + esc(m.error); mountsHost.appendChild(d); return; }
      mountsHost.innerHTML = "";
      if (!m.data.mounts.length) { const d = div("set-sub"); d.textContent = "No folders mounted yet."; mountsHost.appendChild(d); return; }
      m.data.mounts.forEach((mt) => {
        const row = div("set-row cred-row");
        const lab = div(); lab.innerHTML = `<div class="set-label mono cred-key">${icon("folder", 14)} ${esc(mt.path)}</div>`;
        row.appendChild(lab);
        const del = document.createElement("button");
        del.className = "act danger"; del.textContent = "Unmount";
        del.onclick = () => invoke("mount.remove", { path: mt.path }, { then: () => loadMounts() });
        row.appendChild(del);
        mountsHost.appendChild(row);
      });
    }});
  }
  loadPerms();
  loadCreds();
  loadLibrary();
  loadConns();
  loadMounts();
}

// ── built-in: Finder ──────────────────────────────────────────────────────────
// Dual-source: the Sandbox source uses the jailed fs.list/fs.read with relative
// paths; each mounted folder is a separate source that uses files.list/files.read
// with absolute real paths. A small switcher picks the source; the grid/nav code
// is shared. Real-file reads are governed (the AI is asked) but the human reads
// freely, so for the operator's own Finder this is seamless.
function openFinder() {
  let w = windows.get("finder");
  if (w) { unminimize(w); focusWin(w); return; }
  w = newShell("finder", "Finder");
  Object.assign(w.node.style, { width: "640px", height: "480px" });
  const c = w.node.querySelector(".content");
  c.classList.add("finder");
  c.innerHTML =
    '<div class="fin-bar"><button class="fin-up">↑</button>' +
      '<select class="fin-src"></select>' +
      '<div class="fin-path"></div></div>' +
    '<div class="fin-grid"></div>';
  const grid = c.querySelector(".fin-grid");
  const pathEl = c.querySelector(".fin-path");
  const srcEl = c.querySelector(".fin-src");

  // A source is either the sandbox (real:false, root "") or a mount (real:true,
  // root = the absolute mount path). `path` is the current location: relative
  // for sandbox, absolute for a mount.
  let src = { real: false, root: "" };
  let path = "";

  function listCap() { return src.real ? "files.list" : "fs.list"; }
  function readCap() { return src.real ? "files.read" : "fs.read"; }

  function load(p) {
    invoke(listCap(), { path: p }, { then: (m) => {
      if (!m.ok) { grid.innerHTML = `<div class="w-unknown">${esc(m.error)}</div>`; return; }
      path = m.data.path || "";
      pathEl.textContent = src.real ? path : "/" + path;
      grid.innerHTML = "";
      m.data.entries.forEach((e) => {
        const it = div("fin-item");
        it.innerHTML = `<div class="fin-icon">${icon(e.dir ? "folder" : "file", 34)}</div><div class="fin-name">${esc(e.name)}</div>`;
        // Mount entries carry an absolute `path`; sandbox entries are relative.
        const child = src.real ? (e.path || (path ? path + "/" + e.name : e.name))
                               : (path ? path + "/" + e.name : e.name);
        it.ondblclick = () => e.dir ? load(child) : openFileView(child, src.real);
        grid.appendChild(it);
      });
    }});
  }

  function goUp() {
    if (src.real) {
      // Stop at the mount root — never navigate above it (the daemon would
      // reject it anyway, but this keeps the UI honest).
      if (!path || path === src.root) return;
      const parts = path.split("/"); parts.pop();
      const up = parts.join("/") || "/";
      load(up.length >= src.root.length ? up : src.root);
    } else {
      if (!path) return;
      const parts = path.split("/"); parts.pop(); load(parts.join("/"));
    }
  }
  c.querySelector(".fin-up").onclick = goUp;

  srcEl.onchange = () => {
    const v = srcEl.value;
    if (v === "__sandbox__") { src = { real: false, root: "" }; load(""); }
    else { src = { real: true, root: v }; load(v); }
  };

  // Populate the source switcher: Sandbox first, then each mount. mount.list is
  // governable; for the human operator it just returns the list.
  function buildSources() {
    srcEl.innerHTML = "";
    const opt = document.createElement("option");
    opt.value = "__sandbox__"; opt.textContent = "Sandbox";
    srcEl.appendChild(opt);
    invoke("mount.list", {}, { then: (m) => {
      if (m.ok && m.data.mounts.length) {
        m.data.mounts.forEach((mt) => {
          const o = document.createElement("option");
          o.value = mt.path;
          o.textContent = mt.path.split("/").filter(Boolean).pop() || mt.path;
          o.title = mt.path;
          srcEl.appendChild(o);
        });
      }
    }});
  }
  buildSources();
  load("");
}
function openFileView(p, real) {
  invoke(real ? "files.read" : "fs.read", { path: p }, { then: (m) => {
    const key = "file:" + p;
    let w = windows.get(key);
    if (!w) w = newShell(key, p.split("/").pop());
    else focusWin(w);
    const c = w.node.querySelector(".content");
    c.className = "content";
    const pre = document.createElement("pre");
    pre.className = "fileview";
    pre.textContent = m.ok ? m.data.content : "⛔ " + m.error;
    c.innerHTML = ""; c.appendChild(pre);
  }});
}

// ── built-in: Chat — the OS's main interface ──────────────────────────────────
// You talk; the server-side agent loop calls bus capabilities as tools (governed
// by consent). Surfaces the assistant renders open as windows.
function openChat() {
  let w = windows.get("chat");
  if (w) { unminimize(w); focusWin(w); return; }
  w = newShell("chat", "Chat");
  Object.assign(w.node.style, { width: "560px", height: "600px", left: "60px", top: "50px" });
  const c = w.node.querySelector(".content");
  c.classList.add("chatapp");
  c.innerHTML =
    '<div class="chat-log"></div>' +
    '<div class="chat-input"><input class="chat-text" placeholder="Ask webOS to do something…"><button class="act chat-send">Send</button></div>';
  const logEl = c.querySelector(".chat-log");
  const input = c.querySelector(".chat-text");
  const btn = c.querySelector(".chat-send");
  const history = [];

  bubble("assistant", "Hi — I'm webOS. Ask me to pull data from your connectors, build a view, or anything else. I'll ask before doing anything new.");

  function bubble(role, text) {
    const b = div("chat-msg " + role);
    b.textContent = text;
    logEl.appendChild(b);
    logEl.scrollTop = logEl.scrollHeight;
    return b;
  }
  function send() {
    const text = input.value.trim();
    if (!text) return;
    input.value = "";
    history.push({ role: "user", content: text });
    bubble("user", text);
    const thinking = bubble("assistant thinking", "…");
    btn.disabled = true;
    invoke("chat.send", { messages: history }, { then: (m) => {
      btn.disabled = false;
      thinking.remove();
      if (!m.ok) { bubble("assistant err", "⛔ " + m.error); return; }
      const reply = m.data.reply || "(no reply)";
      history.push({ role: "assistant", content: reply });
      bubble("assistant", reply);
      (m.data.surfaces || []).forEach((id) => openApp(id, id)); // assistant opened a window
      logEl.scrollTop = logEl.scrollHeight;
    }});
  }
  btn.onclick = send;
  input.onkeydown = (e) => { if (e.key === "Enter") send(); };
  setTimeout(() => input.focus(), 50);
}

// ── built-in: Interface Builder ───────────────────────────────────────────────
// Right = live data fields from a connector op. Left = the interface. Drag a
// field onto the canvas → ai.compose rebuilds the Surface. Save it as a dock app.
function openBuilder() {
  let w = windows.get("builder");
  if (w) { unminimize(w); focusWin(w); return; }
  w = newShell("builder", "Interface Builder");
  Object.assign(w.node.style, { width: "920px", height: "600px", left: "120px", top: "50px" });
  const c = w.node.querySelector(".content");
  c.classList.add("builder");
  c.innerHTML =
    '<div class="b-wrap">' +
      '<div class="b-left">' +
        '<div class="b-toolbar"><input class="b-title" placeholder="App name…"><button class="act b-save">Save as App</button></div>' +
        '<div class="b-canvas"><div class="b-empty">Drag data fields here →<br><span>the AI rebuilds the interface on each drop</span></div></div>' +
      '</div>' +
      '<div class="b-right">' +
        '<div class="b-rtop"><select class="b-conn"></select><select class="b-op"></select><button class="act b-load">Load data</button></div>' +
        '<div class="b-fields"><div class="set-sub">Pick a connector + op, then Load data.</div></div>' +
      '</div>' +
    '</div>';

  const connSel = c.querySelector(".b-conn");
  const opSel = c.querySelector(".b-op");
  const fieldsHost = c.querySelector(".b-fields");
  const canvas = c.querySelector(".b-canvas");
  let current = null;          // the Surface being built
  let sampleCtx = null;        // { connector, op, args, items }
  let opMeta = {};             // connector id -> ops[]

  function renderPreview() {
    if (window.WebOSSurface) { try { window.WebOSSurface.unmount(canvas); } catch (e) {} }
    canvas.innerHTML = "";
    if (!current) { canvas.innerHTML = '<div class="b-empty">Drag data fields here →<br><span>the AI rebuilds the interface on each drop</span></div>'; return; }
    // Flat json-render spec → the React island; legacy {widget} tree → vanilla.
    if (current.root && current.elements && window.WebOSSurface) {
      window.WebOSSurface.mount(canvas, current);
    } else if (current.widget) {
      canvas.appendChild(renderWidget(current.widget));
    }
  }

  invoke("connector.list", {}, { then: (m) => {
    if (!m.ok) return;
    connSel.innerHTML = '<option value="">connector…</option>' +
      (m.data.connectors || []).map((cn) => `<option value="${esc(cn.id)}">${esc(cn.display_name)}</option>`).join("");
  }});

  connSel.onchange = () => {
    opSel.innerHTML = "";
    if (!connSel.value) return;
    invoke("connector.describe", { id: connSel.value }, { then: (m) => {
      if (!m.ok) return;
      opMeta[connSel.value] = m.data.ops || [];
      opSel.innerHTML = (m.data.ops || []).filter((o) => o.class === "read")
        .map((o) => `<option value="${esc(o.op_id)}">${esc(o.op_id)} — ${esc(o.summary || o.method)}</option>`).join("");
    }});
  };

  c.querySelector(".b-load").onclick = () => {
    const connector = connSel.value, op = opSel.value;
    if (!connector || !op) return;
    const ops = opMeta[connector] || [];
    const meta = ops.find((o) => o.op_id === op) || {};
    const args = meta.default_args || {};
    fieldsHost.innerHTML = '<div class="set-sub">loading sample…</div>';
    invoke("conn.call", { connector, op, args }, { then: (m) => {
      const u = unwrap(m);
      if (u.err) { fieldsHost.innerHTML = `<div class="w-unknown">⛔ ${esc(u.err)}</div>`; return; }
      // determine the array path + a sample element to introspect
      let items = "", sample = u.payload;
      if (Array.isArray(u.payload)) { items = ""; sample = u.payload[0] || {}; }
      else if (u.payload && typeof u.payload === "object") {
        const arrKey = Object.keys(u.payload).find((k) => Array.isArray(u.payload[k]));
        if (arrKey) { items = arrKey; sample = u.payload[arrKey][0] || {}; }
      }
      sampleCtx = { connector, op, args, items };
      const fields = flattenFields(sample);
      fieldsHost.innerHTML = "";
      const hint = div("set-sub"); hint.textContent = `${fields.length} fields — drag onto the interface`;
      fieldsHost.appendChild(hint);
      fields.forEach((f) => {
        const chip = div("b-chip"); chip.draggable = true;
        chip.innerHTML = `<span class="b-chip-k">${esc(f.label)}</span><span class="b-chip-v">${esc(f.sample)}</span>`;
        chip.ondragstart = (e) => e.dataTransfer.setData("text/plain", JSON.stringify({
          connector, op, args, items, path: f.path, label: f.label,
        }));
        fieldsHost.appendChild(chip);
      });
    }});
  };

  canvas.ondragover = (e) => { e.preventDefault(); canvas.classList.add("drop"); };
  canvas.ondragleave = () => canvas.classList.remove("drop");
  canvas.ondrop = (e) => {
    e.preventDefault();
    canvas.classList.remove("drop");
    let add; try { add = JSON.parse(e.dataTransfer.getData("text/plain")); } catch { return; }
    canvas.innerHTML = '<div class="b-building">⊞ AI is rebuilding the interface…</div>';
    invoke("ai.compose", { intent: `Add the field "${add.label}" (${add.path}) to the interface.`,
      context: { surface: current, add } }, { then: (m) => {
      if (m.ok) { current = m.data; normalizeBindings(current, sampleCtx); renderPreview(); }
      else { canvas.innerHTML = `<div class="w-unknown">⛔ ${esc(m.error)}</div>`; }
    }});
  };

  c.querySelector(".b-save").onclick = () => {
    if (!current) { log("nothing to save yet — drag a field first"); return; }
    const title = (c.querySelector(".b-title").value.trim()) || "My App";
    const id = "app-" + title.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
    current.id = id; current.title = title;
    invoke("app.save", { id, title, glyph: (current && current.icon) || "grid", surface: current }, { then: (m) => {
      if (m.ok) { refreshSavedApps(); log(`saved "${title}" to the dock`); }
      else log("save failed: " + m.error);
    }});
  };
}
// The model decides component TYPE + columns/labels (the creative part); the
// builder owns the data binding so the model can never mis-bind the source/items.
// Flat spec: rewrite props.source/props.items on every data element.
function normalizeBindings(surface, ctx) {
  if (!ctx || !surface || !surface.elements) return;
  const src = { capability: "conn.call", args: { connector: ctx.connector, op: ctx.op, args: ctx.args } };
  Object.values(surface.elements).forEach((el) => {
    if (!el || typeof el !== "object") return;
    if (el.type === "Table" || el.type === "Chart" || el.type === "Detail" || el.type === "Sparkline") {
      el.props = el.props || {};
      el.props.source = src;
      el.props.items = ctx.items;
    }
  });
}
function flattenFields(obj, prefix = "") {
  let out = [];
  for (const [k, v] of Object.entries(obj || {})) {
    const path = prefix ? prefix + "." + k : k;
    if (v && typeof v === "object" && !Array.isArray(v)) out = out.concat(flattenFields(v, path));
    else out.push({ path, label: k, sample: Array.isArray(v) ? "[…]" : String(v).slice(0, 40) });
  }
  return out.slice(0, 50);
}

// ── AI consent prompt ───────────────────────────────────────────────────────--
function enqueueApproval(m) {
  approvalQueue.push(m);
  log(`approval requested: ${m.capability}`);
  if (approvalQueue.length === 1) showApproval();
}
function showApproval() {
  const m = approvalQueue[0];
  const ov = $("#approval");
  if (!m) { ov.classList.add("hidden"); return; }
  const conn = m.conn;
  const classEl = ov.querySelector(".ap-class");
  if (conn) {
    // connector call — show the daemon-computed class + host + untrusted summary
    ov.querySelector(".ap-cap").textContent = `${conn.connector} · ${conn.op}`;
    classEl.textContent = (conn.class || "").toUpperCase();
    classEl.className = "ap-class " + (conn.class === "write" ? "write" : "read");
    classEl.style.display = "";
    ov.querySelector(".ap-host").textContent = "host: " + (conn.host || "—");
    ov.querySelector(".ap-desc").textContent = conn.summary || "";
    ov.querySelector(".ap-desc").style.display = conn.summary ? "" : "none";
  } else {
    ov.querySelector(".ap-cap").textContent = m.capability;
    classEl.style.display = "none";
    ov.querySelector(".ap-host").textContent = "";
    ov.querySelector(".ap-desc").style.display = "none";
  }
  const args = JSON.stringify(m.args || {});
  ov.querySelector(".ap-args").textContent = args.length > 200 ? args.slice(0, 200) + "…" : args;
  ov.classList.remove("hidden");
}
function resolveApproval(verdict) {
  const m = approvalQueue.shift();
  if (m) invoke("approval.resolve",
    { approvalId: m.approvalId, verdict, grantKey: m.grantKey, capability: m.capability }, { then: () => {} });
  showApproval();
}

// ── Surface widget renderer ─────────────────────────────────────────────────--
function renderWidget(w) {
  switch (w.type) {
    case "stack": return container("w-stack", w.children);
    case "row":   return container("w-row", w.children);
    case "grid":  return container("w-grid", w.children);
    case "card": {
      const e = div("w-card");
      if (w.title) { const h = document.createElement("h3"); h.textContent = w.title; e.appendChild(h); }
      (w.children || []).forEach((c) => e.appendChild(renderWidget(c)));
      return e;
    }
    case "heading": { const e = document.createElement("h2"); e.textContent = w.value || ""; return e; }
    case "text":    { const e = document.createElement("p");  e.textContent = w.value || ""; return e; }
    case "input":   { const e = document.createElement("input"); e.id = "in-" + w.id; e.placeholder = w.placeholder || ""; return e; }
    case "valuebox":{ const e = div("w-valuebox"); e.id = "box-" + w.id; e.textContent = "—"; return e; }
    case "button": {
      const e = document.createElement("button"); e.className = "act"; e.textContent = w.label || "Run";
      e.onclick = () => invoke(w.capability, resolveArgs(w.args || {}), { target: w.bindResultTo, field: w.field });
      return e;
    }
    case "table":  return renderTable(w);
    case "list":   return renderList(w);
    case "detail": return renderDetail(w);
    default: { const e = div("w-unknown"); e.textContent = `[unknown widget: ${w.type}]`; return e; }
  }
}

// ── connector-bound data widgets (pull binding) ───────────────────────────────
// A data widget self-fetches via its source capability (usually conn.call) and
// renders the result. All text goes through textContent — never innerHTML — so
// untrusted external data can't inject markup.
function fetchSource(source, cb) {
  if (!source || !source.capability) { cb({ ok: false, error: "widget has no source" }); return; }
  invoke(source.capability, source.args || {}, { then: cb });
}
function getPath(obj, path) {
  if (!path) return obj;
  return path.split(".").reduce((o, k) => (o == null ? undefined : o[k]), obj);
}
function fmtCell(v) {
  if (v == null) return "";
  return typeof v === "object" ? JSON.stringify(v) : String(v);
}
// conn.call wraps the API payload: { ok, status, data:<payload>, ... }. Unwrap
// to the payload, then pull the array (or single object) the widget binds to.
function unwrap(msg) {
  if (!msg.ok) return { err: msg.error };
  const r = msg.data;
  if (r && r.ok === false) return { err: "HTTP " + (r.status || "error") };
  return { payload: r && Object.prototype.hasOwnProperty.call(r, "data") ? r.data : r };
}
function itemsOf(payload, itemsPath) {
  let v = itemsPath && itemsPath.length ? getPath(payload, itemsPath) : payload;
  if (Array.isArray(v)) return v;
  if (v && typeof v === "object") return [v];
  return [];
}

function renderTable(w) {
  const wrap = div("w-table-wrap");
  const tbl = document.createElement("table"); tbl.className = "w-table";
  const thead = document.createElement("thead"); const htr = document.createElement("tr");
  (w.columns || []).forEach((c) => { const th = document.createElement("th"); th.textContent = c.header || c.path; htr.appendChild(th); });
  thead.appendChild(htr); tbl.appendChild(thead);
  const tbody = document.createElement("tbody"); tbl.appendChild(tbody);
  const status = div("w-data-status"); status.textContent = "loading…";
  wrap.appendChild(tbl); wrap.appendChild(status);
  fetchSource(w.source, (msg) => {
    const u = unwrap(msg);
    if (u.err) { status.textContent = "⛔ " + u.err; return; }
    const rows = itemsOf(u.payload, w.items);
    tbody.innerHTML = "";
    rows.forEach((item) => {
      const tr = document.createElement("tr");
      (w.columns || []).forEach((c) => { const td = document.createElement("td"); td.textContent = fmtCell(getPath(item, c.path)); tr.appendChild(td); });
      tbody.appendChild(tr);
    });
    status.textContent = rows.length ? "" : "no rows";
  });
  return wrap;
}

function renderList(w) {
  const wrap = div("w-list");
  const status = div("w-data-status"); status.textContent = "loading…";
  wrap.appendChild(status);
  fetchSource(w.source, (msg) => {
    const u = unwrap(msg);
    if (u.err) { status.textContent = "⛔ " + u.err; return; }
    const rows = itemsOf(u.payload, w.items);
    status.textContent = rows.length ? "" : "no items";
    rows.forEach((item) => {
      const card = div("w-card");
      (w.fields || []).forEach((f) => {
        const line = div("w-kv");
        const k = document.createElement("span"); k.className = "w-k"; k.textContent = (f.label || f.path) + ": ";
        const v = document.createElement("span"); v.textContent = fmtCell(getPath(item, f.path));
        line.appendChild(k); line.appendChild(v); card.appendChild(line);
      });
      wrap.appendChild(card);
    });
  });
  return wrap;
}

function renderDetail(w) {
  const card = div("w-card");
  const status = div("w-data-status"); status.textContent = "loading…";
  card.appendChild(status);
  fetchSource(w.source, (msg) => {
    const u = unwrap(msg);
    if (u.err) { status.textContent = "⛔ " + u.err; return; }
    status.textContent = "";
    const obj = itemsOf(u.payload, w.items)[0] || {};
    (w.fields || []).forEach((f) => {
      const line = div("w-kv");
      const k = document.createElement("span"); k.className = "w-k"; k.textContent = (f.label || f.path) + ": ";
      const v = document.createElement("span"); v.textContent = fmtCell(getPath(obj, f.path));
      line.appendChild(k); line.appendChild(v); card.appendChild(line);
    });
  });
  return card;
}
function container(cls, children) { const e = div(cls); (children || []).forEach((c) => e.appendChild(renderWidget(c))); return e; }
function resolveArgs(args) {
  const out = {};
  for (const [k, v] of Object.entries(args)) {
    if (v && typeof v === "object" && "$input" in v) { const el = document.getElementById("in-" + v["$input"]); out[k] = el ? el.value : ""; }
    else out[k] = v;
  }
  return out;
}
function setBox(id, val) { const el = document.getElementById("box-" + id); if (el) el.textContent = typeof val === "string" ? val : JSON.stringify(val); }

// ── dock ────────────────────────────────────────────────────────────────────--
function buildDock() {
  const dock = $("#dock");
  dock.innerHTML = "";
  APPS.forEach((a) => {
    const item = div("dock-item"); item.dataset.key = a.key; item.title = a.label;
    const tile = div("dock-tile"); tile.style.background = a.bg; tile.innerHTML = icon(a.icon, 24);
    item.appendChild(tile); item.appendChild(div("dock-dot"));
    item.onclick = () => openApp(a.key, a.label);
    dock.appendChild(item);
  });
  dock.appendChild(div("dock-sep"));
  dock.appendChild(div("dock-apps")); // saved docked apps land here
  dock.appendChild(div("dock-sep"));
  const plus = div("dock-item"); plus.title = "Build a screen (⌘Space)";
  const ptile = div("dock-tile plus"); ptile.innerHTML = icon("plus", 24); plus.appendChild(ptile); plus.appendChild(div("dock-dot"));
  plus.onclick = showSpotlight;
  dock.appendChild(plus);
}
function refreshDock() {
  document.querySelectorAll(".dock-item").forEach((it) => {
    const w = windows.get(it.dataset.key);
    const dot = it.querySelector(".dock-dot");
    if (dot) dot.classList.toggle("on", !!(w && !w.min));
  });
}
// Saved Surfaces appear in the dock as launchable apps.
function refreshSavedApps() {
  invoke("app.list", {}, { then: (m) => {
    if (!m.ok) return;
    const host = document.querySelector("#dock .dock-apps");
    if (!host) return;
    host.innerHTML = "";
    (m.data.apps || []).forEach((a) => {
      const item = div("dock-item"); item.dataset.key = a.id; item.title = a.title;
      const tile = div("dock-tile"); tile.style.background = "linear-gradient(135deg,#b06aff,#6a3ad8)";
      // a.glyph holds an icon name now; legacy emoji values render the grid fallback.
      const svg = icon(a.glyph, 24);
      tile.innerHTML = (svg && svg.indexOf("<path") !== -1) ? svg : icon("grid", 24);
      item.appendChild(tile); item.appendChild(div("dock-dot"));
      item.onclick = () => openApp(a.id, a.title);
      host.appendChild(item);
    });
    refreshDock();
  }});
}

// ── Spotlight ───────────────────────────────────────────────────────────────--
function showSpotlight() { $("#spotlight").classList.remove("hidden"); const i = $("#sl-input"); i.value = ""; i.focus(); }
function hideSpotlight() { $("#spotlight").classList.add("hidden"); }

// ── wiring ────────────────────────────────────────────────────────────────────
$("#sl-input").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { const v = e.target.value.trim(); hideSpotlight(); if (v) compose(v); }
  if (e.key === "Escape") hideSpotlight();
});
$("#spotlight").addEventListener("pointerdown", (e) => { if (e.target.id === "spotlight") hideSpotlight(); });
window.addEventListener("keydown", (e) => {
  if ((e.metaKey || e.ctrlKey) && e.code === "Space") {
    e.preventDefault();
    $("#spotlight").classList.contains("hidden") ? showSpotlight() : hideSpotlight();
  }
});
$("#approval .ap-once").onclick = () => resolveApproval("allow_once");
$("#approval .ap-always").onclick = () => resolveApproval("allow_always");
$("#approval .ap-deny").onclick = () => resolveApproval("deny");

$("#principal-seg").querySelectorAll("button").forEach((b) => {
  b.onclick = () => {
    if (b.dataset.p === principal) return;
    $("#principal-seg .active").classList.remove("active"); b.classList.add("active");
    principal = b.dataset.p; connect();
  };
});
function tick() {
  const d = new Date();
  $("#clock").textContent = d.toLocaleDateString([], { weekday: "short" }) + "  " + d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
}

// ── global agent-worker toast ─────────────────────────────────────────────────
// Ref-counts concurrent ai tasks (the bus spawns a task per invocation); shows a
// spinner + latest label whenever ANY ai-principal work is in flight, whether or
// not the user triggered it. Pushed by kerneld's gate (govern) over the bus.
const aiInflight = new Map();
let toastHideTimer = null;
function onActivity(msg) {
  if (msg.state === "done") aiInflight.delete(msg.token);
  else aiInflight.set(msg.token || "x", msg.label || "Working…");
  renderToast();
}
function renderToast() {
  const el = $("#agent-toast");
  if (!el) return;
  const labels = [...aiInflight.values()];
  if (labels.length) {
    if (toastHideTimer) { clearTimeout(toastHideTimer); toastHideTimer = null; }
    el.querySelector(".at-label").textContent = labels[labels.length - 1];
    el.classList.add("show");
  } else if (!toastHideTimer) {
    // debounce hide to avoid flicker between back-to-back tool calls
    toastHideTimer = setTimeout(() => { el.classList.remove("show"); toastHideTimer = null; }, 600);
  }
}

// Let json-render navigate (and links) open apps/surfaces by key.
function webosOpen(key) { openApp(key, key); }
window.webos = { open: webosOpen };

// ── boot ───────────────────────────────────────────────────────────────────--
async function boot() {
  try {
    const t = await (await fetch("/bootstrap")).json();
    humanToken = t.human_token;
    aiToken = t.ai_token;
  } catch (e) {
    log("bootstrap failed: " + e);
  }
  connect();
}
buildDock();
tick();
setInterval(tick, 1000);
boot();
