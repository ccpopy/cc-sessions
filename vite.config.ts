import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "node:path";

const host = process.env.TAURI_DEV_HOST;

function manualChunks(id: string): string | undefined {
  const normalized = id.replace(/\\/g, "/");
  if (!normalized.includes("/node_modules/")) return undefined;

  if (
    normalized.includes("/react/") ||
    normalized.includes("/react-dom/") ||
    normalized.includes("/scheduler/")
  ) {
    return "vendor-react";
  }

  if (
    normalized.includes("/react-router/") ||
    normalized.includes("/react-router-dom/")
  ) {
    return "vendor-router";
  }

  if (normalized.includes("/@tauri-apps/")) {
    return "vendor-tauri";
  }

  if (
    normalized.includes("/@radix-ui/") ||
    normalized.includes("/aria-hidden/") ||
    normalized.includes("/lucide-react/") ||
    normalized.includes("/class-variance-authority/") ||
    normalized.includes("/clsx/") ||
    normalized.includes("/react-remove-scroll") ||
    normalized.includes("/react-style-singleton/") ||
    normalized.includes("/use-callback-ref/") ||
    normalized.includes("/use-sidecar/") ||
    normalized.includes("/tailwind-merge/")
  ) {
    return "vendor-ui";
  }

  if (
    normalized.includes("/echarts/") ||
    normalized.includes("/zrender/")
  ) {
    return "vendor-charts";
  }

  if (
    normalized.includes("/react-markdown/") ||
    normalized.includes("/remark-") ||
    normalized.includes("/rehype-") ||
    normalized.includes("/mdast-") ||
    normalized.includes("/hast-") ||
    normalized.includes("/micromark") ||
    normalized.includes("/unified/") ||
    normalized.includes("/unist-") ||
    normalized.includes("/vfile") ||
    normalized.includes("/property-information/") ||
    normalized.includes("/space-separated-tokens/") ||
    normalized.includes("/comma-separated-tokens/") ||
    normalized.includes("/character-entities") ||
    normalized.includes("/decode-named-character-reference/") ||
    normalized.includes("/stringify-entities/") ||
    normalized.includes("/parse-entities/") ||
    normalized.includes("/html-url-attributes/") ||
    normalized.includes("/zwitch/") ||
    normalized.includes("/devlop/") ||
    normalized.includes("/bail/") ||
    normalized.includes("/trough/")
  ) {
    return "vendor-markdown";
  }

  if (normalized.includes("/react-json-view-lite/")) {
    return "vendor-json";
  }

  if (normalized.includes("/date-fns/")) {
    return "vendor-date";
  }

  if (
    normalized.includes("/zustand/") ||
    normalized.includes("/sonner/") ||
    normalized.includes("/zod/") ||
    normalized.includes("/@tanstack/")
  ) {
    return "vendor-app";
  }

  return undefined;
}

export default defineConfig(async () => ({
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? { protocol: "ws", host, port: 1421 }
      : undefined,
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: {
    rollupOptions: {
      output: {
        manualChunks,
        entryFileNames: "assets/[name]-[hash].js",
        chunkFileNames: "assets/[name]-[hash].js",
        assetFileNames: "assets/[name]-[hash][extname]",
      },
    },
  },
}));
