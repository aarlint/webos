// webOS surface CATALOG — the single source of truth for the component
// vocabulary, their prop schemas, and human descriptions. Kept JSX-free so it
// can be imported by BOTH the React renderer (ui/src/surface.tsx, which adds the
// component implementations via defineRegistry) AND the prompt generator
// (ui/gen-prompt.mjs, which calls catalog.prompt() to emit web/catalog-prompt.txt
// for the AI). Adding/changing a component here updates the renderer's guard-rail
// AND the AI's manifest from one place — no hand-written drift.
//
// STAGE 1 (shadcn): the catalog is now a MERGE of two families —
//
//  1. webOS data/display widgets (Table, Chart, Board, Metric, Detail, KeyValue,
//     Sparkline, Icon) + the layout/typography/form primitives that the backend
//     caps (caps.rs ui.table/ui.chart/ui.board) and saved-to-dock surfaces
//     already emit (Stack/Row/Grid/Card/Heading/Text/Badge/Progress/Input/
//     Toggle/Button). These KEEP their established prop names (Heading.value,
//     Text.value, Badge.label/tone, Button.label/tone, Stack {}, …) so existing
//     specs and the Rust caps keep validating — but their React implementations
//     in surface.tsx now render real shadcn/ui-styled markup.
//
//  2. the rest of the @json-render/shadcn presentational library (Separator,
//     Tabs, Accordion, Collapsible, Dialog, Drawer, Tooltip, Popover,
//     DropdownMenu, Image, Avatar, Alert, Skeleton, Spinner, Link, Textarea,
//     Select, Checkbox, Radio, Switch, Slider, ToggleGroup, ButtonGroup,
//     Carousel, Pagination), pulled in VERBATIM from shadcnComponentDefinitions
//     with their own schemas, so the AI can compose with the full polished set.
//
// On name collision (shadcn also ships Card/Badge/Progress/Stack/Grid/Heading/
// Text/Input/Toggle/Button/Table) OUR definition wins, so the connector-bound
// data widgets and the caps.rs-emitted prop shapes are preserved.
import { defineCatalog } from "@json-render/core";
import { schema } from "@json-render/react/schema";
import { shadcnComponentDefinitions as sc } from "@json-render/shadcn/catalog";
import { z } from "zod";

// The shadcn presentational components we adopt as-is (no webOS collision). Their
// schemas/descriptions come straight from the package so the prompt advertises
// the real shadcn prop surface (variants, validation, $bindState, …).
const SHADCN_ADDED = [
  "Separator", "Tabs", "Accordion", "Collapsible", "Dialog", "Drawer",
  "Tooltip", "Popover", "DropdownMenu", "Image", "Avatar", "Alert",
  "Skeleton", "Spinner", "Link", "Textarea", "Select", "Checkbox",
  "Radio", "Switch", "Slider", "ToggleGroup", "ButtonGroup", "Carousel",
  "Pagination",
] as const;

const shadcnAdded = Object.fromEntries(
  SHADCN_ADDED.map((name) => {
    const def = (sc as Record<string, any>)[name];
    if (!def) throw new Error(`@json-render/shadcn/catalog is missing '${name}'`);
    return [name, def];
  }),
);

// ── catalog (guard-rail + AI manifest source) ───────────────────────────────────
export const catalog = defineCatalog(schema, {
  components: {
    // ── webOS primitives + data widgets (names/props kept for caps.rs + saved
    //    surfaces; implementations in surface.tsx render shadcn-styled markup) ──
    Stack: { props: z.object({ gap: z.enum(["none", "sm", "md", "lg", "xl"]).optional() }).passthrough(), description: "Vertical stack of children." },
    Row: { props: z.object({ gap: z.enum(["none", "sm", "md", "lg", "xl"]).optional() }).passthrough(), description: "Horizontal row of children." },
    Grid: { props: z.object({ cols: z.number().optional() }), description: "Responsive grid of children." },
    Card: { props: z.object({ title: z.string().optional() }), description: "Titled container (shadcn Card)." },
    Heading: { props: z.object({ value: z.string() }), description: "Section heading." },
    Text: { props: z.object({ value: z.string() }), description: "Paragraph; value may be a {$state}/{$template} binding." },
    Metric: { props: z.object({ label: z.string(), value: z.any(), unit: z.string().optional(), delta: z.number().optional(), icon: z.string().optional() }), description: "Big stat card with optional delta% and icon." },
    Badge: { props: z.object({ label: z.string(), tone: z.enum(["ok", "warn", "deny", "info"]).optional() }), description: "Small status pill (shadcn Badge)." },
    Progress: { props: z.object({ value: z.number(), tone: z.enum(["accent", "ok", "deny"]).optional() }), description: "0-100 progress bar (shadcn Progress)." },
    KeyValue: { props: z.object({ source: z.any().optional(), items: z.string().optional(), refresh: z.number().optional(), record: z.any().optional(), empty: z.string().optional(), fields: z.array(z.object({ label: z.string(), path: z.string().optional(), value: z.any().optional() })) }), description: "Labeled fields for one record. Bind `record` to { \"$state\": \"/selected\" } to show the row a Table/Board selected (master→detail); or set `source` to fetch one record; `empty` is the hint shown before a selection." },
    Icon: { props: z.object({ name: z.string(), size: z.number().optional(), color: z.string().optional() }), description: "Inline icon by name." },
    Table: { props: z.object({ source: z.any(), items: z.string().optional(), refresh: z.number().optional(), selectInto: z.string().optional(), onSelect: z.any().optional(), columns: z.array(z.object({ header: z.string(), path: z.string() })) }), description: "Data table bound to a conn.call source (shadcn-styled); refresh=seconds for live polling. Set `selectInto` to a state path (e.g. \"/selected\") to make ROWS CLICKABLE — a click writes the whole row object there (drive a Detail bound to { \"$state\": that path }); or set `onSelect` to an action binding ({action,params}) fired on row click." },
    Detail: { props: z.object({ source: z.any().optional(), items: z.string().optional(), refresh: z.number().optional(), record: z.any().optional(), empty: z.string().optional(), fields: z.array(z.object({ label: z.string(), path: z.string() })) }), description: "Detail view of one record. Bind `record` to { \"$state\": \"/selected\" } for the master→detail side panel (the row a Table/Board selected), or set `source` to fetch one; `empty` is the placeholder shown before anything is selected." },
    Chart: { props: z.object({ source: z.any(), items: z.string().optional(), type: z.enum(["bar", "line", "area", "donut"]).optional(), x: z.string().optional(), y: z.union([z.string(), z.array(z.string())]).optional(), agg: z.string().optional(), refresh: z.number().optional(), height: z.number().optional() }), description: "SVG chart bound to a conn.call source; agg:'count' groups by x and counts; refresh=seconds for live polling." },
    Board: { props: z.object({ source: z.any(), items: z.string().optional(), groupBy: z.string(), cardTitle: z.string(), cardFields: z.array(z.object({ label: z.string(), path: z.string() })).optional(), refresh: z.number().optional(), selectInto: z.string().optional(), onSelect: z.any().optional() }), description: "Kanban board bound to a conn.call source: shadcn-card columns grouped by the groupBy dot-path (e.g. state.name); cardTitle is the title dot-path; refresh=seconds for live polling. Set `selectInto` to a state path (e.g. \"/selected\") to make CARDS CLICKABLE — a click writes the card's row object there for a master→detail panel; or set `onSelect` to an action binding fired on card click." },
    Sparkline: { props: z.object({ source: z.any(), items: z.string().optional(), y: z.string().optional(), refresh: z.number().optional(), height: z.number().optional() }), description: "Compact inline trend line." },
    Input: { props: z.object({ label: z.string().optional(), placeholder: z.string().optional(), value: z.any().optional() }), description: "Text input; value can be a {$bindState} two-way binding." },
    Toggle: { props: z.object({ label: z.string(), pressed: z.any().optional() }), description: "On/off switch; pressed can be {$bindState}." },
    Button: { props: z.object({ label: z.string(), tone: z.enum(["accent", "ghost", "danger"]).optional() }), description: "Button; wire element.on.press to a built-in action." },

    // ── shadcn presentational components (verbatim from the package) ──
    ...shadcnAdded,
  },
  // Custom actions (in addition to the runtime built-ins setState/pushState/
  // removeState/validateForm, which need no catalog entry). These two are
  // implemented by ACTION_HANDLERS in surface.tsx and wired on JSONUIProvider;
  // declaring them here advertises them to the AI (catalog.prompt → the
  // "AVAILABLE ACTIONS" block) so it knows the open/call vocabulary. Both are
  // governed: `call` is constrained server-side to the read-only capability
  // safelist, and the surface validator rejects any other action name.
  actions: {
    open: {
      params: z.object({ id: z.string() }),
      description: "Open another surface/app window by id (e.g. a saved widget or a sibling surface). Params: { id: string }.",
    },
    call: {
      params: z.object({ capability: z.string(), args: z.record(z.string(), z.any()).optional() }),
      description: "Invoke ONE governed, read-only bus capability (e.g. conn.call to fetch related data on click). Params: { capability: string, args?: object }. Only read capabilities are permitted; never policy/credential/connector-management verbs.",
    },
  },
});

// Component names, exported so callers (and tests) can assert the vocabulary
// without reaching into the catalog internals.
export const COMPONENT_NAMES: string[] = catalog.componentNames;
