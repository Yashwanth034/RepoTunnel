import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  build: {
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (!id.includes("node_modules")) return undefined;
          if (id.includes("/react/") || id.includes("/react-dom/")) {
            return "react";
          }
          // Preserve the editor's dynamic language imports instead of folding every
          // parser into the startup bundle. Only the CodeMirror core is shared.
          if (
            id.includes("/@codemirror/lang-") ||
            id.includes("/@codemirror/legacy-modes/") ||
            (id.includes("/@lezer/") &&
              !id.includes("/@lezer/common/") &&
              !id.includes("/@lezer/highlight/") &&
              !id.includes("/@lezer/lr/"))
          ) {
            return undefined;
          }
          if (
            id.includes("/codemirror/") ||
            id.includes("/@codemirror/state/") ||
            id.includes("/@codemirror/view/") ||
            id.includes("/@codemirror/commands/") ||
            id.includes("/@codemirror/language/") ||
            id.includes("/@codemirror/search/") ||
            id.includes("/@codemirror/lint/") ||
            id.includes("/@codemirror/autocomplete/") ||
            id.includes("/@lezer/common/") ||
            id.includes("/@lezer/highlight/") ||
            id.includes("/@lezer/lr/")
          ) {
            return "editor-core";
          }
          return "vendor";
        },
      },
    },
  },
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      ignored: ["**/src-tauri/**", "**/.cargo-ci/**"],
    },
  },
});
