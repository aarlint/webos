// Generate the AI component manifest from the SAME catalog the renderer uses.
//
// Reads ui/src/catalog.ts (the single source of truth shared with
// surface.tsx), calls @json-render/core's catalog.prompt() to produce the
// full component vocabulary + binding/repeat/conditional/action syntax, and
// writes it to web/catalog-prompt.txt. kerneld loads that file at boot and
// injects it into both the chat agent and ai.compose system prompts, replacing
// the previously hand-written component list (which drifted from the catalog).
//
// catalog.ts is TypeScript and imports @json-render/* + zod, so we bundle it
// with esbuild (already a Vite dependency) into a temporary ESM module, import
// it, then clean up. Run from package.json after `vite build`.
import { build } from "esbuild";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const entry = resolve(here, "src/catalog.ts");
const outFile = resolve(here, "../web/catalog-prompt.txt");

const tmp = mkdtempSync(join(tmpdir(), "webos-catalog-"));
const bundlePath = join(tmp, "catalog.mjs");

try {
  await build({
    entryPoints: [entry],
    bundle: true,
    format: "esm",
    platform: "node",
    target: "node18",
    outfile: bundlePath,
    logLevel: "warning",
  });

  const mod = await import(pathToFileURL(bundlePath).href);
  const catalog = mod.catalog;
  if (!catalog || typeof catalog.prompt !== "function") {
    throw new Error("catalog.ts did not export a catalog with a prompt() method");
  }

  // standalone mode: the model should emit ONLY the JSON spec (no prose) — the
  // chat agent wraps this in a ui.surface tool call, and ai.compose expects a
  // bare spec object. The intro/rules in the Rust prompts add the tool-usage
  // and connector-binding guidance around this catalog description.
  const text = catalog.prompt({ mode: "standalone" });
  if (!text || !text.trim()) {
    throw new Error("catalog.prompt() returned empty text");
  }

  writeFileSync(outFile, text, "utf8");
  console.log(`gen-prompt: wrote ${text.length} chars to ${outFile}`);
} finally {
  rmSync(tmp, { recursive: true, force: true });
}
