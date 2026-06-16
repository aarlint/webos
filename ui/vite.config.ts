import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { defineConfig, type Plugin } from "vite";

// Vite 5 lib mode always names the single bundled CSS asset "style.css", which
// would COLLIDE with the vanilla shell's own web/style.css. This plugin renames
// that emitted asset to surface.css before it is written, so the shell stylesheet
// is never touched. (cssFileName is only honored by Vite 6+, hence this guard.)
function renameSurfaceCss(): Plugin {
  return {
    name: "webos-rename-surface-css",
    enforce: "post",
    generateBundle(_options, bundle) {
      for (const [key, asset] of Object.entries(bundle)) {
        if (asset.type === "asset" && asset.fileName === "style.css") {
          asset.fileName = "surface.css";
          delete bundle[key];
          bundle["surface.css"] = asset;
        }
      }
    },
  };
}

// Build a single IIFE bundle (React + json-render + shadcn inlined) that the
// vanilla webOS shell loads as a plain <script>, exposing window.WebOSSurface.
// surface.tsx imports ./surface.css, so @tailwindcss/vite compiles the scoped
// Tailwind/shadcn styles and Vite emits them next to the JS as surface.css —
// which index.html loads alongside the shell's style.css.
export default defineConfig({
  plugins: [react(), tailwindcss(), renameSurfaceCss()],
  define: { "process.env.NODE_ENV": '"production"' },
  build: {
    outDir: "../web",
    emptyOutDir: false,
    cssCodeSplit: false,
    lib: {
      entry: "src/surface.tsx",
      formats: ["iife"],
      name: "WebOSSurface",
      fileName: () => "surface.js",
    },
    rollupOptions: { output: { inlineDynamicImports: true } },
  },
});
