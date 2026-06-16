// webOS surface renderer — @json-render/react with a MERGED catalog: the full
// @json-render/shadcn presentational library (real shadcn/ui + Radix + Tailwind
// components) PLUS webOS's own connector-bound data/display widgets (Table,
// Chart, Board, Metric, Detail, KeyValue, Sparkline, Icon) and the layout/form
// primitives the backend caps already emit (Stack/Row/Grid/Card/Heading/Text/
// Badge/Progress/Input/Toggle/Button). State bindings ($state/$template/$cond/
// $computed), visibility, repeat, and the built-in actions are unlocked by
// lifting spec.state into the provider. Data widgets bridge to the capability
// bus via the shell's window.invoke.
//
// CSS: ./surface.css is compiled by @tailwindcss/vite into web/surface.css and
// scoped to `.webos-surface` (the mount container, see mount() below) so the
// Tailwind/shadcn styles NEVER touch the vanilla shell chrome (web/style.css).
import { useEffect, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import { autoFixSpec, validateSpec, createSpecStreamCompiler } from "@json-render/core";
import { defineRegistry, Renderer, JSONUIProvider, useBoundProp, useActions } from "@json-render/react";
import { shadcnComponents } from "@json-render/shadcn";
// The component vocabulary + prop schemas live in catalog.ts so the prompt
// generator (gen-prompt.mjs → web/catalog-prompt.txt) and this renderer share
// ONE definition. Here we only attach the React implementations.
import { catalog } from "./catalog";
import "./surface.css";

// ── bus bridge + data helpers ──────────────────────────────────────────────────
function invoke(capability: string, args: any): Promise<any> {
  return new Promise((resolve) => {
    const w = window as any;
    if (!w.invoke) return resolve({ ok: false, error: "bus unavailable" });
    w.invoke(capability, args || {}, { then: resolve });
  });
}
function getPath(o: any, p?: string) {
  if (!p) return o;
  return p.split(".").reduce((x: any, k: string) => (x == null ? undefined : x[k]), o);
}
function unwrap(msg: any) {
  if (!msg || !msg.ok) return { error: (msg && msg.error) || "error" };
  const r = msg.data;
  if (r && r.ok === false) return { error: "HTTP " + (r.status || "error") };
  return { payload: r && Object.prototype.hasOwnProperty.call(r, "data") ? r.data : r };
}
function itemsOf(payload: any, items?: string) {
  let v = items && items.length ? getPath(payload, items) : payload;
  if (Array.isArray(v)) return v;
  if (v && typeof v === "object") return [v];
  return [];
}
function fmt(v: any) {
  if (v == null) return "";
  return typeof v === "object" ? JSON.stringify(v) : String(v);
}
function num(v: any) { const n = Number(v); return isFinite(n) ? n : 0; }
// Fetches the source once, then (if refresh>0 seconds) polls on an interval
// while mounted — live data. The interval is cleared on unmount (window close).
// Poll errors keep the last-good data (no flicker); only the initial load shows
// loading/errors.
function useSource(source: any, refresh?: number) {
  const [st, setSt] = useState<any>({ loading: true });
  const secs = Number(refresh) || 0;
  useEffect(() => {
    let live = true;
    if (!source || !source.capability) { setSt({ error: "widget has no source" }); return; }
    const fetchOnce = (initial: boolean) => {
      invoke(source.capability, source.args).then((msg) => {
        if (!live) return;
        const u = unwrap(msg);
        if (u.error && !initial) return; // transient poll error → keep last-good
        setSt(u);
      });
    };
    fetchOnce(true);
    const timer = secs > 0 ? setInterval(() => fetchOnce(false), secs * 1000) : null;
    return () => { live = false; if (timer) clearInterval(timer); };
  }, [JSON.stringify(source), secs]);
  return st;
}

// ── inline-SVG icon set (lucide-style line icons; name-keyed = injection-safe) ──
const ICONS: Record<string, string> = {
  star: "M12 2l3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01L12 2z",
  check: "M20 6L9 17l-5-5",
  x: "M18 6L6 18M6 6l12 12",
  alert: "M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0zM12 9v4M12 17h.01",
  activity: "M22 12h-4l-3 9L9 3l-3 9H2",
  chart: "M3 3v18h18M7 16v-5M12 16V8M17 16v-9",
  folder: "M3 7a2 2 0 012-2h4l2 2h8a2 2 0 012 2v8a2 2 0 01-2 2H5a2 2 0 01-2-2V7z",
  file: "M14 2H6a2 2 0 00-2 2v16a2 2 0 002 2h12a2 2 0 002-2V8zM14 2v6h6",
  cloud: "M17.5 19a4.5 4.5 0 100-9h-1.8A7 7 0 104 14.9",
  user: "M20 21v-2a4 4 0 00-4-4H8a4 4 0 00-4 4v2M16 7a4 4 0 11-8 0 4 4 0 018 0z",
  mail: "M4 4h16a2 2 0 012 2v12a2 2 0 01-2 2H4a2 2 0 01-2-2V6a2 2 0 012-2zM22 6l-10 7L2 6",
  calendar: "M8 2v4M16 2v4M3 10h18M5 4h14a2 2 0 012 2v14a2 2 0 01-2 2H5a2 2 0 01-2-2V6a2 2 0 012-2z",
  git: "M6 3v12M18 9a3 3 0 100-6 3 3 0 000 6zM6 21a3 3 0 100-6 3 3 0 000 6zM15 6a9 9 0 01-9 9",
  search: "M10 18a8 8 0 100-16 8 8 0 000 16zM21 21l-5-5",
  clock: "M12 22a10 10 0 100-20 10 10 0 000 20zM12 6v6l4 2",
  database: "M12 8c4.42 0 8-1.34 8-3s-3.58-3-8-3-8 1.34-8 3 3.58 3 8 3zM4 5v6c0 1.66 3.58 3 8 3s8-1.34 8-3V5M4 11v6c0 1.66 3.58 3 8 3s8-1.34 8-3v-6",
  // dock + control glyphs (added to de-emoji the shell)
  home: "M3 9.5L12 3l9 6.5M5 9v11a1 1 0 001 1h12a1 1 0 001-1V9",
  "message-circle": "M21 11.5a8.38 8.38 0 01-8.5 8.5 8.38 8.38 0 01-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 01-.9-3.8 8.5 8.5 0 0117 0z",
  sun: "M12 17a5 5 0 100-10 5 5 0 000 10zM12 1v2M12 21v2M4.22 4.22l1.42 1.42M18.36 18.36l1.42 1.42M1 12h2M21 12h2M4.22 19.78l1.42-1.42M18.36 5.64l1.42-1.42",
  edit: "M11 4H4a2 2 0 00-2 2v14a2 2 0 002 2h14a2 2 0 002-2v-7M18.5 2.5a2.121 2.121 0 013 3L12 15l-4 1 1-4 9.5-9.5z",
  grid: "M3 3h7v7H3zM14 3h7v7h-7zM14 14h7v7h-7zM3 14h7v7H3z",
  settings: "M12 15a3 3 0 100-6 3 3 0 000 6zM19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 11-2.83 2.83l-.06-.06a1.65 1.65 0 00-1.82-.33 1.65 1.65 0 00-1 1.51V21a2 2 0 01-4 0v-.09A1.65 1.65 0 009 19.4a1.65 1.65 0 00-1.82.33l-.06.06a2 2 0 11-2.83-2.83l.06-.06a1.65 1.65 0 00.33-1.82 1.65 1.65 0 00-1.51-1H3a2 2 0 010-4h.09A1.65 1.65 0 004.6 9a1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 112.83-2.83l.06.06a1.65 1.65 0 001.82.33H9a1.65 1.65 0 001-1.51V3a2 2 0 014 0v.09a1.65 1.65 0 001 1.51 1.65 1.65 0 001.82-.33l.06-.06a2 2 0 112.83 2.83l-.06.06a1.65 1.65 0 00-.33 1.82V9a1.65 1.65 0 001.51 1H21a2 2 0 010 4h-.09a1.65 1.65 0 00-1.51 1z",
  terminal: "M4 17l6-6-6-6M12 19h8",
  download: "M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4M7 10l5 5 5-5M12 15V3",
  plus: "M12 5v14M5 12h14",
  refresh: "M23 4v6h-6M1 20v-6h6M3.51 9a9 9 0 0114.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0020.49 15",
  trash: "M3 6h18M19 6v14a2 2 0 01-2 2H7a2 2 0 01-2-2V6m3 0V4a2 2 0 012-2h4a2 2 0 012 2v2M10 11v6M14 11v6",
  save: "M19 21H5a2 2 0 01-2-2V5a2 2 0 012-2h11l5 5v11a2 2 0 01-2 2zM17 21v-8H7v8M7 3v5h8",
  key: "M21 2l-2 2m-7.61 7.61a5.5 5.5 0 11-7.778 7.778 5.5 5.5 0 017.777-7.777zm0 0L15.5 7.5m0 0l3 3L22 7l-3-3m-3.5 3.5L19 4",
};

// SVG markup string for a name (used by the vanilla shell via window.WebOSSurface.icon).
// Returns a self-contained <svg> drawn with currentColor so CSS can theme it.
function iconSvg(name: string, size?: number): string {
  const d = ICONS[name];
  const s = size || 18;
  if (!d) return `<svg width="${s}" height="${s}" viewBox="0 0 24 24"></svg>`;
  return `<svg width="${s}" height="${s}" viewBox="0 0 24 24" fill="none" stroke="currentColor" ` +
    `stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="${d}"/></svg>`;
}
function IconView({ name, size, color }: any) {
  const d = ICONS[name];
  const s = size || 18;
  if (!d) return <span className="w-icon-missing" style={{ width: s, height: s }} />;
  return (
    <svg className="w-icon" width={s} height={s} viewBox="0 0 24 24" fill="none"
      stroke={color || "currentColor"} strokeWidth={2} strokeLinecap="round" strokeLinejoin="round">
      <path d={d} />
    </svg>
  );
}

// ── hand-rolled SVG charts ──────────────────────────────────────────────────────
const CHART_COLORS = ["#5b8cff", "#3ecf8e", "#febc2e", "#c77dff", "#2bd4d4", "#ff8b85"];
function niceMax(m: number) { if (m <= 0) return 1; const p = Math.pow(10, Math.floor(Math.log10(m))); return Math.ceil(m / p) * p; }
// Shared shadcn-styled status rows for the data widgets while a source loads
// or errors (scoped Tailwind tokens; matches the dark glass theme).
function Loading() {
  return <div className="flex items-center gap-2 py-2 text-sm text-muted-foreground"><Spin /> loading…</div>;
}
function Spin() {
  return <span className="inline-block h-3.5 w-3.5 animate-spin rounded-full border-2 border-muted-foreground/40 border-t-primary" />;
}
function ErrorRow({ msg }: { msg: string }) {
  return <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">{msg}</div>;
}
function EmptyRow({ msg }: { msg: string }) {
  return <div className="py-2 text-sm text-muted-foreground">{msg}</div>;
}

function ChartView({ source, items, type = "bar", x, y, agg, height, refresh }: any) {
  const st = useSource(source, refresh);
  if (st.loading) return <Loading />;
  if (st.error) return <ErrorRow msg={st.error} />;
  const rows = itemsOf(st.payload, items);
  let labels: string[];
  let series: number[][];
  // Count mode: group rows by the x category and plot the per-group count.
  // Used for categorical data (e.g. issues by status) with no numeric field —
  // also the default when no y is given but an x is.
  if (agg === "count" || (!y && x)) {
    const groups: Record<string, number> = {};
    rows.forEach((r: any) => { const k = fmt(getPath(r, x)) || "—"; groups[k] = (groups[k] || 0) + 1; });
    labels = Object.keys(groups);
    series = [labels.map((k) => groups[k])];
  } else {
    const ykeys: string[] = Array.isArray(y) ? y : [y || "value"];
    labels = rows.map((r: any) => fmt(x ? getPath(r, x) : ""));
    series = ykeys.map((k) => rows.map((r: any) => num(getPath(r, k))));
  }
  const W = 520, H = height || 180, pad = 24;
  const maxV = niceMax(Math.max(1, ...series.flat()));
  const iw = W - pad * 2, ih = H - pad * 2;

  if (type === "donut") {
    const vals = series[0] || [];
    const total = vals.reduce((a: number, b: number) => a + b, 0) || 1;
    let acc = 0; const cx = H / 2, cy = H / 2, r = H / 2 - 8, rin = r * 0.58;
    return (
      <svg className="w-chart-svg" viewBox={`0 0 ${H + 180} ${H}`} width="100%" height={H}>
        {vals.map((v: number, i: number) => {
          const a0 = (acc / total) * 2 * Math.PI - Math.PI / 2; acc += v;
          const a1 = (acc / total) * 2 * Math.PI - Math.PI / 2;
          const p = (ang: number, rr: number) => [cx + rr * Math.cos(ang), cy + rr * Math.sin(ang)];
          const [x0, y0] = p(a0, r), [x1, y1] = p(a1, r), [x2, y2] = p(a1, rin), [x3, y3] = p(a0, rin);
          const large = a1 - a0 > Math.PI ? 1 : 0;
          return <path key={i} fill={CHART_COLORS[i % CHART_COLORS.length]}
            d={`M${x0} ${y0}A${r} ${r} 0 ${large} 1 ${x1} ${y1}L${x2} ${y2}A${rin} ${rin} 0 ${large} 0 ${x3} ${y3}Z`} />;
        })}
        {vals.map((_: number, i: number) => (
          <g key={"l" + i} transform={`translate(${H + 14}, ${20 + i * 20})`}>
            <rect width="11" height="11" rx="3" fill={CHART_COLORS[i % CHART_COLORS.length]} />
            <text x="18" y="10" className="w-chart-leg">{labels[i] || ykeys[0] + " " + i}</text>
          </g>
        ))}
      </svg>
    );
  }

  const xstep = iw / Math.max(1, labels.length);
  return (
    <svg className="w-chart-svg" viewBox={`0 0 ${W} ${H}`} width="100%" height={H} preserveAspectRatio="none">
      {[0, 0.5, 1].map((g, i) => (
        <line key={i} className="w-chart-grid" x1={pad} x2={W - pad} y1={pad + ih * g} y2={pad + ih * g} />
      ))}
      {series.map((vals, si) => {
        const col = CHART_COLORS[si % CHART_COLORS.length];
        if (type === "bar") {
          const bw = (xstep * 0.7) / series.length;
          return vals.map((v, i) => {
            const h = (v / maxV) * ih;
            return <rect key={si + "-" + i} fill={col} rx="2"
              x={pad + i * xstep + xstep * 0.15 + si * bw} y={pad + ih - h} width={bw} height={Math.max(0, h)} />;
          });
        }
        const pts = vals.map((v, i) => [pad + i * xstep + xstep / 2, pad + ih - (v / maxV) * ih]);
        const line = pts.map((p, i) => (i ? "L" : "M") + p[0] + " " + p[1]).join(" ");
        return (
          <g key={si}>
            {type === "area" && pts.length > 1 && (
              <path fill={col} opacity="0.18"
                d={`${line}L${pts[pts.length - 1][0]} ${pad + ih}L${pts[0][0]} ${pad + ih}Z`} />
            )}
            <path d={line} fill="none" stroke={col} strokeWidth="2.5" strokeLinejoin="round" />
            {pts.map((p, i) => <circle key={i} cx={p[0]} cy={p[1]} r="2.5" fill={col} />)}
          </g>
        );
      })}
    </svg>
  );
}

// ── click-through (Stage 2 interactivity) ───────────────────────────────────────
// A row/card click writes the clicked record into state via the json-render
// action pipeline (the built-in "setState" action), so a Detail bound to that
// path with { "$state": "/selected" } updates — the master→detail pattern.
//
// We route through useActions().execute (NOT a raw store write) so the write
// goes through the SAME governed/observable action machinery as every other
// action, and a Table/Board with no selectInto/onSelect stays inert.
//
//  • selectInto: a JSON-Pointer state path (e.g. "/selected"). Clicking a row/
//    card runs { action:"setState", params:{ statePath:selectInto, value:row } }.
//  • onSelect: an explicit ActionBinding ({action, params}). The clicked record
//    is offered to it under params.value when params is omitted, so e.g.
//    { action:"open", params:{ id:"detail-win" } } still fires on click.
// Both are optional; when neither is set the widget is read-only (legacy).
function useRowSelect(selectInto?: string, onSelect?: any) {
  const { execute } = useActions();
  const interactive = !!(selectInto || onSelect);
  const select = (row: any) => {
    if (selectInto) {
      void execute({ action: "setState", params: { statePath: selectInto, value: row } });
    }
    if (onSelect && onSelect.action) {
      const params = onSelect.params != null ? onSelect.params : { value: row };
      void execute({ ...onSelect, params });
    }
  };
  return { interactive, select };
}

// ── data + display views ────────────────────────────────────────────────────────
// Connector-bound data table — fetches via useSource and renders a shadcn-styled
// table (bordered, sticky-feel header, zebra hover) using the curated columns.
// When selectInto/onSelect is set, rows become clickable (master→detail).
function TableView({ source, items, columns, refresh, selectInto, onSelect }: any) {
  const st = useSource(source, refresh);
  const { interactive, select } = useRowSelect(selectInto, onSelect);
  if (st.loading) return <Loading />;
  if (st.error) return <ErrorRow msg={st.error} />;
  const rows = itemsOf(st.payload, items);
  return (
    <div className="overflow-hidden rounded-lg border border-border">
      <table className="w-full text-sm">
        <thead>
          <tr className="border-b border-border bg-muted/50">
            {(columns || []).map((c: any, i: number) => (
              <th key={i} className="px-3 py-2 text-left font-medium text-muted-foreground">{c.header || c.path}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((it: any, ri: number) => (
            <tr key={ri}
              className={"border-b border-border/60 last:border-0 hover:bg-muted/40 " + (interactive ? "cursor-pointer" : "")}
              onClick={interactive ? () => select(it) : undefined}>
              {(columns || []).map((c: any, ci: number) => (
                <td key={ci} className="px-3 py-2 align-top">{fmt(getPath(it, c.path))}</td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
      {rows.length ? null : <EmptyRow msg="no rows" />}
    </div>
  );
}
// Detail / key-value record view — shadcn Card-styled labeled rows. Three ways
// to supply the record (checked in order):
//   • record — a pre-resolved object, typically bound with { "$state":
//     "/selected" }; this is the master→detail target for a clickable Table/Board.
//   • source — a conn.call binding (fetches one record, like the data widgets).
//   • neither — fields fall back to their literal f.value.
// When `record`/`source` yields nothing yet (e.g. before the first row click),
// an empty-state hint renders instead of blank rows.
function KeyValueView({ source, items, fields, refresh, record, empty }: any) {
  const st = source ? useSource(source, refresh) : { payload: undefined };
  if (st.loading) return <Loading />;
  if (st.error) return <ErrorRow msg={st.error} />;
  const obj = record != null ? record : source ? (itemsOf(st.payload, items)[0] || {}) : {};
  const hasRecord = obj && typeof obj === "object" && Object.keys(obj).length > 0;
  // Show the empty hint when there's no record to show yet. `empty` is only set
  // for the master→detail pattern (record bound to {$state:"/selected"}), where
  // the binding resolves to null/undefined before the first row click — so an
  // `empty` prop means "render the hint until something is selected", regardless
  // of whether the null came through as a present-but-null prop or a dropped one.
  if (!hasRecord && (empty != null || source)) {
    return <EmptyRow msg={empty || "nothing selected"} />;
  }
  return (
    <div className="rounded-lg border border-border bg-card p-3">
      {(fields || []).map((f: any, i: number) => (
        <div key={i} className="flex items-baseline justify-between gap-4 border-b border-border/50 py-1.5 text-sm last:border-0">
          <span className="text-muted-foreground">{f.label || f.path}</span>
          <span className="text-right font-medium">{fmt(f.value != null ? f.value : getPath(obj, f.path))}</span>
        </div>
      ))}
    </div>
  );
}
// KPI metric card — shadcn Card-styled big stat with optional delta + icon.
function MetricView({ label, value, unit, delta, icon }: any) {
  const d = delta == null ? null : Number(delta);
  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <div className="flex items-center gap-1.5 text-sm text-muted-foreground">
        {icon ? <IconView name={icon} size={16} /> : null}<span>{label}</span>
      </div>
      <div className="mt-1 text-2xl font-semibold tracking-tight">
        {fmt(value)}{unit ? <span className="ml-1 text-base font-normal text-muted-foreground">{unit}</span> : null}
      </div>
      {d != null && (
        <div className={"mt-1 text-xs font-medium " + (d >= 0 ? "text-[var(--ok,#3ecf8e)]" : "text-destructive")}>
          {d >= 0 ? "▲" : "▼"} {Math.abs(d)}%
        </div>
      )}
    </div>
  );
}
// Kanban board: fetch the source, bucket rows by the groupBy dot-path into
// columns (first-seen order, but if every key is a number the columns sort
// ascending so e.g. priority 0..4 reads naturally), then render a horizontal
// row of columns — each a header (group value + count) over a vertical stack of
// cards. Each card shows cardTitle in bold plus the configured cardFields.
function BoardView({ source, items, groupBy, cardTitle, cardFields, refresh, selectInto, onSelect }: any) {
  const st = useSource(source, refresh);
  const { interactive, select } = useRowSelect(selectInto, onSelect);
  if (st.loading) return <Loading />;
  if (st.error) return <ErrorRow msg={st.error} />;
  const rows = itemsOf(st.payload, items);
  const order: string[] = [];
  const groups: Record<string, any[]> = {};
  rows.forEach((r: any) => {
    const k = fmt(getPath(r, groupBy)) || "—";
    if (!groups[k]) { groups[k] = []; order.push(k); }
    groups[k].push(r);
  });
  // Natural sort when every group key parses as a finite number; else keep
  // first-seen order (preserves a connector's own status ordering).
  if (order.length && order.every((k) => isFinite(Number(k)) && k !== "")) {
    order.sort((a, b) => Number(a) - Number(b));
  }
  const fields = cardFields || [];
  return (
    <div className="flex gap-3 overflow-x-auto pb-1">
      {order.map((g) => (
        <div className="flex w-56 shrink-0 flex-col gap-2" key={g}>
          <div className="flex items-center justify-between px-1 text-sm font-medium text-muted-foreground">
            <span>{g}</span>
            <span className="rounded-full bg-muted px-2 py-0.5 text-xs">{groups[g].length}</span>
          </div>
          {groups[g].map((it: any, ci: number) => (
            <div
              className={"rounded-lg border border-border bg-card p-3 shadow-sm " + (interactive ? "cursor-pointer transition-colors hover:border-primary/60 hover:bg-muted/30" : "")}
              key={ci}
              onClick={interactive ? () => select(it) : undefined}>
              <div className="text-sm font-semibold">{fmt(getPath(it, cardTitle))}</div>
              {fields.map((f: any, fi: number) => (
                <div key={fi} className="mt-1 flex items-baseline justify-between gap-2 text-xs">
                  <span className="text-muted-foreground">{f.label || f.path}</span>
                  <span className="text-right">{fmt(getPath(it, f.path))}</span>
                </div>
              ))}
            </div>
          ))}
        </div>
      ))}
      {order.length ? null : <EmptyRow msg="no rows" />}
    </div>
  );
}

// Badge/Progress/Button tone → shadcn token classes.
const BADGE_TONE: Record<string, string> = {
  ok: "border-transparent bg-[var(--ok,#3ecf8e)]/15 text-[var(--ok,#3ecf8e)]",
  warn: "border-transparent bg-amber-400/15 text-amber-300",
  deny: "border-transparent bg-destructive/15 text-destructive",
  info: "border-transparent bg-primary/15 text-primary",
};
const PROGRESS_TONE: Record<string, string> = {
  accent: "bg-primary",
  ok: "bg-[var(--ok,#3ecf8e)]",
  deny: "bg-destructive",
};

// ── custom action handlers (Stage 2) ─────────────────────────────────────────────
// Surfaces can bind element.on.{press|select|change|…} to these in addition to
// the runtime built-ins (setState/pushState/removeState/validateForm, auto-
// handled by ActionProvider) and `navigate` (wired on JSONUIProvider below).
//
//  • open  — open another surface/app by id in the shell (window.webos.open).
//            params: { id: string }  (also accepts { path } as an alias).
//  • call  — invoke ONE governed bus capability via the shell's window.invoke.
//            params: { capability: string, args?: object }. The shell socket's
//            consent gate still applies (an ai-principal call prompts the
//            operator; a human click is allowed). Returns the unwrapped payload
//            so onSuccess/onError can branch. This is the SAME bus the data
//            widgets read through — capability is constrained server-side to the
//            ALLOWED_CAPS safelist (no policy/creds/connector.* writes); the
//            surface validator also rejects any disallowed action name.
const ACTION_HANDLERS: Record<string, (params: Record<string, any>) => Promise<unknown> | unknown> = {
  open: (params) => {
    const id = (params && (params.id ?? params.path)) as string | undefined;
    const w = window as any;
    if (id && w.webos?.open) w.webos.open(String(id));
  },
  call: async (params) => {
    const cap = params && (params.capability as string | undefined);
    if (!cap) return { ok: false, error: "call: capability required" };
    const msg = await invoke(cap, (params && params.args) || {});
    return unwrap(msg);
  },
};

// ── registry (catalog + React implementations) ──────────────────────────────────
//
// Two families merged into ONE registry:
//  • shadcnComponents — the prebuilt @json-render/shadcn library (Separator,
//    Tabs, Accordion, Dialog, Alert, Select, Switch, Slider, …). Spread first…
//  • webOS widgets/primitives — spread AFTER so OUR connector-bound data widgets
//    and caps.rs-compatible primitives win on name collision (Table/Card/Badge/
//    Progress/Stack/Grid/Heading/Text/Input/Toggle/Button). These now render
//    shadcn-token Tailwind markup so they're polished too.
const { registry } = defineRegistry(catalog, {
  components: {
    ...shadcnComponents,

    Stack: ({ props, children }: any) => {
      const gap = { none: "gap-0", sm: "gap-2", md: "gap-3", lg: "gap-4", xl: "gap-6" }[(props?.gap as string) || "md"] || "gap-3";
      return <div className={"flex flex-col " + gap}>{children}</div>;
    },
    Row: ({ props, children }: any) => {
      const gap = { none: "gap-0", sm: "gap-2", md: "gap-3", lg: "gap-4", xl: "gap-6" }[(props?.gap as string) || "md"] || "gap-3";
      return <div className={"flex flex-row flex-wrap items-center " + gap}>{children}</div>;
    },
    Grid: ({ props, children }: any) => <div className="grid gap-3" style={props.cols ? { gridTemplateColumns: `repeat(${props.cols},minmax(0,1fr))` } : undefined}>{children}</div>,
    Card: ({ props, children }: any) => (
      <div className="rounded-lg border border-border bg-card p-4 shadow-sm">
        {props.title ? <h3 className="mb-2 text-base font-semibold">{props.title}</h3> : null}
        <div className="flex flex-col gap-3">{children}</div>
      </div>
    ),
    Heading: ({ props }: any) => <h2 className="text-lg font-semibold tracking-tight">{props.value}</h2>,
    Text: ({ props }: any) => <p className="text-sm text-foreground">{props.value}</p>,
    Metric: ({ props }: any) => <MetricView {...props} />,
    Badge: ({ props }: any) => (
      <span className={"inline-flex items-center rounded-md border px-2 py-0.5 text-xs font-medium " + (BADGE_TONE[props.tone as string] || BADGE_TONE.info)}>{props.label}</span>
    ),
    Progress: ({ props }: any) => (
      <div className="h-2 w-full overflow-hidden rounded-full bg-muted">
        <div className={"h-full rounded-full transition-all " + (PROGRESS_TONE[props.tone as string] || PROGRESS_TONE.accent)} style={{ width: Math.max(0, Math.min(100, num(props.value))) + "%" }} />
      </div>
    ),
    KeyValue: ({ props }: any) => <KeyValueView {...props} />,
    Icon: ({ props }: any) => <IconView {...props} />,
    Table: ({ props }: any) => <TableView {...props} />,
    Detail: ({ props }: any) => <KeyValueView {...props} />,
    Chart: ({ props }: any) => <ChartView {...props} />,
    Board: ({ props }: any) => <BoardView {...props} />,
    Sparkline: ({ props }: any) => <ChartView {...props} type="line" height={props.height || 48} />,
    Input: ({ props, bindings }: any) => {
      const [v, setV] = useBoundProp(props.value, bindings?.value);
      return (
        <label className="flex flex-col gap-1.5">
          {props.label ? <span className="text-sm font-medium text-muted-foreground">{props.label}</span> : null}
          <input
            className="h-9 w-full rounded-md border border-input bg-transparent px-3 py-1 text-sm shadow-sm outline-none transition-colors focus-visible:border-ring focus-visible:ring-2 focus-visible:ring-ring/30"
            value={(v as any) ?? ""} placeholder={props.placeholder || ""} onChange={(e) => setV(e.target.value)} />
        </label>
      );
    },
    Toggle: ({ props, bindings }: any) => {
      const [v, setV] = useBoundProp(props.pressed, bindings?.pressed);
      return (
        <button type="button" onClick={() => setV(!v)}
          className={"inline-flex items-center gap-2 rounded-md px-3 py-1.5 text-sm font-medium transition-colors " + (v ? "bg-primary text-primary-foreground" : "bg-muted text-muted-foreground hover:bg-muted/70")}>
          <span className={"inline-block h-2 w-2 rounded-full " + (v ? "bg-primary-foreground" : "bg-muted-foreground/50")} />
          {props.label}
        </button>
      );
    },
    Button: ({ props, emit }: any) => {
      const tone = props.tone === "ghost"
        ? "bg-transparent hover:bg-muted text-foreground"
        : props.tone === "danger"
        ? "bg-destructive text-destructive-foreground hover:bg-destructive/90"
        : "bg-primary text-primary-foreground hover:bg-primary/90";
      return (
        <button type="button" onClick={() => emit("press")}
          className={"inline-flex h-9 items-center justify-center gap-2 rounded-md px-4 text-sm font-medium shadow-sm transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40 " + tone}>
          {props.label}
        </button>
      );
    },
  },
  // The catalog declares `open` + `call`, so defineRegistry requires matching
  // action handlers. They delegate to the SAME ACTION_HANDLERS used by the
  // JSONUIProvider `handlers` prop (defined below) — one implementation, two
  // entry points (the provider path is what actually fires at runtime; these
  // keep defineRegistry's typed-action surface in sync with the catalog).
  actions: {
    open: async (params: any) => { await ACTION_HANDLERS.open(params || {}); },
    call: async (params: any) => { await ACTION_HANDLERS.call(params || {}); },
  },
});

function Unknown() { return <div className="w-unknown">[unsupported element]</div>; }

// ── mount API ───────────────────────────────────────────────────────────────────
const roots = new WeakMap<Element, Root>();

// Harden an AI-authored flat spec before rendering. Both helpers from
// @json-render/core are pure + synchronous (no catalog arg, no unbounded
// recursion — the only walk, orphan-checking, is Set-guarded and off by
// default), so calling them ONCE here per mount is safe and cannot loop.
// autoFixSpec relocates misplaced visible/on/repeat/watch keys; validateSpec
// then confirms root + child references resolve, else we show a clear notice
// instead of rendering nothing.
function harden(raw: any): any {
  if (!raw || !raw.root || !raw.elements) return raw;
  let spec = raw;
  try {
    const { spec: fixed } = autoFixSpec(raw);
    if (fixed) spec = { ...raw, ...fixed };
  } catch { /* fall back to the raw spec */ }
  try {
    const { valid, issues } = validateSpec(spec);
    if (!valid) {
      const msg = (issues || []).map((i: any) => i.message).join("; ") || "invalid spec";
      return {
        id: spec.id || "invalid",
        title: spec.title || "Surface",
        root: "err",
        elements: { err: { type: "Text", props: { value: "Could not render this surface: " + msg } } },
      };
    }
  } catch { /* if validation itself throws, render the spec as-is */ }
  return spec;
}

function mount(el: Element, raw: any) {
  // Tag the container so the scoped Tailwind/shadcn styles (web/surface.css,
  // all rules under `.webos-surface`) apply here and nowhere else in the shell.
  el.classList.add("webos-surface");
  let root = roots.get(el);
  if (!root) { root = createRoot(el); roots.set(el, root); }
  const spec = harden(raw);
  root.render(
    <JSONUIProvider
      registry={registry}
      initialState={(spec && spec.state) || {}}
      handlers={ACTION_HANDLERS}
      navigate={(p: string) => (window as any).webos?.open?.(p)}
    >
      <Renderer spec={spec} registry={registry} fallback={Unknown} />
    </JSONUIProvider>
  );
}
function unmount(el: Element) {
  const r = roots.get(el);
  if (r) { r.unmount(); roots.delete(el); }
  el.classList.remove("webos-surface");
}

// ── streaming mount (client hook, ready for a future streaming server) ──────────
//
// There is NO server streaming endpoint today — kerneld builds complete specs —
// so this is the *client* half only: it feeds @json-render/core's
// createSpecStreamCompiler with SpecStream chunks (JSONL patch lines) as they
// arrive and re-renders the progressively-built spec, ready to be wired to a
// streaming source later. The returned handle is intentionally minimal:
//
//   const s = window.WebOSSurface.mountStream(el);
//   s.push('{"op":"add","path":"/root","value":"stack"}\n');  // partial paints
//   s.push('{"op":"add","path":"/elements/stack",...}\n');
//   s.done();                                                   // final harden+paint
//
// push() repaints only when the chunk produced new patches (the compiler tells
// us via newPatches), so a chunk that doesn't complete a line is a no-op render.
// We render the *raw* partial through the same mount(), but skip harden() on
// partials (an incomplete spec would always fail validateSpec); done() does a
// final mount() so the completed spec gets the full harden() pass.
function mountStream(el: Element) {
  const compiler = createSpecStreamCompiler<any>();
  let lastJson = "";
  const paint = (spec: any, hardened: boolean) => {
    el.classList.add("webos-surface"); // scope the shadcn/Tailwind styles here
    let root = roots.get(el);
    if (!root) { root = createRoot(el); roots.set(el, root); }
    const s = hardened ? harden(spec) : spec;
    // A partial spec may not yet have a resolvable root; render nothing rather
    // than throwing until at least root + that element exist.
    if (!s || !s.root || !s.elements || !s.elements[s.root]) return;
    root.render(
      <JSONUIProvider
        registry={registry}
        initialState={(s && s.state) || {}}
        handlers={ACTION_HANDLERS}
        navigate={(p: string) => (window as any).webos?.open?.(p)}
      >
        <Renderer spec={s} registry={registry} fallback={Unknown} />
      </JSONUIProvider>
    );
  };
  return {
    push(chunk: string) {
      let res;
      try {
        res = compiler.push(chunk);
      } catch {
        return; // tolerate a partial/garbled line; the next chunk may complete it
      }
      if (res && res.newPatches && res.newPatches.length > 0) {
        const j = JSON.stringify(res.result);
        if (j !== lastJson) { lastJson = j; paint(res.result, false); }
      }
    },
    done() {
      const spec = compiler.getResult();
      paint(spec, true); // final paint goes through harden() (autoFix + validate)
      return spec;
    },
    reset() { compiler.reset(); lastJson = ""; },
  };
}

export { mount, unmount, mountStream, iconSvg as icon };
