import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// During development the SPA proxies the API; in production it is embedded into
// the `mosaic` binary via rust-embed (see docs/web/management-app.md).
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/api": "http://localhost:8080",
    },
  },
  build: {
    outDir: "dist",
  },
});
